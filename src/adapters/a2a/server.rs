//! The A2A HTTP front door.
//!
//! Story 18.1a shipped a loopback-only, zero-authority discovery surface. Story
//! 18.1b turns it into the authority + network + execution surface: inbound
//! tasks are admitted by a decision core, executed as **local** peer nodes under
//! our authority, disclosed through one redaction boundary, and — off loopback —
//! reachable only behind TLS plus API-key authentication.
//!
//! # The shape of a request
//!
//! ```text
//! TCP/TLS ─▶ request deadline (30s, outermost)
//!         ─▶ auth middleware  (non-loopback: API key, pre-dispatch)
//!         ─▶ JSON-RPC dispatch
//!             ├─ message/send  ─▶ admit()  ─▶ register_peer ─▶ drive turn
//!             ├─ tasks/get     ─▶ submitter-scoped lookup ─▶ projection
//!             ├─ tasks/cancel  ─▶ submitter-scoped lookup ─▶ CancellationToken
//!             ├─ x-rustain-items/list    ─▶ principal-scoped read (19.16b)
//!             └─ x-rustain-items/retract ─▶ admit_item_retract ─▶ principal-
//!                                           scoped mark, journal first (19.16d)
//! ```
//!
//! # JSON-RPC 2.0 profile (AC6b / R8) — declared, narrow, enforced
//!
//! Rather than diverge silently, this server implements a **stated** subset and
//! rejects the rest with a message that names the profile:
//!
//! * **Notifications** (a request object with **no** `id` member) are honoured:
//!   the method runs and the server answers `204 No Content` with no body, as
//!   the spec requires. An absent `id` and an explicit `"id": null` are
//!   distinguished — the previous cut collapsed both onto `Value::Null`.
//!   ⚠ **One method refuses the notification form** (Story 19.16d `P3`):
//!   `x-rustain-items/retract` is a write whose collapse disclosure rides the
//!   response, so a notification of it answers `-32600` and writes nothing —
//!   a write whose disclosure rides the response must not be answerable with
//!   an empty 204.
//! * **An explicit `"id": null`** is rejected with `-32600`. It is legal but
//!   discouraged, and accepting it would make a notification and a null-id call
//!   indistinguishable in every response we emit.
//! * **Batch arrays** are rejected with a single `-32600` naming the profile.
//! * **A2A-Version** (spec §3.6) selects this interface's A2A dialect:
//!   the first header value when non-empty after trimming, else the first
//!   matching query pair, else [`A2A_PROTOCOL_VERSION`]. Major.Minor must match;
//!   header bytes outside visible ASCII are unsupported. A valid patch is
//!   ignored. Unsupported or malformed versions answer `-32009` over HTTP 200
//!   before dispatch, even for notifications (`id: null`).
//!
//! These rules are pinned by the envelope unit tests and the real-listener
//! `tests/conformance_19_16e_a2a_version_boundary.rs` behavioural tests.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Extension, RawQuery, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, oneshot};
use tokio_util::sync::CancellationToken;

use crate::adapters::policy::recipient_item::ThreadTurn;
use crate::adapters::rap::{AgentSigner, IdentityKeyStore};
use crate::domain::models::provenance::{Authorship, SendAuthorization};
use crate::domain::models::{
    A2aPeerSpec, AppConfig, CapabilityRegistry, ItemPrincipal, ItemScope, PeerId, PolicyPrincipal,
    RapTaskState, ReceivedStories, RecipientItemAllocator, RetractOutcome, RoomEvent,
    SemanticMessageType,
};
use crate::domain::ports::{InboundApprovalTicket, InboundPeerRuntime, InboundPeerTask};

use super::admission::{
    A2aAdmissionPolicy, AdmissionRequest, AdmissionVerdict, CapabilityGatedVerb, CapabilityVerdict,
    SubmitterTrust, THREAD_ACCESS_OFF, ThreadReads, admit, admit_item_retract, post_lookup,
    pre_lookup,
};
use super::auth::{
    A2aServerAuth, A2aServerSecurity, API_KEY_HEADER, AuthOutcome, BindDecision, BindEvidence,
    evaluate_bind_safety,
};
use super::card_cache::SignedCardCache;
use super::client::is_json_content_type;
use super::exec::{
    CANCEL_DETAIL, FAILURE_DETAIL, INBOUND_SUBAGENT_TYPE, InboundTaskStore, PendingAuth,
    RESTART_DETAIL, START_FAILURE_DETAIL, SubmitterKey, UNRECORDED_ACCEPT_DETAIL,
    mint_inbound_node_id, parse_inbound_node_id, project_node_to_rap,
};
use super::item_capability::{
    PresentedCapability, capability_matches, parse_presented_capability, presented_matches,
};
use super::jsonrpc::{
    A2A_PROTOCOL_VERSION, A2A_VERSION_HEADER, CODE_INTERNAL_ERROR, CODE_INVALID_PARAMS,
    CODE_INVALID_REQUEST, CODE_ITEM_REMOVED, CODE_METHOD_NOT_FOUND, CODE_PARSE_ERROR,
    CODE_REFUSED_BY_POLICY, CODE_TASK_NOT_FOUND, CODE_VERSION_NOT_SUPPORTED, JsonRpcErrorResponse,
    JsonRpcResponse,
};
use super::projection::{RemotePeerViewer, RoomProjection};
use super::story_carrier::parse_story_carrier;
use super::transparency::{InboundOutcome, TransparencySink};
use crate::adapters::policy::JournalRecipientItemProjection;

const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CANCEL_WAIT_TIMEOUT: Duration = Duration::from_secs(25);
const MAX_TASK_ID_BYTES: usize = 256;
const PENDING_TASK_FILE: &str = "a2a-pending.json";

/// The one message a non-owner or unknown task id ever receives.
const TASK_NOT_FOUND: &str = "Task not found";

/// Story 19.16d `AC1(d)` — the one message a retract naming another
/// principal's item, or an id that never existed, ever receives. One
/// constant, emitted from one `JsonRpcErrorResponse::new` site with
/// `CODE_TASK_NOT_FOUND`, so the two are byte-identical by construction
/// (`ADR-17-4a-01` R21). ⛔ `TASK_NOT_FOUND`'s noun is wrong for an item.
const ITEM_NOT_FOUND: &str = "Item not found";

/// Spec §3.6: header first, then the first form-decoded query match, else
/// the interface's default. A header outside visible ASCII (RFC 9110 OWS is
/// SP/HTAB only) is unsupported and never falls through.
fn a2a_version_supported(header: Option<&HeaderValue>, query: Option<&str>) -> bool {
    if let Some(header) = header {
        let Ok(value) = header.to_str() else {
            return false;
        };
        if !value.trim().is_empty() {
            return a2a_version_pair(value.trim()) == a2a_version_pair(A2A_PROTOCOL_VERSION);
        }
    }
    if let Some((_, value)) = query.and_then(|query| {
        url::form_urlencoded::parse(query.as_bytes())
            .find(|(key, _)| key.eq_ignore_ascii_case(A2A_VERSION_HEADER))
    }) {
        let value = value.trim();
        if !value.is_empty() {
            return a2a_version_pair(value) == a2a_version_pair(A2A_PROTOCOL_VERSION);
        }
    }
    true
}

/// Major.Minor membership; patch is digit-checked, never parsed (spec §3.6).
fn a2a_version_pair(value: &str) -> Option<(u32, u32)> {
    fn digits(value: &str) -> bool {
        !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
    }

    let mut parts = value.split('.');
    let major = parts.next()?;
    let minor = parts.next()?;
    if !digits(major) || !digits(minor) {
        return None;
    }
    if parts.next().is_some_and(|patch| !digits(patch)) || parts.next().is_some() {
        return None;
    }
    Some((major.parse().ok()?, minor.parse().ok()?))
}

fn version_not_supported(id: serde_json::Value) -> JsonRpcErrorResponse {
    let message = format!(
        "A2A protocol version not supported: this agent serves A2A {A2A_PROTOCOL_VERSION} on \
         this interface; send `A2A-Version: {A2A_PROTOCOL_VERSION}` or omit it"
    );
    JsonRpcErrorResponse::new(id, CODE_VERSION_NOT_SUPPORTED, message)
}

/// Story 19.16d `Q3` — the caller's own item is a tombstone. ⛔ Not
/// `ITEM_NOT_FOUND`: ownership is already proven, and a tombstone must be
/// distinguishable from not-found (`AD-1822`). ⛔ Never "deleted".
const ITEM_REMOVED: &str = "Item already removed by its recipient; nothing was marked";

/// Story 19.16d `P3` — why a retract notification is refused.
const RETRACT_NOTIFICATION_REFUSED: &str = "x-rustain-items/retract is not accepted as a \
     notification by this server's profile; send it with a string or number id so its result \
     can be returned";

use crate::domain::services::recipient_thread_bounds::{MAX_THREAD_TURN_BYTES, MAX_THREAD_TURNS};

pub use super::auth::BindDecision as ServerBindDecision;

/// Everything a served request needs. Cheap to clone (all `Arc`).
#[derive(Clone)]
struct ServerState {
    registry: Arc<CapabilityRegistry>,
    signer: Arc<AgentSigner>,
    endpoint_url: Arc<str>,
    security: A2aServerSecurity,
    /// `None` on a loopback-only, discovery-only deployment. Admission answers a
    /// policy verdict rather than pretending the capability exists.
    runtime: Option<Arc<dyn InboundPeerRuntime>>,
    tasks: Arc<InboundTaskStore>,
    cards: Arc<SignedCardCache>,
    transparency: Arc<TransparencySink>,
    recipient_items: Arc<JournalRecipientItemProjection>,
    /// Highest journal `seq` already folded into `recipient_items`.
    ///
    /// Story 19.16b: the served read must see acts this host performed on its
    /// **other** rail. `recipient_items` is applied in place only by the
    /// inbound ingress (`message/send`); a `/team ack` or `/team remove`
    /// travels the daemon socket and reaches the durable journal, never this
    /// projection — so without a refresh the cross-host read would answer
    /// `received` for an item the operator acknowledged an hour ago, until the
    /// host restarted. Mirrors the shipped `ConsentGate::refresh_projection`
    /// watermark: re-fold only when the journal actually grew.
    recipient_items_seq: Arc<std::sync::atomic::AtomicU64>,
    /// Serializes the recipient-item fold's two writer shapes (19.16b review):
    /// the ingress's durable-append + in-place `apply` pair, and the read-side
    /// refresh's load + `replace_from` + watermark store. Without this, a
    /// refresh snapshot loaded before an ingress append can land after the
    /// ingress's `apply` and erase the just-accepted item, and two interleaved
    /// refreshes can leave the watermark AHEAD of the fold — a stall that does
    /// not self-heal until the journal grows again. `tokio::sync` because both
    /// critical sections span journal I/O (`.await`) — the std-sync exception
    /// pattern does not apply across an await point.
    recipient_items_fold: Arc<tokio::sync::Mutex<()>>,
    item_allocator: Arc<RecipientItemAllocator>,
    policy: A2aAdmissionPolicy,
    workspace: Arc<PathBuf>,
    /// Host-sensitive fragments supplied by the runtime and used only as
    /// disclosure scrub needles. They are never included in a response.
    scrub_fragments: Vec<String>,
    /// Serializes read-modify-write updates to the small pending-task journal.
    pending_tasks: Arc<Mutex<()>>,
    /// Whether requests arriving here already cleared a credential gate.
    requires_credential: bool,
}

/// Composition inputs for [`serve`]. A struct rather than eleven positional
/// parameters: the two that matter most (`security`, `runtime`) are exactly the
/// ones a positional call site would transpose.
pub struct ServeConfig {
    pub registry: Arc<CapabilityRegistry>,
    pub signer: AgentSigner,
    pub security: A2aServerSecurity,
    pub runtime: Option<Arc<dyn InboundPeerRuntime>>,
    pub transparency: Arc<TransparencySink>,
    pub policy: A2aAdmissionPolicy,
    pub workspace: PathBuf,
    /// Public host and port to publish in the AgentCard. A wildcard bind must
    /// provide this because `0.0.0.0`/`::` are not client-reachable authorities.
    pub advertised_host: Option<String>,
    /// Supplied by the caller rather than minted inside `serve` so a test can
    /// read the cache's own signing counter — the AC7b keystone is "how many
    /// signatures did THIS server perform", which is unanswerable if the cache
    /// is private to a spawned task.
    pub cards: Arc<SignedCardCache>,
}

impl ServeConfig {
    /// A discovery-only, loopback-only configuration — the Story 18.1a posture.
    #[must_use]
    pub fn discovery_only(
        registry: Arc<CapabilityRegistry>,
        signer: AgentSigner,
        workspace: PathBuf,
    ) -> Self {
        Self {
            registry,
            signer,
            security: A2aServerSecurity::default(),
            runtime: None,
            transparency: Arc::new(TransparencySink::inert()),
            policy: A2aAdmissionPolicy::Deny,
            workspace,
            advertised_host: None,
            cards: Arc::new(SignedCardCache::new()),
        }
    }
}

/// Serve an already-bound listener. Tests enter through this production
/// spawn-and-serve core so they exercise real HTTP framing without owning
/// process-global Ctrl-C handling.
///
/// # The last line of defence (AC3b / R3)
///
/// [`evaluate_bind_safety`] is an effect-free decision over a *string*. This is
/// the only place that can see the address the kernel actually gave us, so it is
/// where the non-loopback invariant is enforced — against hostname resolution,
/// and against any caller that hands us a listener it bound itself, bypassing
/// `run` entirely.
///
/// Story 18.1b **conditions** that check on TLS + authentication evidence; it
/// does not delete it. Deleting it would remove the defence 18.1a's review
/// installed; leaving it unconditional would make non-loopback serving
/// impossible no matter what the decision core said.
pub async fn serve(
    listener: tokio::net::TcpListener,
    config: ServeConfig,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let bound = listener.local_addr()?;
    let loopback = bound.ip().to_canonical().is_loopback();
    anyhow::ensure!(
        loopback || config.security.is_network_ready(),
        "refusing to serve A2A on non-loopback address {bound}: non-loopback serving requires \
         TLS and API-key authentication together (configure `server.tls` and `server.api_key_env` \
         in .rustain/a2a.json)"
    );

    let advertised_host = config
        .advertised_host
        .as_deref()
        .map(str::trim)
        .filter(|host| !host.is_empty());
    anyhow::ensure!(
        !bound.ip().is_unspecified() || advertised_host.is_some(),
        "refusing to serve A2A on wildcard address {bound} without `server.advertised_host`; \
         configure a client-reachable advertised_host in .rustain/a2a.json"
    );

    let scheme = if config.security.tls.is_some() {
        "https"
    } else {
        "http"
    };
    let endpoint_url: Arc<str> = match advertised_host {
        Some(host) => format!("{scheme}://{host}").into(),
        None => format!("{scheme}://{bound}").into(),
    };
    let scrub_fragments = match config.runtime.as_ref() {
        Some(runtime) => runtime.disclosure_forbidden_fragments().await,
        None => Vec::new(),
    };
    let recipient_entries = config.transparency.load_entries().await?;
    let recipient_items = Arc::new(JournalRecipientItemProjection::from_entries(
        &recipient_entries,
    ));
    let recipient_items_seq = Arc::new(std::sync::atomic::AtomicU64::new(
        recipient_entries.last().map_or(0, |entry| entry.seq),
    ));
    let item_allocator = Arc::new(RecipientItemAllocator::from_addresses(
        recipient_items.snapshot().items.keys().cloned(),
    ));
    // Story 19.18d `A12`/`P10`/`P23` — "startup says so": the same startup fold
    // classifies every projected item no caller of this listener can reach.
    let orphaned =
        orphaned_recipient_items(&recipient_items, config.security.auth.as_ref(), loopback);
    if orphaned > 0 {
        tracing::warn!(
            "{}",
            ORPHANED_RECIPIENT_ITEMS_WARNING.replace("{count}", &orphaned.to_string())
        );
    }
    let state = ServerState {
        registry: config.registry,
        signer: Arc::new(config.signer),
        endpoint_url,
        runtime: config.runtime,
        tasks: Arc::new(InboundTaskStore::default()),
        cards: config.cards,
        transparency: config.transparency,
        recipient_items,
        recipient_items_seq,
        recipient_items_fold: Arc::new(tokio::sync::Mutex::new(())),
        item_allocator,
        policy: config.policy,
        workspace: Arc::new(config.workspace),
        scrub_fragments,
        pending_tasks: Arc::new(Mutex::new(())),
        // Loopback stays plaintext and unauthenticated (18.1a). Off loopback the
        // credential is mandatory, and `serve` has already refused to run
        // without one configured.
        requires_credential: !loopback,
        security: config.security,
    };

    // AC5b — a task the previous process was running must not read as a zombie
    // `working`. Reconcile BEFORE the router accepts anything, so the very first
    // `tasks/get` after a restart already tells the truth.
    //
    // Story 19.18c `P16` — the pending file is read ONCE, before the orphan
    // pass: under consent a pending task already has a durable node, so the
    // orphan pass inserts it first and must take its capability hash from
    // the pending record, or the reseeded task would turn legacy.
    let pending = read_pending_records(&state).await;
    reconcile_after_restart(&state, pending.as_deref().unwrap_or_default()).await;
    reconcile_pending_after_restart(&state, pending).await;

    let tls = state.security.tls.clone();
    let router = build_router(state);

    match tls {
        Some(material) => {
            let listener = super::tls::TlsListener::new(listener, &material)?;
            axum::serve(listener, router)
                .with_graceful_shutdown(cancel.cancelled_owned())
                .await?;
        }
        None => {
            axum::serve(listener, router)
                .with_graceful_shutdown(cancel.cancelled_owned())
                .await?;
        }
    }
    Ok(())
}

/// Fail every inbound task a previous process left in flight, and seed the task
/// map so its submitter gets `failed` + a reason instead of `Task not found`.
///
/// Durable host-side *resumption* is deferred (`DF-18-1-HOSTRECONCILE`); the
/// defer must not become a place tasks disappear.
///
/// Story 19.18c `A6`/`P16` — the reseeded task keeps its capability hash: from
/// the durable item sharing `(principal, task)` (Removed included; the class
/// of a task id is fixed, `A22`), else from the pending record keyed by
/// `(submitter, task)` — ⛔ never by task alone, which would lend one
/// principal's hash to another principal's orphan with the same id.
async fn reconcile_after_restart(state: &ServerState, pending: &[PendingTaskRecord]) {
    let Some(runtime) = state.runtime.as_ref() else {
        return;
    };
    for node_id in runtime
        .reconcile_orphaned_tasks(INBOUND_SUBAGENT_TYPE)
        .await
    {
        let Some((submitter, task_id)) = parse_inbound_node_id(&node_id) else {
            // Story 19.18d `P4` — a node id whose submitter handle is not one
            // of the three canonical forms restores nothing under any
            // fallback principal.
            tracing::warn!(node = %node_id, "skipping malformed A2A inbound task node");
            continue;
        };
        let policy_principal = submitter.policy_principal();
        // Story 19.18d `F5` — the item principal comes from the handle, never
        // from the policy principal's display peer.
        let principal = submitter.principal();
        let capability_class = state
            .recipient_items
            .task_capability_classes(&principal, &task_id)
            .pop()
            .or_else(|| {
                pending
                    .iter()
                    .find(|record| {
                        record.task_id == task_id && record.submitter == submitter.as_str()
                    })
                    .map(|record| record.capability_hash.clone())
            });
        let Some(capability_hash) = capability_class else {
            // No received item and no pending record: absence is not proof of
            // a legacy task. A failed pending-file write must not turn a
            // capability-bearing orphan into a co-holder-readable task.
            tracing::warn!(task = %task_id, node = %node_id, "A2A orphan task capability class unknown; not restoring task");
            continue;
        };
        let Some(task) = state
            .tasks
            .insert(
                task_id.clone(),
                node_id.clone(),
                submitter,
                RapTaskState::Submitted,
                capability_hash,
            )
            .await
        else {
            continue;
        };
        if state
            .transparency
            .has_recorded_status_query(&policy_principal, &task_id)
            .await
        {
            task.restore_status_query();
        }
        if let Some(item) = state.recipient_items.find_by_task(&principal, &task_id) {
            task.set_item_address(item.address).await;
        }
        task.advance(RapTaskState::Working).await;
        task.set_detail(RESTART_DETAIL).await;
        task.advance(RapTaskState::Failed).await;
        tracing::warn!(task = %task_id, node = %node_id, "A2A inbound task failed by host restart");
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PendingTaskRecord {
    task_id: String,
    submitter: String,
    /// Story 19.18c `A6` — the task's capability **hash** (⛔ never the
    /// value), so a restart reseeds the pending task as capability-bearing.
    /// Absent on a pre-19.18c record, which stays legacy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    capability_hash: Option<String>,
}

fn pending_tasks_path(state: &ServerState) -> PathBuf {
    state.workspace.join(".rustain").join(PENDING_TASK_FILE)
}

/// Best-effort persistence for a task that has crossed into `auth-required`.
///
/// A mutex makes concurrent requests update one JSON list rather than losing a
/// sibling record to competing read-modify-write cycles. The file is only an
/// honest-restart aid: a write failure is logged but never blocks or changes an
/// admission decision already made in memory.
async fn append_pending_task(state: &ServerState, record: PendingTaskRecord) {
    let _guard = state.pending_tasks.lock().await;
    let path = pending_tasks_path(state);
    let Some(parent) = path.parent() else {
        tracing::error!(path = %path.display(), "A2A pending task path has no parent");
        return;
    };
    if let Err(error) = tokio::fs::create_dir_all(parent).await {
        tracing::error!(%error, path = %path.display(), "failed to create A2A pending task directory");
        return;
    }
    let mut records = match tokio::fs::read(&path).await {
        Ok(raw) => match serde_json::from_slice::<Vec<PendingTaskRecord>>(&raw) {
            Ok(records) => records,
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "cannot append to corrupt A2A pending task file");
                return;
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            tracing::error!(%error, path = %path.display(), "failed to read A2A pending task file");
            return;
        }
    };
    if !records.iter().any(|existing| {
        existing.task_id == record.task_id && existing.submitter == record.submitter
    }) {
        records.push(record);
    }
    let encoded = match serde_json::to_vec(&records) {
        Ok(encoded) => encoded,
        Err(error) => {
            tracing::error!(%error, "failed to encode A2A pending task file");
            return;
        }
    };
    if let Err(error) = tokio::fs::write(&path, encoded).await {
        tracing::error!(%error, path = %path.display(), "failed to persist A2A pending task");
    }
}

/// Best-effort removal after the pending task reaches any terminal state.
async fn remove_pending_task(state: &ServerState, record: &PendingTaskRecord) {
    let _guard = state.pending_tasks.lock().await;
    let path = pending_tasks_path(state);
    let mut records = match tokio::fs::read(&path).await {
        Ok(raw) => match serde_json::from_slice::<Vec<PendingTaskRecord>>(&raw) {
            Ok(records) => records,
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "cannot remove from corrupt A2A pending task file");
                return;
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            tracing::error!(%error, path = %path.display(), "failed to read A2A pending task file");
            return;
        }
    };
    let before = records.len();
    records.retain(|existing| {
        existing.task_id != record.task_id || existing.submitter != record.submitter
    });
    if records.len() == before {
        return;
    }
    let encoded = match serde_json::to_vec(&records) {
        Ok(encoded) => encoded,
        Err(error) => {
            tracing::error!(%error, "failed to encode A2A pending task file");
            return;
        }
    };
    if let Err(error) = tokio::fs::write(&path, encoded).await {
        tracing::error!(%error, path = %path.display(), "failed to remove A2A pending task");
    }
}

/// Read `a2a-pending.json` once at startup (Story 19.18c `P16`). `None` when
/// it is missing, corrupt or unreadable — each warned, and then nothing is
/// restored or truncated, exactly as before the read was split out.
async fn read_pending_records(state: &ServerState) -> Option<Vec<PendingTaskRecord>> {
    let path = pending_tasks_path(state);
    match tokio::fs::read(&path).await {
        Ok(raw) => match serde_json::from_slice::<Vec<PendingTaskRecord>>(&raw) {
            Ok(records) => Some(records),
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "skipping corrupt A2A pending task file");
                None
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(path = %path.display(), "A2A pending task file is missing; no approval-pending tasks restored");
            None
        }
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "skipping unreadable A2A pending task file");
            None
        }
    }
}

/// Rehydrate approval-pending tasks after their owning process disappeared.
/// A record whose task the orphan pass already reseeded is skipped here; that
/// pass took the record's capability hash (`P16`).
async fn reconcile_pending_after_restart(
    state: &ServerState,
    records: Option<Vec<PendingTaskRecord>>,
) {
    let Some(records) = records else {
        return;
    };
    let path = pending_tasks_path(state);
    for record in &records {
        // Story 19.18d `P4` — a handle that is not one of the three canonical
        // forms is malformed like an empty one: one warning, nothing restored.
        let submitter = SubmitterKey::from_encoded(record.submitter.clone());
        let Some(submitter) = submitter
            .filter(|_| !record.task_id.is_empty() && record.task_id.len() <= MAX_TASK_ID_BYTES)
        else {
            tracing::warn!(path = %path.display(), "skipping malformed A2A pending task record");
            continue;
        };
        let policy_principal = submitter.policy_principal();
        let Some(task) = state
            .tasks
            .insert(
                record.task_id.clone(),
                mint_inbound_node_id(&submitter, &record.task_id),
                submitter,
                RapTaskState::Submitted,
                record.capability_hash.clone(),
            )
            .await
        else {
            continue;
        };
        if state
            .transparency
            .has_recorded_status_query(&policy_principal, &record.task_id)
            .await
        {
            task.restore_status_query();
        }
        task.advance(RapTaskState::Working).await;
        task.set_detail(RESTART_DETAIL).await;
        task.advance(RapTaskState::Failed).await;
        tracing::warn!(task = %record.task_id, "A2A approval-pending task failed by host restart");
    }

    if let Err(error) = tokio::fs::write(&path, b"[]").await {
        tracing::warn!(%error, path = %path.display(), "failed to truncate reconciled A2A pending task file");
    }
}

fn build_router(state: ServerState) -> Router {
    let auth_state = state.clone();
    Router::new()
        .route("/.well-known/agent-card.json", get(serve_agent_card))
        .route(
            "/",
            post(handle_jsonrpc).route_layer(axum::middleware::from_fn_with_state(
                auth_state,
                authenticate_request,
            )),
        )
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        // Outermost: `Router::layer` wraps what is already there, so this
        // deadline covers body streaming and extractor work. A timeout applied
        // inside the handler starts after `Bytes` has been collected and is
        // therefore useless against a slow-body client.
        .layer(axum::middleware::from_fn(enforce_request_deadline))
        .with_state(state)
}

async fn enforce_request_deadline(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    match tokio::time::timeout(REQUEST_TIMEOUT, next.run(request)).await {
        Ok(response) => response,
        Err(_) => json_response(
            &JsonRpcErrorResponse::new(
                serde_json::Value::Null,
                CODE_INTERNAL_ERROR,
                "A2A request timed out",
            ),
            StatusCode::REQUEST_TIMEOUT,
        ),
    }
}

/// The AgentCard is served **unauthenticated, by design** (AC7b / D1).
///
/// Key-gating it would gate the document that explains how to get past the gate
/// — and empirically 53 of 141 live cards publish their `securitySchemes` in the
/// public card. What protects this endpoint instead is that it is *cheap*: the
/// signed bytes are cached per catalogue generation.
async fn serve_agent_card(State(state): State<ServerState>) -> Response {
    match state
        .cards
        .signed(
            &state.registry,
            &state.signer,
            &state.endpoint_url,
            state
                .security
                .auth
                .as_ref()
                .filter(|_| state.requires_credential),
        )
        .await
    {
        Ok(raw) => (
            StatusCode::OK,
            [(CONTENT_TYPE, "application/json")],
            raw.to_string(),
        )
            .into_response(),
        Err(error) => {
            tracing::error!(%error, "failed to sign served AgentCard");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Who is calling, having already cleared the credential gate.
#[derive(Clone)]
struct Caller {
    key: SubmitterKey,
    trust: SubmitterTrust,
    /// Story 19.18d `A7`/`P2` — the roster alias of the bound slot this
    /// caller's key matched (display attribution, never identity). `None` for
    /// loopback and unbound keys; ⛔ never synthesized from the pin.
    ingress_alias: Option<String>,
}

/// Authenticate one request. Runs **before** dispatch, so a rejected caller
/// never reaches admission, `register_peer`, or the journal.
/// The error side is boxed: `axum::Response` is a large value, and returning
/// it inline would make every `Ok` path pay for the rejection's footprint.
fn authenticate(state: &ServerState, headers: &HeaderMap) -> Result<Caller, Box<Response>> {
    if !state.requires_credential {
        return Ok(Caller {
            key: SubmitterKey::loopback(),
            trust: SubmitterTrust::Loopback,
            ingress_alias: None,
        });
    }
    let Some(auth) = state.security.auth.as_ref() else {
        // Unreachable: `serve` refuses a non-loopback bind without auth. Answer
        // rather than panic — a reachable panic on the request path would be a
        // remote denial of service.
        return Err(unauthorized("A2A server is misconfigured"));
    };
    let presented = headers
        .get(API_KEY_HEADER)
        .and_then(|value| value.to_str().ok());
    let has_bearer = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("bearer ")
        });

    match auth.verify(presented, has_bearer) {
        // Story 19.18d `A7` — the selected slot decides: a bound slot makes the
        // caller its pinned peer, an unbound one keeps the key's pseudonym.
        AuthOutcome::Authenticated { slot } => Ok(match auth.binding(slot) {
            Some(binding) => Caller {
                key: SubmitterKey::bound(binding.peer()),
                trust: SubmitterTrust::BoundApiKey,
                ingress_alias: Some(binding.alias().to_owned()),
            },
            None => Caller {
                key: SubmitterKey::from_api_key(presented.unwrap_or_default()),
                trust: SubmitterTrust::ApiKey,
                ingress_alias: None,
            },
        }),
        AuthOutcome::NoCredential => Err(unauthorized(
            "missing API key: present it in the `x-api-key` header (an Authorization: Bearer \
             token is not an accepted scheme — see the served AgentCard's securitySchemes)",
        )),
        AuthOutcome::Rejected => Err(unauthorized("invalid API key")),
    }
}

fn unauthorized(message: &str) -> Box<Response> {
    Box::new(json_response(
        &JsonRpcErrorResponse::new(serde_json::Value::Null, CODE_INVALID_REQUEST, message),
        StatusCode::UNAUTHORIZED,
    ))
}

/// Authenticate and validate the request metadata before the body extractor can
/// allocate or parse anything. This layer applies only to POST `/`; the public
/// AgentCard route intentionally remains outside it.
async fn authenticate_request(
    State(state): State<ServerState>,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let caller = match authenticate(&state, request.headers()) {
        Ok(caller) => caller,
        Err(response) => return *response,
    };
    let content_type = request
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    if !content_type.is_some_and(is_json_content_type) {
        return json_response(
            &JsonRpcErrorResponse::new(
                serde_json::Value::Null,
                CODE_INVALID_REQUEST,
                "Content-Type must be application/json or application/*+json",
            ),
            StatusCode::OK,
        );
    }
    request.extensions_mut().insert(caller);
    next.run(request).await
}

async fn handle_jsonrpc(
    State(state): State<ServerState>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Response {
    match parse_envelope(&body) {
        Err(response) => json_response(&response, StatusCode::OK),
        Ok(envelope)
            if !a2a_version_supported(headers.get(A2A_VERSION_HEADER), query.as_deref()) =>
        {
            let id = match envelope {
                Envelope::Call { id, .. } => id,
                Envelope::Notification { .. } => serde_json::Value::Null,
            };
            json_response(&version_not_supported(id), StatusCode::OK)
        }
        Ok(Envelope::Notification { method, params }) => {
            // Story 19.16d `P3` — the one method that refuses the notification
            // form. A retract is a write whose collapse disclosure
            // (`principalCollapsed`) rides the response; answered as a 204,
            // the disclosure would be undeliverable by the caller's choice.
            // ⛔ Refused BEFORE `dispatch`, so nothing is written.
            if method == super::ITEMS_RETRACT_METHOD {
                return json_response(
                    &JsonRpcErrorResponse::new(
                        serde_json::Value::Null,
                        CODE_INVALID_REQUEST,
                        RETRACT_NOTIFICATION_REFUSED,
                    ),
                    StatusCode::OK,
                );
            }
            // Spec: a notification receives no response. Run it for effect and
            // answer 204 — the previous cut returned an HTTP-200 `-32600`.
            let _ = dispatch(&state, &caller, &method, params, None).await;
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(Envelope::Call { id, method, params }) => {
            match dispatch(&state, &caller, &method, params, Some(id.clone())).await {
                Ok(result) => json_response(&JsonRpcResponse::new(id, result), StatusCode::OK),
                Err(error) => json_response(&error, StatusCode::OK),
            }
        }
    }
}

enum Envelope {
    Call {
        id: serde_json::Value,
        method: String,
        params: serde_json::Value,
    },
    Notification {
        method: String,
        params: serde_json::Value,
    },
}

/// Parse one request object against the declared narrow profile.
///
/// Works over `serde_json::Value` rather than a `Deserialize` struct precisely
/// so an **absent** `id` member and an explicit `"id": null` stay
/// distinguishable — a `#[serde(default)] id: Value` collapses both.
fn parse_envelope(body: &[u8]) -> Result<Envelope, JsonRpcErrorResponse> {
    let invalid = |message: &str| {
        JsonRpcErrorResponse::new(serde_json::Value::Null, CODE_INVALID_REQUEST, message)
    };

    let value: serde_json::Value = serde_json::from_slice(body).map_err(|_| {
        JsonRpcErrorResponse::new(serde_json::Value::Null, CODE_PARSE_ERROR, "Parse error")
    })?;

    if value.is_array() {
        return Err(invalid(
            "batch requests are not supported by this server's JSON-RPC profile; send one \
             request object per HTTP POST",
        ));
    }
    let Some(object) = value.as_object() else {
        return Err(invalid("Invalid Request"));
    };
    if object.get("jsonrpc").and_then(serde_json::Value::as_str) != Some("2.0") {
        return Err(invalid("Invalid Request"));
    }
    let method = object
        .get("method")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|method| !method.is_empty())
        .ok_or_else(|| invalid("Invalid Request"))?
        .to_owned();
    let params = object
        .get("params")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    match object.get("id") {
        None => Ok(Envelope::Notification { method, params }),
        Some(serde_json::Value::Null) => Err(invalid(
            "an explicit null JSON-RPC id is not accepted by this server's profile; omit `id` \
             for a notification or send a string or number id",
        )),
        Some(id @ (serde_json::Value::String(_) | serde_json::Value::Number(_))) => {
            Ok(Envelope::Call {
                id: id.clone(),
                method,
                params,
            })
        }
        Some(_) => Err(invalid("Invalid Request")),
    }
}

async fn dispatch(
    state: &ServerState,
    caller: &Caller,
    method: &str,
    params: serde_json::Value,
    id: Option<serde_json::Value>,
) -> Result<serde_json::Value, JsonRpcErrorResponse> {
    let notification = id.is_none();
    let echo = id.unwrap_or(serde_json::Value::Null);
    match method {
        "message/send" => message_send(state, caller, &params, echo, notification).await,
        "tasks/get" => tasks_get(state, caller, &params, echo).await,
        "tasks/cancel" => tasks_cancel(state, caller, &params, echo).await,
        // Story 19.16b AC1(a) — the fourth arm, additive and namespaced (`A20`,
        // NFR68). ⛔ `params` is deliberately unread: `AD-1826`'s decided half
        // says a receiver **ignores** unknown fields on a JSON-RPC verb's
        // payload, and the read takes no parameter at all — a `peerId`
        // parameter would be the cross-peer enumeration oracle
        // `ADR-17-4a-01` R21 forbids (AC1(c)).
        //
        // A notification carries no response, so the refresh — a whole-journal
        // read — would run for an answer nobody reads, and a credential-less
        // loopback caller could burn the fold for free (19.16b review). The
        // ADR R22 204 stands; nothing else changes.
        super::ITEMS_LIST_METHOD if notification => Ok(serde_json::Value::Null),
        super::ITEMS_LIST_METHOD => items_list(state, caller).await,
        // Story 19.16d AC1(a) — the fifth arm, a write. ⚠ A notification never
        // reaches it: `handle_jsonrpc` refuses that form first (`P3`). The
        // scope is derived HERE from the authenticated caller and handed
        // down; ⛔ nothing in `params` can name it (`AC1(c)`, R21).
        super::ITEMS_RETRACT_METHOD => {
            let scope = caller_scope(state, caller);
            items_retract(state, &scope, caller.trust, &params, echo).await
        }
        // Story 19.18 `AC4(a)` — the per-item thread read. A notification
        // carries no response, so it answers `Null` and reads nothing, like
        // the list (its refresh is a journal read nobody would receive).
        super::ITEMS_THREAD_METHOD if notification => Ok(serde_json::Value::Null),
        super::ITEMS_THREAD_METHOD => items_thread(state, caller, &params, echo).await,
        _ => Err(JsonRpcErrorResponse::new(
            echo,
            CODE_METHOD_NOT_FOUND,
            "Method not found",
        )),
    }
}

// ── message/send ────────────────────────────────────────────────────────────

async fn message_send(
    state: &ServerState,
    caller: &Caller,
    params: &serde_json::Value,
    echo: serde_json::Value,
    notification: bool,
) -> Result<serde_json::Value, JsonRpcErrorResponse> {
    // AC1(e) one-key-one-spelling: the receive side reads the carrier through
    // the shared const, so a one-side key respelling cannot compile silently.
    let message_type = SemanticMessageType::parse_optional(
        params
            .pointer(&format!(
                "/message/metadata/{}",
                super::MESSAGE_TYPE_METADATA_KEY
            ))
            .and_then(serde_json::Value::as_str),
    );
    let Some(message) = well_formed_message(params.get("message")) else {
        return Err(JsonRpcErrorResponse::new(
            echo,
            CODE_INVALID_PARAMS,
            "Invalid params",
        ));
    };

    let task_id = match params
        .pointer("/message/messageId")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
    {
        Some(message_id) => message_id.to_owned(),
        None if notification => format!("a2a-{}", nanoid::nanoid!()),
        None => format!("a2a-{}", jsonrpc_id_text(&echo)),
    };
    if task_id.len() > MAX_TASK_ID_BYTES {
        return Err(JsonRpcErrorResponse::new(
            echo,
            CODE_INVALID_PARAMS,
            "Invalid params",
        ));
    }
    // Story 19.18c `A7` step 1 / `P7` — both capability carriers parse HERE,
    // for every caller trust, before anything reads or writes: a malformed
    // value is `-32602` with zero rows, and `null` is absent. The raw value
    // is reduced to its digest at once and dropped (`A13`); ⛔ never logged.
    let carrier = |key: &str| params.pointer(&format!("/message/metadata/{key}"));
    let (Ok(item_capability), Ok(reply_capability)) = (
        parse_presented_capability(carrier(super::ITEM_CAPABILITY_METADATA_KEY)),
        parse_presented_capability(carrier(super::REPLY_CAPABILITY_METADATA_KEY)),
    ) else {
        return Err(JsonRpcErrorResponse::new(
            echo,
            CODE_INVALID_PARAMS,
            "Invalid params",
        ));
    };
    // Story 19.18d `F5`/`F6` — the refusal row shows the caller's display
    // peer (the pin for a bound key, the pseudonym otherwise); the class
    // check below runs on the caller's exact item principal, ⛔ never scope.
    let peer_id = caller.key.policy_principal().peer().clone();
    let reference =
        follow_up_reference(state, caller, params, &echo, reply_capability.as_ref()).await?;
    // Story 19.18c `A22` — one capability class per `(principal, task)`,
    // checked for every admission verdict before `admit`, so a refusal
    // writes zero rows. A co-holder that resubmits another sender's task id
    // after a restart or an eviction gets today's in-memory duplicate answer,
    // byte for byte, instead of an item the operator's answer would land on.
    if !task_class_admits(
        state,
        &caller.key.principal(),
        &task_id,
        item_capability.as_ref(),
    )
    .await
    {
        return Err(JsonRpcErrorResponse::new(
            echo,
            CODE_INTERNAL_ERROR,
            format!("task id {task_id} is already in flight"),
        ));
    }
    let facts = IngressFacts {
        authorship: Authorship::from_wire(
            params
                .get("message")
                .and_then(|message| message.get("metadata"))
                .and_then(|metadata| metadata.get(super::AUTHORSHIP_METADATA_KEY))
                .and_then(serde_json::Value::as_str)
                .unwrap_or(""),
        ),
        send_authorization: SendAuthorization::from_wire(
            params
                .get("message")
                .and_then(|message| message.get("metadata"))
                .and_then(|metadata| metadata.get(super::SEND_AUTHORIZATION_METADATA_KEY))
                .and_then(serde_json::Value::as_str)
                .unwrap_or(""),
        ),
        stories: parse_story_carrier(message_type, carrier(super::STORIES_METADATA_KEY)),
        in_reply_to: reference.as_ref().map(|reference| reference.id.clone()),
        capability_hash: item_capability
            .as_ref()
            .map(PresentedCapability::stored_hash),
        reply_capability_matched: reference.is_some_and(|reference| reference.matched),
        ingress_alias: caller.ingress_alias.clone(),
    };

    // ── Decision core. Effect-free: no lock, no node, no journal write. ──
    let verdict = admit(
        &AdmissionRequest {
            text: &message,
            executor_available: state.runtime.is_some(),
        },
        state.policy,
        caller.trust,
    );

    match verdict {
        AdmissionVerdict::Reject { reason } => {
            // NFR70: the refusal path performs ZERO core mutation — no
            // `register_peer`, no `set_state`. The transparency record below is
            // a durable room event, which AC2b *requires*; it is not core state.
            //
            // AC1: a journal failure must NOT convert this refusal into an
            // accept. The refusal still goes back to the peer; the operator
            // learns about the missing record through the sink's latch.
            let _ = state
                .transparency
                .record(InboundOutcome::Refused {
                    peer: peer_id,
                    task_id: task_id.clone(),
                    reason: reason.clone(),
                })
                .await;
            Ok(rejected_task_json(&task_id, &reason, &state.signer))
        }
        AdmissionVerdict::Accept => {
            start_task(state, caller, task_id, message, message_type, facts, false)
                .await
                .map_err(|reason| JsonRpcErrorResponse::new(echo, CODE_INTERNAL_ERROR, &reason))
        }
        AdmissionVerdict::AcceptPendingApproval => {
            start_task(state, caller, task_id, message, message_type, facts, true)
                .await
                .map_err(|reason| JsonRpcErrorResponse::new(echo, CODE_INTERNAL_ERROR, &reason))
        }
    }
}

/// Story 19.18c — what `message_send` validated before admission, carried to
/// `launch` as one value so the four signatures grow by one parameter.
#[derive(Clone, Debug)]
struct IngressFacts {
    /// Story 19.20 — sender-reported facts retained across deferred approval.
    authorship: Authorship,
    send_authorization: SendAuthorization,
    /// The bounded story statement, retained across deferred approval.
    stories: Option<ReceivedStories>,
    /// Story 19.18 `AC5(c)` — the resolved follow-up reference, stored raw.
    in_reply_to: Option<String>,
    /// `sha256` hex of the presented item capability; `None` is legacy.
    capability_hash: Option<String>,
    /// `A15` — the reply capability matched the reference's nearest item.
    reply_capability_matched: bool,
    /// Story 19.18d `F11` — the matched bound slot's roster alias, written into
    /// `RecipientItemReceived.alias` by `launch` on the direct and the
    /// approval path alike. `None` for loopback and unbound keys.
    ingress_alias: Option<String>,
}

/// A follow-up reference that resolved inside the caller's scope.
struct FollowUpReference {
    id: String,
    /// The presented reply capability matched the reference's nearest item.
    matched: bool,
}

/// Story 19.18e `A3` — the submitter handles this caller adopts: for a bound
/// caller, the key handle of every slot bound to the same pin with
/// `adoptPreBinding` (`ApiKeySet::adopted_for`); empty for loopback, an unbound
/// key, or a listener without keys. The one source of both the item scope
/// ([`caller_scope`]) and the adopted task lookup ([`scoped_task`]).
fn adopted_submitters(state: &ServerState, caller: &Caller) -> Vec<SubmitterKey> {
    match (caller.key.principal(), state.security.auth.as_ref()) {
        (ItemPrincipal::A2aBound(pin), Some(auth)) => auth.keys().adopted_for(&pin),
        _ => Vec::new(),
    }
}

/// Story 19.18d `A11` — the item principals this caller may read: its own,
/// and for a bound caller also the pseudonyms of every key bound to the same
/// pin with `adoptPreBinding` ([`adopted_submitters`]). Every other caller's
/// scope is exact.
fn caller_scope(state: &ServerState, caller: &Caller) -> ItemScope {
    ItemScope::with_adopted(
        caller.key.principal(),
        adopted_submitters(state, caller)
            .iter()
            .map(SubmitterKey::principal),
    )
}

/// Story 19.18c `A22` — whether a `message/send` for `task_id` keeps one
/// capability class per `(principal, task)`: every existing item sharing it,
/// Removed included, must carry the presented class and hash (`None` is a
/// class). Lookup first, refresh on a miss; a refresh that fails refuses
/// rather than decide on a stale fold. Cost: one O(items) scan per send
/// (`DF-19-16B-ACK-READ-REFOLDS-THE-WHOLE-JOURNAL`). Story 19.18d: the
/// caller's exact item principal, ⛔ never its adoption scope.
async fn task_class_admits(
    state: &ServerState,
    principal: &ItemPrincipal,
    task_id: &str,
    presented: Option<&PresentedCapability>,
) -> bool {
    let mut classes = state
        .recipient_items
        .task_capability_classes(principal, task_id);
    if classes.is_empty() {
        if !refresh_recipient_items(state).await {
            return false;
        }
        classes = state
            .recipient_items
            .task_capability_classes(principal, task_id);
    }
    classes
        .iter()
        .all(|class| match (class.as_deref(), presented) {
            (None, None) => true,
            (Some(stored), Some(presented)) => capability_matches(stored, presented),
            (None, Some(_)) | (Some(_), None) => false,
        })
}

/// Story 19.18 `AC5(a)(b)` — read and validate the follow-up carrier
/// ([`super::REPLY_TO_METADATA_KEY`]) **before** admission's side effects.
///
/// - Absent (or `null`) ⇒ `None`: an ordinary root item, unchanged.
/// - Not a non-empty string within `MAX_PEER_ID_BYTES` ⇒ `-32602`, nothing
///   written (the invalid-params precedent).
/// - Present ⇒ Story 19.18c's `A7` order: an API-key caller that presents
///   **no** reply capability meets 19.18's pre-lookup gate unchanged
///   (`-32040` under `off`); otherwise the reference must resolve inside the
///   CALLER's principal (lookup first, refresh only on a miss), and its
///   nearest item decides (`post_lookup`): a capability-bearing item needs
///   the matching reply capability, a legacy one keeps the `threadReads`
///   gate. A miss is ONE error — `CODE_TASK_NOT_FOUND` with `ITEM_NOT_FOUND`
///   — for not-yours, never-existed and wrong-capability alike (R21), and ⛔
///   mints nothing.
///
/// `reply_capability` was parsed by `message_send` (`P7`): this function
/// returns before reading anything when there is no reference, so a
/// malformed carrier parsed here would be silently ignored.
async fn follow_up_reference(
    state: &ServerState,
    caller: &Caller,
    params: &serde_json::Value,
    echo: &serde_json::Value,
    reply_capability: Option<&PresentedCapability>,
) -> Result<Option<FollowUpReference>, JsonRpcErrorResponse> {
    let reference = match params.pointer(&format!(
        "/message/metadata/{}",
        super::REPLY_TO_METADATA_KEY
    )) {
        None | Some(serde_json::Value::Null) => return Ok(None),
        Some(value) => value
            .as_str()
            .filter(|reference| {
                !reference.is_empty()
                    && reference.len() <= crate::domain::services::transparency::MAX_PEER_ID_BYTES
            })
            .ok_or_else(|| {
                JsonRpcErrorResponse::new(echo.clone(), CODE_INVALID_PARAMS, "Invalid params")
            })?,
    };
    let setting = state.security.thread_reads;
    let refused =
        |reason: &str| JsonRpcErrorResponse::new(echo.clone(), CODE_REFUSED_BY_POLICY, reason);
    pre_lookup(
        caller.trust,
        CapabilityGatedVerb::ThreadOrFollowUp,
        reply_capability.is_some(),
        setting,
    )
    .map_err(refused)?;
    // Story 19.18d `A11` — the reference resolves in the caller's scope.
    let scope = caller_scope(state, caller);
    let not_found = || JsonRpcErrorResponse::new(echo.clone(), CODE_TASK_NOT_FOUND, ITEM_NOT_FOUND);
    let nearest = match state.recipient_items.nearest_item(&scope, reference) {
        Some(nearest) => nearest,
        None if refresh_recipient_items(state).await => state
            .recipient_items
            .nearest_item(&scope, reference)
            .ok_or_else(not_found)?,
        None => return Err(not_found()),
    };
    let matched = presented_matches(nearest.capability_hash.as_deref(), reply_capability);
    match post_lookup(
        caller.trust,
        CapabilityGatedVerb::ThreadOrFollowUp,
        nearest.capability_hash.as_deref(),
        matched,
        setting,
    ) {
        CapabilityVerdict::Serve => Ok(Some(FollowUpReference {
            id: reference.to_owned(),
            matched,
        })),
        CapabilityVerdict::NotFound => Err(not_found()),
        CapabilityVerdict::AccessOff => Err(refused(THREAD_ACCESS_OFF)),
    }
}

/// Admit a task: register the store entry, then either run it or park it on a
/// human decision.
///
/// **R10 — this function never awaits a person.** `ApprovalSource::RemotePeer`
/// renders a TUI permission prompt; the outermost request deadline is 30s. An
/// approval awaited inline therefore fails with `-32603` and every client retry
/// queues another operator prompt — an unbounded prompt-flood over an
/// authenticated but pre-approval channel. The approval is raised, the wire
/// answers `auth-required`, and a detached watcher resumes the task.
async fn start_task(
    state: &ServerState,
    caller: &Caller,
    task_id: String,
    text: String,
    message_type: SemanticMessageType,
    facts: IngressFacts,
    needs_approval: bool,
) -> Result<serde_json::Value, String> {
    let runtime = state
        .runtime
        .clone()
        .ok_or_else(|| "no execution runtime".to_owned())?;
    let principal = caller.key.policy_principal();
    // Keyed by the SUBMITTER, so a restart can rebuild scoping from the tree.
    let node_id = mint_inbound_node_id(&caller.key, &task_id);
    let task = state
        .tasks
        .insert(
            task_id.clone(),
            node_id,
            caller.key.clone(),
            RapTaskState::Submitted,
            facts.capability_hash.clone(),
        )
        .await
        .ok_or_else(|| format!("task id {task_id} is already in flight"))?;

    // Once the entry exists it must have an owned lifecycle, even if the HTTP
    // request times out and drops its receiver. `tokio::spawn` is deliberately
    // placed immediately after insertion with no await between them.
    let (response_tx, response_rx) = oneshot::channel();
    let state_for_setup = ServerState::clone(state);
    tokio::spawn(async move {
        let response = setup_task(
            state_for_setup,
            runtime,
            task,
            task_id,
            text,
            principal,
            message_type,
            facts,
            needs_approval,
        )
        .await;
        let _ = response_tx.send(response);
    });
    response_rx
        .await
        .map_err(|_| "A2A task setup ended before producing a task response".to_owned())?
}

/// Own the post-insert lifecycle independently of the HTTP handler.
///
/// The response is sent over a oneshot, but this future intentionally does not
/// depend on its receiver: a request deadline can cancel the handler without
/// canceling admission setup and leaving a live task with no lifecycle owner.
async fn setup_task(
    state: ServerState,
    runtime: Arc<dyn InboundPeerRuntime>,
    task: Arc<super::exec::InboundTask>,
    task_id: String,
    text: String,
    principal: PolicyPrincipal,
    message_type: SemanticMessageType,
    facts: IngressFacts,
    needs_approval: bool,
) -> Result<serde_json::Value, String> {
    let peer_id = principal.peer().clone();
    if needs_approval || runtime.enforces_sender_consent() {
        let consent_parked = runtime.enforces_sender_consent();
        if consent_parked
            && let Err(error) = runtime
                .park_pending_consent(&task.node_id, INBOUND_SUBAGENT_TYPE, task.cancel.clone())
                .await
        {
            tracing::error!(%error, task = %task_id, "failed to park consent-pending peer node");
            terminalize_start_failure(&state, &task, &peer_id, None).await;
            return task_projection(&state, &task, &peer_id).await;
        }
        let ticket = match runtime.request_admission_approval(&principal, &text).await {
            Ok(ticket) => ticket,
            Err(error) => {
                if consent_parked {
                    runtime.discard_pending_consent_node(&task.node_id).await;
                }
                tracing::error!(%error, task = %task_id, "failed to request A2A admission approval");
                terminalize_start_failure(&state, &task, &peer_id, None).await;
                return task_projection(&state, &task, &peer_id).await;
            }
        };

        if ticket.pending {
            let pending = PendingTaskRecord {
                task_id: task_id.clone(),
                submitter: task.submitter.as_str().to_owned(),
                // Story 19.18c `A6` — the hash only, so a restart reseeds
                // this pending task as capability-bearing.
                capability_hash: facts.capability_hash.clone(),
            };
            // Persist the admission outcome before exposing AuthRequired.
            let _ = state
                .transparency
                .record(InboundOutcome::AwaitingApproval {
                    principal: principal.clone(),
                    task_id: task_id.clone(),
                })
                .await;
            task.advance(RapTaskState::Working).await;
            task.advance(RapTaskState::AuthRequired).await;
            task.set_pending_auth(PendingAuth::Approval).await;
            append_pending_task(&state, pending.clone()).await;

            // Detached: the HTTP response is already decided. This watcher owns
            // every resolution of the auth-required state, including cancel.
            tokio::spawn(watch_pending_approval(
                state.clone(),
                runtime,
                task.clone(),
                text,
                principal,
                message_type,
                facts,
                pending,
                ticket,
            ));
            return Ok(
                projection_for(&state, &task_id, RapTaskState::AuthRequired, None, None).await,
            );
        }

        // Policy resolved it without a human. Still race cancellation so a
        // request canceled during setup cannot be registered after the fact.
        let decision = tokio::select! {
            biased;
            _ = task.cancel.cancelled() => {
                if consent_parked {
                    runtime.discard_pending_consent_node(&task.node_id).await;
                }
                terminalize_canceled(&state, &task, &peer_id, None).await;
                return task_projection(&state, &task, &peer_id).await;
            }
            decision = ticket.decision => decision.unwrap_or(
                crate::domain::ports::InboundApprovalDecision::Decline
            ),
        };
        if decision == crate::domain::ports::InboundApprovalDecision::RuntimeShutdown {
            if consent_parked {
                runtime.discard_pending_consent_node(&task.node_id).await;
            }
            terminalize_start_failure(&state, &task, &peer_id, None).await;
            return task_projection(&state, &task, &peer_id).await;
        }
        if decision == crate::domain::ports::InboundApprovalDecision::Decline {
            if consent_parked {
                runtime.discard_pending_consent_node(&task.node_id).await;
            }
            let reason = "admission policy declined this task".to_owned();
            let _ = state
                .transparency
                .record(InboundOutcome::Refused {
                    peer: peer_id.clone(),
                    task_id: task.id.clone(),
                    reason: reason.clone(),
                })
                .await;
            task.advance(RapTaskState::Working).await;
            task.set_detail(reason).await;
            task.advance(RapTaskState::Rejected).await;
            return task_projection(&state, &task, &peer_id).await;
        }
    }

    if task.cancel.is_cancelled() {
        runtime.discard_pending_consent_node(&task.node_id).await;
        terminalize_canceled(&state, &task, &peer_id, None).await;
        return task_projection(&state, &task, &peer_id).await;
    }
    launch(
        &state,
        &runtime,
        &task,
        text,
        principal,
        message_type,
        facts,
        None,
    )
    .await?;
    task_projection(&state, &task, &peer_id).await
}

async fn task_projection(
    state: &ServerState,
    task: &Arc<super::exec::InboundTask>,
    peer_id: &PeerId,
) -> Result<serde_json::Value, String> {
    let snapshot = task.snapshot().await;
    let text = snapshot
        .result
        .as_ref()
        .map(|result| result.text.as_str())
        .or(snapshot.detail.as_deref());
    let projection = projection_for(
        state,
        &task.id,
        snapshot.state,
        text,
        snapshot.item_address.as_ref(),
    )
    .await;
    if let Some(result) = snapshot.result.as_ref()
        && let Some(disclosed_bytes) = disclosed_text_bytes(&projection)
        && task.claim_result_disclosure().await
    {
        match state
            .transparency
            .record(InboundOutcome::Disclosed {
                peer: peer_id.clone(),
                node: task.node_id.clone(),
                task_id: task.id.clone(),
                disclosed_bytes,
                authorship: result.authorship,
                send_authorization: result.send_authorization,
            })
            .await
        {
            Ok(()) => task.commit_result_disclosure(),
            Err(error) => {
                task.release_result_disclosure();
                return Err(format!(
                    "withholding A2A result because its disclosure could not be journaled: {error}"
                ));
            }
        }
    }
    Ok(projection)
}

async fn terminalize_start_failure(
    state: &ServerState,
    task: &Arc<super::exec::InboundTask>,
    peer_id: &PeerId,
    pending: Option<&PendingTaskRecord>,
) {
    task.set_pending_auth(PendingAuth::None).await;
    task.set_detail(START_FAILURE_DETAIL).await;
    task.advance(RapTaskState::Failed).await;
    let _ = state
        .transparency
        .record(InboundOutcome::Refused {
            peer: peer_id.clone(),
            task_id: task.id.clone(),
            reason: START_FAILURE_DETAIL.to_owned(),
        })
        .await;
    if let Some(record) = pending {
        remove_pending_task(state, record).await;
    }
}

async fn terminalize_canceled(
    state: &ServerState,
    task: &Arc<super::exec::InboundTask>,
    peer_id: &PeerId,
    pending: Option<&PendingTaskRecord>,
) {
    task.set_pending_auth(PendingAuth::None).await;
    task.set_detail(CANCEL_DETAIL).await;
    task.advance(RapTaskState::Canceled).await;
    let _ = state
        .transparency
        .record(InboundOutcome::Refused {
            peer: peer_id.clone(),
            task_id: task.id.clone(),
            reason: CANCEL_DETAIL.to_owned(),
        })
        .await;
    if let Some(record) = pending {
        remove_pending_task(state, record).await;
    }
}

async fn watch_pending_approval(
    state: ServerState,
    runtime: Arc<dyn InboundPeerRuntime>,
    task: Arc<super::exec::InboundTask>,
    text: String,
    principal: PolicyPrincipal,
    message_type: SemanticMessageType,
    facts: IngressFacts,
    pending: PendingTaskRecord,
    ticket: InboundApprovalTicket,
) {
    let peer_id = principal.peer();
    // Cancellation wins an already-ready grant too. Dropping the ticket receiver
    // on this branch withdraws this task's interest in the operator decision.
    let decision = tokio::select! {
        biased;
        _ = task.cancel.cancelled() => {
            runtime.discard_pending_consent_node(&task.node_id).await;
            terminalize_canceled(&state, &task, peer_id, Some(&pending)).await;
            return;
        }
        decision = ticket.decision => decision.unwrap_or(
            crate::domain::ports::InboundApprovalDecision::Decline
        ),
    };

    // Story 19.20c: runtime shutdown cancels the responder, not the admission.
    // Keep its pending record (including capability hash) and parked node so
    // the existing cold-restart passes restore it as failed with RESTART_DETAIL.
    if decision == crate::domain::ports::InboundApprovalDecision::RuntimeShutdown {
        return;
    }

    task.set_pending_auth(PendingAuth::None).await;
    if decision == crate::domain::ports::InboundApprovalDecision::Decline {
        runtime.discard_pending_consent_node(&task.node_id).await;
        let reason = "the operator declined this task".to_owned();
        task.set_detail(reason.clone()).await;
        task.advance(RapTaskState::Rejected).await;
        let _ = state
            .transparency
            .record(InboundOutcome::Refused {
                peer: peer_id.clone(),
                task_id: task.id.clone(),
                reason,
            })
            .await;
        remove_pending_task(&state, &pending).await;
        return;
    }
    if task.cancel.is_cancelled() {
        runtime.discard_pending_consent_node(&task.node_id).await;
        terminalize_canceled(&state, &task, peer_id, Some(&pending)).await;
        return;
    }
    let _ = launch(
        &state,
        &runtime,
        &task,
        text,
        principal,
        message_type,
        facts,
        Some(pending),
    )
    .await;
}

/// Record acceptance durably, then register the peer node, drive the turn, and
/// watch the node's lifecycle back onto the task's wire state.
async fn launch(
    state: &ServerState,
    runtime: &Arc<dyn InboundPeerRuntime>,
    task: &Arc<super::exec::InboundTask>,
    text: String,
    principal: PolicyPrincipal,
    message_type: SemanticMessageType,
    facts: IngressFacts,
    pending: Option<PendingTaskRecord>,
) -> Result<(), String> {
    let peer_id = principal.peer().clone();
    // Story 19.18d `F5` — the item principal comes from the submitter handle
    // (`A2aBound(pin)` for a bound key), ⛔ never from the policy principal.
    let address = match state
        .item_allocator
        .allocate_a2a_ingress(task.submitter.ingress_principal())
        .await
    {
        Ok(address) => address,
        Err(error) => {
            tracing::error!(%error, task = %task.id, "refusing A2A task after item-id collision");
            task.fail_without_execution(error.to_string()).await;
            if let Some(record) = pending.as_ref() {
                remove_pending_task(state, record).await;
            }
            return Err(error.to_string());
        }
    };
    let received = RoomEvent::RecipientItemReceived {
        address: address.clone(),
        message_type,
        authorship: facts.authorship,
        send_authorization: facts.send_authorization,
        stories: facts.stories,
        task: task.id.clone(),
        // Story 19.18d `P2`/`F11` — the matched bound slot's alias (display
        // attribution only); `None` for loopback and unbound keys.
        alias: facts.ingress_alias,
        content: text.clone(),
        // Story 19.18 `AC5(c)` — the reference `message_send` already
        // resolved inside this caller's principal, stored raw.
        in_reply_to: facts.in_reply_to,
        // Story 19.18c `A13`/`A14` — the digest only, for every caller trust:
        // the capability is a property of the item.
        capability_hash: facts.capability_hash,
        reply_capability_matched: facts.reply_capability_matched,
    };
    // The fold lock (19.16b review): the durable append and the in-place
    // `apply` must be atomic against the read-side refresh's load +
    // `replace_from` + watermark store, or a snapshot loaded before this
    // append can land after the apply and erase the just-accepted item.
    {
        let _fold_guard = state.recipient_items_fold.lock().await;
        if let Err(error) = state.transparency.record_room_event(received.clone()).await {
            tracing::error!(%error, task = %task.id, "refusing A2A task: recipient item was not durable");
            task.fail_without_execution(UNRECORDED_ACCEPT_DETAIL).await;
            if let Some(record) = pending.as_ref() {
                remove_pending_task(state, record).await;
            }
            return Err(error.to_string());
        }
        state.recipient_items.apply(&received);
    }
    task.set_item_address(address).await;

    // ── AC1 fail-closed keystone ────────────────────────────────────────────
    // The canonical acceptance must exist before `InboundPeerRuntime::start`
    // can register a node or spawn provider/tool work. A failed append therefore
    // reaches only task-local bookkeeping and a refusal response.
    if let Err(error) = state
        .transparency
        .record(InboundOutcome::Accepted {
            peer: peer_id.clone(),
            node: task.node_id.clone(),
            task_id: task.id.clone(),
        })
        .await
    {
        runtime.discard_pending_consent_node(&task.node_id).await;
        tracing::error!(
            %error,
            task = %task.id,
            "refusing an admitted A2A task: its acceptance could not be journaled"
        );
        task.fail_without_execution(UNRECORDED_ACCEPT_DETAIL).await;
        if let Some(record) = pending.as_ref() {
            remove_pending_task(state, record).await;
        }
        // Journaling the refusal is best-effort by construction: the journal
        // is the thing that just failed. The sink latches the condition.
        let _ = state
            .transparency
            .record(InboundOutcome::Refused {
                peer: peer_id.clone(),
                task_id: task.id.clone(),
                reason: UNRECORDED_ACCEPT_DETAIL.to_owned(),
            })
            .await;
        // AC1(e)/AD-1803 fail closed: the acceptance was never journaled, so
        // the request path must surface a JSON-RPC error, not a success
        // envelope wrapping a task that never executed. On the detached
        // watcher path this `Err` is discarded by design.
        return Err(error.to_string());
    }
    task.advance(RapTaskState::Working).await;
    let response_policy = runtime.response_policy(&principal, message_type);
    let started = runtime
        .start(
            InboundPeerTask {
                node_id: task.node_id.clone(),
                principal,
                text,
                subagent_type: INBOUND_SUBAGENT_TYPE.to_owned(),
                response_policy,
            },
            task.cancel.clone(),
        )
        .await;

    let mut status = match started {
        Ok(status) => status,
        Err(error) => {
            tracing::error!(%error, task = %task.id, "failed to start A2A inbound task");
            terminalize_start_failure(state, task, &peer_id, pending.as_ref()).await;
            return Ok(());
        }
    };
    let node_state = *status.borrow();
    let pending_auth = task.pending_auth().await;
    let initial = project_node_to_rap(node_state, pending_auth);
    task.advance(initial).await;
    if initial.is_terminal() {
        if initial == RapTaskState::Completed {
            task.set_result(runtime.take_result(&task.node_id).await)
                .await;
        } else if initial == RapTaskState::Failed {
            task.set_detail(FAILURE_DETAIL).await;
        }
        if let Some(record) = pending.as_ref() {
            remove_pending_task(state, record).await;
        }
        return Ok(());
    }

    let state = ServerState::clone(state);
    let task = task.clone();
    let runtime = runtime.clone();
    tokio::spawn(async move {
        loop {
            let node_state = *status.borrow_and_update();
            let pending_auth = task.pending_auth().await;
            let projected = project_node_to_rap(node_state, pending_auth);
            if projected.is_terminal() {
                if projected == RapTaskState::Canceled {
                    task.set_detail(CANCEL_DETAIL).await;
                    let _ = state
                        .transparency
                        .record(InboundOutcome::Refused {
                            peer: peer_id.clone(),
                            task_id: task.id.clone(),
                            reason: CANCEL_DETAIL.to_owned(),
                        })
                        .await;
                } else if projected == RapTaskState::Failed {
                    // Deliberately opaque. The underlying failure names the
                    // host's model configuration, endpoint, and credential
                    // state — none of which a remote submitter is entitled to,
                    // and all of which a helpful error message would disclose.
                    task.set_detail(FAILURE_DETAIL).await;
                }
                let result = runtime.take_result(&task.node_id).await;
                if projected == RapTaskState::Completed {
                    task.set_result(result).await;
                }
                task.advance(projected).await;
                if let Some(record) = pending.as_ref() {
                    remove_pending_task(&state, record).await;
                }
                break;
            }
            task.advance(projected).await;
            if status.changed().await.is_err() {
                // The node vanished without a terminal state. Say so rather than
                // leaving a zombie `working` on the wire forever.
                tracing::warn!(task = %task.id, "A2A inbound node ended without a terminal state");
                task.set_detail(FAILURE_DETAIL).await;
                let _ = runtime.take_result(&task.node_id).await;
                task.advance(RapTaskState::Failed).await;
                if let Some(record) = pending.as_ref() {
                    remove_pending_task(&state, record).await;
                }
                break;
            }
        }
    });
    Ok(())
}

// ── tasks/get, tasks/cancel ─────────────────────────────────────────────────

/// Look up a task **as** the calling credential.
///
/// A task that exists but belongs to another credential produces the byte-identical
/// response of a task that does not exist. See `exec::InboundTaskStore::get_scoped`
/// and the ADR for why the imprecision is deliberate.
///
/// Story 19.18e `A4` — a bound caller whose pin adopts pre-binding keys
/// ([`adopted_submitters`], computed only after its own handle misses) also
/// reaches those keys' tasks recovered at the restart that applied the
/// binding: exact first, one terminal adopted hit served, a live or ambiguous
/// one not found (`exec::InboundTaskStore::get_in_scope`). The capability gate
/// below applies to whichever task the lookup returned.
///
/// Story 19.18c `A6`/`P8` — for an API-key caller a capability-bearing task is
/// served only with its capability, presented in the A2A extension slot
/// `params.metadata["x-rustain-item-capability"]`; a miss is the same
/// `TASK_NOT_FOUND`, so `tasks/get` journals no `StatusQueried` and
/// `tasks/cancel` fires no token. A `metadata` that is not an object, or a
/// malformed carrier, is `-32602` for every caller; `null` is absent.
async fn scoped_task(
    state: &ServerState,
    caller: &Caller,
    params: &serde_json::Value,
    echo: &serde_json::Value,
) -> Result<Arc<super::exec::InboundTask>, JsonRpcErrorResponse> {
    let invalid = || JsonRpcErrorResponse::new(echo.clone(), CODE_INVALID_PARAMS, "Invalid params");
    let id = params
        .get("id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(invalid)?;
    let presented = match params.get("metadata") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Object(metadata)) => {
            parse_presented_capability(metadata.get(super::ITEM_CAPABILITY_METADATA_KEY))
                .map_err(|_| invalid())?
        }
        Some(_) => return Err(invalid()),
    };
    let not_found = || JsonRpcErrorResponse::new(echo.clone(), CODE_TASK_NOT_FOUND, TASK_NOT_FOUND);
    let task = state
        .tasks
        .get_in_scope(id, &caller.key, || adopted_submitters(state, caller))
        .await
        .ok_or_else(not_found)?;
    let hash = task.capability_hash.as_deref();
    match post_lookup(
        caller.trust,
        CapabilityGatedVerb::RetractOrTask,
        hash,
        presented_matches(hash, presented.as_ref()),
        state.security.thread_reads,
    ) {
        CapabilityVerdict::Serve => Ok(task),
        CapabilityVerdict::NotFound | CapabilityVerdict::AccessOff => Err(not_found()),
    }
}

async fn tasks_get(
    state: &ServerState,
    caller: &Caller,
    params: &serde_json::Value,
    echo: serde_json::Value,
) -> Result<serde_json::Value, JsonRpcErrorResponse> {
    let task = scoped_task(state, caller, params, &echo).await?;
    let principal = caller.key.policy_principal();
    // FR92 (P-5): journal the FIRST status query observed for a task and
    // nothing thereafter. The task-local state has an in-flight claim, not a
    // boolean: a failed append releases the claim so a later poll can retry
    // rather than permanently hiding the observation.
    if task.claim_status_query() {
        let recorded = state
            .transparency
            .record(InboundOutcome::StatusQueried {
                principal: principal.clone(),
                task_id: task.id.clone(),
            })
            .await;
        if recorded.is_ok() {
            task.commit_status_query();
        } else {
            task.release_status_query();
        }
    }
    task_projection(state, &task, principal.peer())
        .await
        .map_err(|error| {
            tracing::error!(%error, task = %task.id, "withholding unjournaled A2A result");
            JsonRpcErrorResponse::new(echo, CODE_INTERNAL_ERROR, "Internal error")
        })
}

async fn tasks_cancel(
    state: &ServerState,
    caller: &Caller,
    params: &serde_json::Value,
    echo: serde_json::Value,
) -> Result<serde_json::Value, JsonRpcErrorResponse> {
    let task = scoped_task(state, caller, params, &echo).await?;
    if !task.is_terminal().await {
        // The real seam: `drive_preloaded_turn` already takes this token, so
        // cancelling it interrupts the running turn rather than merely relabelling
        // the wire state. Wait for the lifecycle owner to persist its terminal
        // transition, bounded inside the outer 30-second HTTP deadline.
        task.cancel.cancel();
        let _ = tokio::time::timeout(CANCEL_WAIT_TIMEOUT, task.wait_for_terminal()).await;
    }
    // Story 19.18d `F5` — the same display peer `tasks_get` shows.
    let peer_id = caller.key.policy_principal().peer().clone();
    task_projection(state, &task, &peer_id)
        .await
        .map_err(|error| {
            tracing::error!(%error, task = %task.id, "withholding unjournaled A2A result");
            JsonRpcErrorResponse::new(echo, CODE_INTERNAL_ERROR, "Internal error")
        })
}

// ── x-rustain-items/list ────────────────────────────────────────────────────

/// Serve the calling principal's own recipient-item set (Story 19.16b, AC1).
///
/// # The three properties this verb exists to hold
///
/// **Per peer, never per item** (`AD-1822`): the result is the caller's *set*,
/// so a sender reads every recipient item it caused on this host in one call.
/// ⛔ `tasks/get` is the anti-pattern — per item, and it journals twice.
///
/// **Side-effect free** (`AD-1822`: *"reading acknowledgement state is not
/// itself an acknowledgement, and never marks anything"*). ⛔ Nothing here
/// writes: the only journal traffic is the read-side re-fold below, and the
/// keystone ratchet is journal-length equality across N calls.
///
/// **The scope filter runs FIRST, the tombstone classification second**
/// (`A22`). The scope comes from the authenticated caller through
/// [`caller_scope`] — the caller's item principal plus, for a bound key, the
/// pseudonyms its bindings adopted (Story 19.18d `A11`) — ⛔ never from a
/// request parameter. Classifying a tombstone before filtering would answer
/// *"that id exists but is not yours"* and reconstitute the cross-peer
/// enumeration oracle `ADR-17-4a-01` R21 kills; another principal's removed
/// item is **absent**, byte-identical to an id that never existed.
///
/// ⛔ `journal_order` never reaches the wire (AC1(f)): it is *not* stable
/// across the cold-fold and live-apply assignment schemes, so a leaked number
/// makes two honest hosts disagree. The serialized `ordinal` is a dense
/// per-peer arrival index derived at render time, which is also
/// `DF-18-3a-b`'s host-minted, peer-byte-free discriminator.
async fn items_list(
    state: &ServerState,
    caller: &Caller,
) -> Result<serde_json::Value, JsonRpcErrorResponse> {
    refresh_recipient_items(state).await;
    let scope = caller_scope(state, caller);
    let items = state
        .recipient_items
        .find_in_scope(&scope)
        .into_iter()
        .enumerate()
        .map(|(ordinal, item)| {
            // The wire stays three-state (`AC1(e)`): the tag is a bare string
            // and the predecessor fact rides as an ADDITIVE sibling field only
            // when it exists — ⛔ never a fifth state, never a widened token.
            let mut row = serde_json::json!({
                "itemId": item.address.item().as_str(),
                "task": item.task,
                "state": item.state.wire_name(),
                "ordinal": ordinal,
            });
            if item.state.acknowledged_before_removal() == Some(true) {
                row["acknowledgedBeforeRemoval"] = serde_json::Value::Bool(true);
            }
            // Story 19.16d AC4(a) — the retract mark, the same ADDITIVE
            // discipline: emitted only when present, and ⛔ `state` untouched.
            // A new `state` token would be WITHHELD by every 19.16b board
            // (`parse_peer_reply` names three states and withholds the rest).
            if let Some(retracted_at_ms) = item.retracted_at_ms {
                row["retractedAtMs"] = serde_json::Value::from(retracted_at_ms);
            }
            // Story 19.18 `AC4(f)` (`P7`/`Q4`) — the board's reply signal, a
            // flat sibling scalar emitted only when ≥ 1: the wire stays
            // three-state and a reply-less row is byte-identical to 19.16d's.
            let replies = state.recipient_items.reply_count(&item.address);
            if replies > 0 {
                row["replyCount"] = serde_json::Value::from(replies);
            }
            row
        })
        .collect::<Vec<_>>();
    let mut result = serde_json::json!({
        "items": items,
        // Story 19.16b AC2(c) — fail-closed on authority, fail-LOUD on
        // legibility. On a loopback bind `authenticate` returns
        // `SubmitterKey::loopback()` for every caller, so every local caller is
        // one principal. That is accurate (they all
        // already run code on this machine) and it buys the read nothing to
        // refuse it — but a board that renders per-recipient attribution over
        // a collapsed principal would claim precision the substrate does not
        // have. One rule, keyed off the trust the caller already carries; ⛔ no
        // second code path, and ⛔ never only in a log.
        "principalCollapsed": principal_collapsed(caller.trust),
    });
    mark_principal_bound(&mut result, caller.trust);
    Ok(result)
}

/// Story 19.16b AC2(c) / 19.16d AC2(c) — the collapse disclosure's one rule,
/// read by the list's response, the retract's response and the retract's
/// durable record alike. `true` exactly when the caller reached this host over
/// a loopback bind, where `authenticate` answers `SubmitterKey::loopback()`
/// for everyone. ⛔ A legibility statement about attribution, never a claim
/// about who acted; ⛔ no second code path computes it. Exhaustive, so a new
/// trust class decides (Story 19.18d: a bound key is never collapsed).
fn principal_collapsed(trust: SubmitterTrust) -> bool {
    match trust {
        SubmitterTrust::Loopback => true,
        SubmitterTrust::ApiKey | SubmitterTrust::BoundApiKey => false,
    }
}

/// Story 19.18d `A8` — the one rule for the `principalBound` wire flag: `true`
/// only for a caller whose key the operator bound to a pinned peer.
fn principal_bound(trust: SubmitterTrust) -> bool {
    match trust {
        SubmitterTrust::BoundApiKey => true,
        SubmitterTrust::Loopback | SubmitterTrust::ApiKey => false,
    }
}

/// Story 19.18d `A8`/`P12` — add `principalBound: true` to a successful
/// list/thread/retract payload, and only when true: an unbound or loopback
/// response stays byte-identical to 19.18c's. Additive (AD-1826).
fn mark_principal_bound(result: &mut serde_json::Value, trust: SubmitterTrust) {
    if principal_bound(trust) {
        result["principalBound"] = serde_json::Value::Bool(true);
    }
}

// ── x-rustain-items/thread ──────────────────────────────────────────────────

/// Serve one thread of the calling principal's (Story 19.18, `AC4`).
///
/// # What it guarantees, in order
///
/// 1. **`itemId` and an optional `capability` are the only parameters**,
///    hand-read — unknown fields are ignored (`AD-1826`); an empty or
///    oversized id, or a malformed capability, ⇒ `-32602` for every caller.
/// 2. **The thread-access gate first for a capability-less API-key caller**
///    (`AC12`, Story 19.18c `A7` step 2): refused `-32040` under `off`
///    before any fold or lookup, so the refusal leaks nothing about the id
///    (`M44`). A caller presenting a capability is classified after the
///    lookup.
/// 3. **The fold is refreshed unconditionally**, as `items_list` does: reply
///    turns are written on the DAEMON rail, which never applies this
///    projection in place, so a lookup-first read would serve a thread
///    without its newest answer. An unchanged journal costs one O(1) probe.
/// 4. **Principal first** (`A6`, R21): the join indexes only this caller's
///    items and turns; not-yours and never-existed leave through ONE site
///    with ONE message. A removed root is a tombstone turn, not not-found.
/// 5. **The nearest item's capability** (Story 19.18c `A7` step 5, `A8`):
///    an API-key caller reads a capability-bearing item's thread only with
///    its capability — a miss leaves through the same not-found site, byte
///    for byte; a legacy item keeps the `threadReads` gate.
/// 6. **Side-effect free** (`AC4(e)`): ⛔ nothing here writes.
/// 7. **Bounded by count and bytes, newest wins** (`P9`, `Q8`): the root is
///    always served (withheld if it alone is over the per-turn bound), then
///    the newest turns that fit; `omittedBefore` counts the non-root turns
///    not served.
/// 8. **Reply content passes the disclosure scrub** every served task payload
///    passes; a body that fails it is served `contentWithheld`, no content.
///
/// ⛔ Private, reachable only through `dispatch` (Rule 2).
async fn items_thread(
    state: &ServerState,
    caller: &Caller,
    params: &serde_json::Value,
    echo: serde_json::Value,
) -> Result<serde_json::Value, JsonRpcErrorResponse> {
    let invalid = || JsonRpcErrorResponse::new(echo.clone(), CODE_INVALID_PARAMS, "Invalid params");
    let item_id = params
        .get("itemId")
        .and_then(serde_json::Value::as_str)
        .filter(|id| {
            !id.is_empty() && id.len() <= crate::domain::services::transparency::MAX_PEER_ID_BYTES
        })
        .ok_or_else(invalid)?;
    let presented = parse_presented_capability(params.get("capability")).map_err(|_| invalid())?;
    let setting = state.security.thread_reads;
    let refused =
        |reason: &str| JsonRpcErrorResponse::new(echo.clone(), CODE_REFUSED_BY_POLICY, reason);
    pre_lookup(
        caller.trust,
        CapabilityGatedVerb::ThreadOrFollowUp,
        presented.is_some(),
        setting,
    )
    .map_err(refused)?;
    refresh_recipient_items(state).await;
    // Story 19.18d `A11` — the caller's scope, as the list reads it.
    let scope = caller_scope(state, caller);
    let not_found = || JsonRpcErrorResponse::new(echo.clone(), CODE_TASK_NOT_FOUND, ITEM_NOT_FOUND);
    let nearest = state
        .recipient_items
        .nearest_item(&scope, item_id)
        .ok_or_else(not_found)?;
    let hash = nearest.capability_hash.as_deref();
    match post_lookup(
        caller.trust,
        CapabilityGatedVerb::ThreadOrFollowUp,
        hash,
        presented_matches(hash, presented.as_ref()),
        setting,
    ) {
        CapabilityVerdict::Serve => {}
        CapabilityVerdict::NotFound => return Err(not_found()),
        CapabilityVerdict::AccessOff => return Err(refused(THREAD_ACCESS_OFF)),
    }
    let Some((root, turns)) = state.recipient_items.thread(&scope, item_id) else {
        return Err(not_found());
    };
    let (served, omitted_before) = bounded_thread(state, &root, &turns);
    let mut result = serde_json::json!({
        "root": root,
        "turns": served,
        "omittedBefore": omitted_before,
        "principalCollapsed": principal_collapsed(caller.trust),
    });
    mark_principal_bound(&mut result, caller.trust);
    Ok(result)
}

/// Serialize one thread under the count and byte budgets, newest turns
/// winning (`P9`). Returns the served turns in thread order and how many
/// non-root turns were left out before them.
fn bounded_thread(
    state: &ServerState,
    root: &str,
    turns: &[crate::adapters::policy::recipient_item::ThreadTurn],
) -> (Vec<serde_json::Value>, usize) {
    let serialized = |ordinal: usize, turn| {
        let mut value = thread_turn_json(state, ordinal, turn);
        if serde_json::to_vec(&value).map_or(usize::MAX, |bytes| bytes.len())
            > MAX_THREAD_TURN_BYTES
        {
            withhold_turn_content(&mut value);
        }
        let bytes = serde_json::to_vec(&value).map_or(usize::MAX, |bytes| bytes.len());
        (value, bytes)
    };
    // The dense ordinal is the turn's position in the WHOLE thread, so an
    // omitted prefix is visible as a gap. ⛔ `journal_order` never reaches
    // the wire (19.16b `AC1(f)`).
    let root_index = turns.iter().position(|turn| turn.id() == root);
    let mut values = Vec::with_capacity(MAX_THREAD_TURNS);
    let selected = crate::domain::services::recipient_thread_bounds::newest_indices(
        turns.len(),
        root_index,
        |index| {
            let (value, bytes) = serialized(index, &turns[index]);
            values.push((index, value));
            bytes
        },
    );
    let omitted_before = turns.len().saturating_sub(selected.len());
    values.sort_by_key(|(index, _)| *index);
    (
        values
            .into_iter()
            .filter(|(index, _)| selected.binary_search(index).is_ok())
            .map(|(_, value)| value)
            .collect(),
        omitted_before,
    )
}

/// One served turn (`AC4(c)`). Items carry `task` (the asker's `you` check
/// matches on it), their three-state wire name and — unless removed — their
/// content; reply turns carry the recorded authorship and scrubbed content.
fn thread_turn_json(
    state: &ServerState,
    ordinal: usize,
    turn: &crate::adapters::policy::recipient_item::ThreadTurn,
) -> serde_json::Value {
    use crate::domain::models::{RecipientItemState, ReplyKind};

    match turn {
        ThreadTurn::Item(item) => {
            let mut value = serde_json::json!({
                "turnId": item.address.item().as_str(),
                "kind": "item",
                "task": item.task,
                "state": item.state.wire_name(),
                "ordinal": ordinal,
            });
            if let Some(parent) = &item.in_reply_to {
                value["inReplyTo"] = serde_json::Value::from(parent.as_str());
            }
            match &item.state {
                RecipientItemState::Received { content }
                | RecipientItemState::Acknowledged { content } => {
                    value["content"] = serde_json::Value::from(content.as_str());
                }
                RecipientItemState::Removed { .. } => {}
            }
            if let Some(retracted_at_ms) = item.retracted_at_ms {
                value["retractedAtMs"] = serde_json::Value::from(retracted_at_ms);
            }
            value
        }
        ThreadTurn::Reply(reply) => {
            let mut value = serde_json::json!({
                "turnId": reply.address.turn().as_str(),
                "kind": "reply",
                "inReplyTo": reply.answers.as_str(),
                "authorship": reply.authorship.wire_name(),
                "ordinal": ordinal,
            });
            match reply.kind {
                ReplyKind::Answer => {}
                ReplyKind::Defer { minutes } => {
                    value["replyKind"] = serde_json::Value::from("defer");
                    value["deferMinutes"] = serde_json::Value::from(minutes);
                }
                ReplyKind::Status => {
                    value["replyKind"] = serde_json::Value::from("status");
                    if let Some(status) = &reply.status {
                        value["status"] = status.to_wire();
                    }
                }
                ReplyKind::Unknown => {
                    value["replyKind"] = serde_json::Value::from("unknown");
                }
            }
            if super::projection::withholds(
                &reply.content,
                state.workspace.as_path(),
                &state.scrub_fragments,
            ) {
                withhold_turn_content(&mut value);
            } else {
                value["content"] = serde_json::Value::from(reply.content.as_str());
            }
            value
        }
    }
}

/// Replace a turn's body with the withheld marker — ⛔ never drop the turn.
fn withhold_turn_content(value: &mut serde_json::Value) {
    if let Some(object) = value.as_object_mut() {
        object.remove("content");
        object.remove("status");
        object.insert("contentWithheld".to_owned(), serde_json::Value::Bool(true));
    }
}

// ── x-rustain-items/retract ─────────────────────────────────────────────────

/// Mark the calling principal's own item as retracted by its sender (Story
/// 19.16d, AC1–AC3). The recipient host's half of `DF-18-4-CROSSHOST-RETRACT`.
///
/// # What it guarantees, in order
///
/// 1. **The item id is the only parameter** (`AC1(c)`), hand-read from the
///    `Value` — ⛔ no closed schema, so an unknown field from a newer build is
///    ignored (`AD-1826`). The scope arrives from `dispatch`, derived from
///    the authenticated caller; ⛔ nothing in `params` can name it.
/// 2. **Authority before any mutation** (`AC1(g)`, NFR70): the pure
///    `admit_item_retract` refuses under `deny` and `ask` before the fold or
///    the journal is touched.
/// 3. **Not-yours is byte-identical to never-existed** (`AC1(d)`, R21): the
///    item is resolved by id **within the caller's scope**, and both misses
///    leave through ONE `JsonRpcErrorResponse::new` site with ONE message.
///    Story 19.18d `F6`: from then on the retract uses that item's **stored**
///    address — the re-lookup and the journaled record — ⛔ never an address
///    rebuilt from the caller (an adopted item lives under its old pseudonym).
/// 4. **Lookup first, refresh only on a miss, then ONE critical section**
///    (`P4`): an unowned or fabricated id costs the O(1) tail probe and never
///    takes the fold lock; an owned id takes it once for decide + append +
///    apply, so N concurrent retracts of one item append exactly one record.
///    ⚠ Inside the section the fold is caught up to the journal first (the
///    lock-held half of the refresh, which never re-takes the non-reentrant
///    lock): without it an operator's `/team remove` — journalled on the
///    daemon rail, invisible to this fold until the next refresh — would let a
///    retract mark a tombstone `Q3` rules must refuse.
/// 5. **Journal first, fail closed** (`AD-1827`, `AD-1803`): the durable
///    record is appended before the in-memory `apply`; an append failure
///    refuses and leaves the item unmarked.
/// 6. **Idempotent on the live states, refused on a tombstone** (`AC1(e)`,
///    `Q3`), both decided by `RecipientItemView::retract_outcome` — the one
///    expression of the rule the fold also applies.
///
/// ⛔ Private, and reachable only through `dispatch`: a test cannot call it
/// directly, which keeps the front-door bypass structurally unavailable
/// (Rule 2).
async fn items_retract(
    state: &ServerState,
    scope: &ItemScope,
    trust: SubmitterTrust,
    params: &serde_json::Value,
    echo: serde_json::Value,
) -> Result<serde_json::Value, JsonRpcErrorResponse> {
    use std::sync::atomic::Ordering;

    let item_id = params
        .get("itemId")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            JsonRpcErrorResponse::new(echo.clone(), CODE_INVALID_PARAMS, "Invalid params")
        })?;
    // Story 19.18c `A7` step 1 — one parse point, every caller trust.
    let presented = parse_presented_capability(params.get("capability")).map_err(|_| {
        JsonRpcErrorResponse::new(echo.clone(), CODE_INVALID_PARAMS, "Invalid params")
    })?;
    admit_item_retract(state.policy, trust).map_err(|reason| {
        JsonRpcErrorResponse::new(echo.clone(), CODE_REFUSED_BY_POLICY, reason)
    })?;

    let not_found = || JsonRpcErrorResponse::new(echo.clone(), CODE_TASK_NOT_FOUND, ITEM_NOT_FOUND);
    let internal_error =
        || JsonRpcErrorResponse::new(echo.clone(), CODE_INTERNAL_ERROR, "Internal error");

    let item = match state.recipient_items.item_in_scope(scope, item_id) {
        Some(item) => item,
        None => {
            if !refresh_recipient_items(state).await {
                return Err(internal_error());
            }
            state
                .recipient_items
                .item_in_scope(scope, item_id)
                .ok_or_else(not_found)?
        }
    };
    let address = item.address.clone();
    // Story 19.18c `D2`/`AC4(c)` — the capability check runs HERE, at the
    // pre-lock lookup: before the fold lock and before `retract_outcome`, so
    // a miss never reaches the tombstone (`-32041`) or `alreadyRetracted`
    // answers — either would tell a co-holder about another sender's item.
    // The hash is immutable per item (the first Received wins), so this one
    // check holds for the whole critical section below.
    let hash = item.capability_hash.as_deref();
    if post_lookup(
        trust,
        CapabilityGatedVerb::RetractOrTask,
        hash,
        presented_matches(hash, presented.as_ref()),
        state.security.thread_reads,
    ) != CapabilityVerdict::Serve
    {
        return Err(not_found());
    }

    let fold_guard = state.recipient_items_fold.lock().await;
    let Some(latest) = probe_journal_tail(state).await else {
        return Err(internal_error());
    };
    if !fold_journal_growth(state, &fold_guard, latest).await {
        return Err(internal_error());
    }
    let item = state.recipient_items.get(&address).ok_or_else(not_found)?;
    let collapsed = principal_collapsed(trust);
    let retracted_at_ms = match item.retract_outcome() {
        RetractOutcome::Mark => wall_now_ms(),
        RetractOutcome::AlreadyMarked => {
            return Ok(retract_result(item_id, item.retracted_at_ms, true, trust));
        }
        RetractOutcome::RefusedRemoved => {
            return Err(JsonRpcErrorResponse::new(
                echo.clone(),
                CODE_ITEM_REMOVED,
                ITEM_REMOVED,
            ));
        }
    };
    let retracted = RoomEvent::RecipientItemRetracted {
        address: address.clone(),
        retracted_at_ms,
        principal_collapsed: collapsed,
    };
    let watermark = state.recipient_items_seq.load(Ordering::Acquire);
    if let Err(error) = state
        .transparency
        .record_room_event(retracted.clone())
        .await
    {
        tracing::error!(%error, "refusing an A2A item retract: its record could not be journaled");
        return Err(internal_error());
    }

    // The daemon rail and another process append without taking
    // `recipient_items_fold`. Apply in place only when the retract is provably
    // the journal's sole new line. Otherwise replay the durable order before
    // answering: a removal immediately before this record must refuse the
    // mark, while a removal immediately after it preserves the landed mark.
    let Some(appended) = watermark.checked_add(1) else {
        return Err(internal_error());
    };
    let Some(latest) = probe_journal_tail(state).await else {
        return Err(internal_error());
    };
    if latest == appended {
        state.recipient_items.apply(&retracted);
        state.recipient_items_seq.store(appended, Ordering::Release);
    } else if !fold_journal_growth(state, &fold_guard, latest).await {
        return Err(internal_error());
    }

    let item = state
        .recipient_items
        .get(&address)
        .ok_or_else(internal_error)?;
    let persisted_at_ms = item.retracted_at_ms;
    let persisted_outcome = item.retract_outcome();
    drop(fold_guard);
    match persisted_at_ms {
        Some(stamp) => Ok(retract_result(
            item_id,
            Some(stamp),
            stamp != retracted_at_ms,
            trust,
        )),
        None if persisted_outcome == RetractOutcome::RefusedRemoved => Err(
            JsonRpcErrorResponse::new(echo.clone(), CODE_ITEM_REMOVED, ITEM_REMOVED),
        ),
        None => Err(internal_error()),
    }
}

/// The retract's success payload. `alreadyRetracted` tells a repeated call
/// from the one that landed the mark (the sender renders them differently);
/// `principalCollapsed` is the same disclosure the list carries, and
/// `principalBound` (Story 19.18d `A8`) rides only when true.
fn retract_result(
    item_id: &str,
    retracted_at_ms: Option<i64>,
    already_retracted: bool,
    trust: SubmitterTrust,
) -> serde_json::Value {
    let mut result = serde_json::json!({
        "itemId": item_id,
        "retractedAtMs": retracted_at_ms,
        "alreadyRetracted": already_retracted,
        "principalCollapsed": principal_collapsed(trust),
    });
    mark_principal_bound(&mut result, trust);
    result
}

/// This host's wall clock, the only source of a retract's stamp (⛔ never the
/// sender's). A pre-epoch clock yields `0`, which the fold keeps as `Some(0)`.
fn wall_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

/// Re-fold `recipient_items` from the durable journal when it has grown.
///
/// The in-memory projection is applied in place by exactly one writer — the
/// inbound `message/send` ingress. Every other recipient-item act (`/team
/// ack`, `/team remove`) arrives on the daemon socket and lands in the durable
/// journal, which this process does not otherwise re-read until the next
/// `serve`. Without this, the cross-host read would report `received` for an
/// item acknowledged hours ago, and `FR165`'s whole sentence — *"a sender can
/// observe each recipient's acknowledgement state"* — would be false on a
/// long-lived host.
///
/// ⛔ Not a write: `load_entries` is the same read-only reader `serve` uses.
/// The `seq` watermark makes a read with no new rows cost one O(1) tail probe
/// and no fold, mirroring the shipped `ConsentGate::refresh_projection`. A
/// load failure keeps the cached fold and warns for reads; callers performing
/// a write use the return value to fail closed instead of deciding on stale
/// state.
///
/// 19.16b review hardening, two parts:
/// - **Probe before loading.** `latest_seq` is the journal's chunked tail
///   read; a read with no new rows parses nothing. Before this, every served
///   read whole-file-parsed the journal under the shared flock.
/// - **Serialize against the ingress.** The load + `replace_from` + store
///   critical section takes `recipient_items_fold`, the same lock the ingress
///   holds across its append + `apply`, so a refresh can neither erase a
///   just-applied item nor strand the watermark ahead of the fold.
///
/// Returns whether the projection is authoritative through the observed
/// journal head.
async fn refresh_recipient_items(state: &ServerState) -> bool {
    use std::sync::atomic::Ordering;

    let Some(latest) = probe_journal_tail(state).await else {
        return false;
    };
    if latest <= state.recipient_items_seq.load(Ordering::Acquire) {
        return true;
    }
    let fold_guard = state.recipient_items_fold.lock().await;
    fold_journal_growth(state, &fold_guard, latest).await
}

/// The journal's last valid `seq`, or `None` — warned — when the tail probe
/// fails. Read-only callers may serve the cached fold; mutating callers must
/// fail closed.
async fn probe_journal_tail(state: &ServerState) -> Option<u64> {
    match state.transparency.latest_seq().await {
        Ok(latest) => Some(latest),
        Err(error) => {
            tracing::warn!(%error, "recipient-item projection refresh failed; serving cached fold");
            None
        }
    }
}

/// The lock-held half of [`refresh_recipient_items`]: re-fold when the journal
/// has grown past the watermark.
///
/// Takes the held `recipient_items_fold` guard as a witness and never locks
/// it: the mutex is a non-reentrant `tokio::sync::Mutex`, and this split is
/// what lets `items_retract` catch its fold up **inside** its own critical
/// section without the self-deadlock Story 19.16d `P4` measured (a hang that
/// only appears once the journal has grown, and that would also wedge
/// `message/send`'s ingress, which takes the same lock).
///
/// Returns whether the projection reached at least `latest`.
async fn fold_journal_growth(
    state: &ServerState,
    _held: &tokio::sync::MutexGuard<'_, ()>,
    latest: u64,
) -> bool {
    use std::sync::atomic::Ordering;

    // Re-check under the lock: a concurrent refresh may have folded past
    // `latest` while we waited.
    if latest <= state.recipient_items_seq.load(Ordering::Acquire) {
        return true;
    }
    let entries = match state.transparency.load_entries().await {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(%error, "recipient-item projection refresh failed; serving cached fold");
            return false;
        }
    };
    let max_seq = entries.last().map_or(0, |entry| entry.seq);
    if max_seq > state.recipient_items_seq.load(Ordering::Acquire) {
        state.recipient_items.replace_from(&entries);
        state.recipient_items_seq.store(max_seq, Ordering::Release);
    }
    state.recipient_items_seq.load(Ordering::Acquire) >= latest
}

// ── projection helpers ──────────────────────────────────────────────────────

/// Every served task payload goes through the one redaction boundary.
async fn projection_for(
    state: &ServerState,
    task_id: &str,
    task_state: RapTaskState,
    text: Option<&str>,
    item_address: Option<&crate::domain::models::ItemAddress>,
) -> serde_json::Value {
    RoomProjection::<RemotePeerViewer>::disclose(
        task_id,
        task_state,
        text,
        state.workspace.as_path(),
        &state.scrub_fragments,
        state.signer.identity().peer_id.to_string(),
    )
    .with_recipient_item_id(item_address.map(|address| address.item().as_str()))
    .to_task_json()
}

/// Byte length of the result text actually present in an A2A task response.
///
/// This reads the completed projection, not the untrusted/raw runtime output:
/// redaction may replace the result before it crosses the protocol boundary.
fn disclosed_text_bytes(task: &serde_json::Value) -> Option<usize> {
    task.pointer("/status/message/parts/0/text")
        .and_then(serde_json::Value::as_str)
        .map(str::len)
}

/// A refusal never touches the task store, so it is projected directly.
fn rejected_task_json(task_id: &str, reason: &str, signer: &AgentSigner) -> serde_json::Value {
    RoomProjection::<RemotePeerViewer>::disclose(
        task_id,
        RapTaskState::Rejected,
        Some(reason),
        // A refusal's reason is policy text we authored; there is no host state
        // in it, and passing a root we would never match keeps the scrub honest.
        std::path::Path::new(""),
        &[],
        signer.identity().peer_id.to_string(),
    )
    .to_task_json()
}

fn jsonrpc_id_text(id: &serde_json::Value) -> String {
    match id {
        serde_json::Value::String(value) => value.clone(),
        serde_json::Value::Number(value) => value.to_string(),
        _ => "unknown".to_owned(),
    }
}

/// A2A `message/send` requires a message with a role and at least one part.
/// Without this, `{"message":{}}` would receive a fabricated task instead of
/// `-32602`, and a client could not tell a malformed payload apart from a policy
/// verdict. Returns the concatenated text parts.
fn well_formed_message(message: Option<&serde_json::Value>) -> Option<String> {
    let message = message.and_then(serde_json::Value::as_object)?;
    message
        .get("role")
        .and_then(serde_json::Value::as_str)
        .filter(|role| !role.trim().is_empty())?;
    let parts = message.get("parts").and_then(serde_json::Value::as_array)?;
    if parts.is_empty() || !parts.iter().all(serde_json::Value::is_object) {
        return None;
    }

    let mut text_parts = Vec::new();
    for part in parts {
        if part.get("kind").and_then(serde_json::Value::as_str) == Some("text") {
            let text = part.get("text").and_then(serde_json::Value::as_str)?;
            if !text.trim().is_empty() {
                text_parts.push(text);
            }
        }
    }
    (!text_parts.is_empty()).then(|| text_parts.join("\n"))
}

fn json_response(value: &impl Serialize, status: StatusCode) -> Response {
    match serde_json::to_vec(value) {
        Ok(body) => (status, [(CONTENT_TYPE, "application/json")], body).into_response(),
        Err(error) => {
            tracing::error!(%error, "failed to serialize A2A JSON response");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Run the standalone A2A server front door.
///
/// `runtime` is `None` for a discovery-only deployment and `Some` when a core is
/// composed behind it (`rustain daemon start --serve-a2a=…`, or the standalone
/// path once it has built its core). `roster` is the active profile's resolved
/// A2A roster (Story 19.18d `A17`): `server.peerKeys` binds keys against it.
pub async fn run(
    addr: String,
    app_config: AppConfig,
    workspace: PathBuf,
    runtime: Option<Arc<dyn InboundPeerRuntime>>,
    transparency: Arc<TransparencySink>,
    roster: Vec<A2aPeerSpec>,
    ready: Option<oneshot::Sender<Result<(), String>>>,
) -> anyhow::Result<()> {
    let mut ready = ready;
    let result = run_inner(
        addr,
        app_config,
        workspace,
        runtime,
        transparency,
        &roster,
        &mut ready,
    )
    .await;
    if let Err(error) = &result
        && let Some(sender) = ready.take()
    {
        let _ = sender.send(Err(error.to_string()));
    }
    result
}

/// Story 19.16d `Q1` (owner answer 2+4 = A) — the retract's ownership check is
/// principal equality, and off loopback an **unbound** key's principal is a
/// digest of the presented key: nothing binds it to a peer. ⛔ A legibility
/// statement, never a security claim; the real control is `server.admission`
/// (`AC1(g)`). Story 19.18c `AC9(a)`: an item carrying a per-item capability
/// needs it; legacy items keep the credential-scoped rule. Story 19.18d
/// `A18`/`B5`: stated only while at least one unbound key is configured.
const CREDENTIAL_SCOPED_RETRACT_DISCLOSURE: &str = "A2A item retraction: ownership is \
     credential-scoped, not peer-scoped — every caller presenting the same API key is one \
     principal. An item a rustain sender tagged with its per-item capability can be retracted \
     only with that capability; for items without one, where peers share a key any of them can \
     retract items another sent. Keep `server.admission` at \"deny\" (the default) on a \
     shared-key deployment.";

/// Story 19.16d `P7` (owner answer 5 = A: accept, state, file, WARN — ⛔ never
/// refuse). The same design seen from the other side: rotation to a distinct
/// key mints a new principal.
const KEY_ROTATION_ORPHANS_RETRACT_WARNING: &str = "A2A item retraction: more than one API key \
     is configured, and an item belongs to the key it was sent with — a peer that rotates to \
     another key can no longer retract the items it sent under the old one.";

/// The startup lines `run_inner` emits for the retract's ownership posture.
/// Pure, so the posture → disclosure mapping is provable without a socket.
/// Story 19.18d `A18`: only **unbound** keys count — the credential-scoped
/// line with at least one, the rotation warning with two distinct values.
fn retract_ownership_disclosures(auth: Option<&A2aServerAuth>) -> Vec<&'static str> {
    let unbound = auth.map_or(0, |auth| auth.keys().distinct_unbound_count());
    let mut lines = Vec::new();
    if unbound >= 1 {
        lines.push(CREDENTIAL_SCOPED_RETRACT_DISCLOSURE);
    }
    if unbound >= 2 {
        lines.push(KEY_ROTATION_ORPHANS_RETRACT_WARNING);
    }
    lines
}

/// Story 19.18 `AC12(c)` (`P11(iii)`) — the thread posture, stated at
/// startup whenever API keys are configured: off by default, or on with the
/// same co-holder caveat the retract disclosure carries. ⛔ A legibility
/// statement, never a security claim. Story 19.18c `AC9(a)`: an item's
/// capability, not the setting, decides for items that carry one.
const THREAD_ACCESS_OFF_DISCLOSURE: &str = "A2A threads: an API-key caller is served a thread \
     read or follow-up reference only with the item's capability, which only its sender can \
     derive; items without one (from an older or non-rustain sender) are refused to API-key \
     callers (server.threadReads is off, the default). Loopback callers are served.";

const THREAD_ACCESS_CREDENTIAL_SCOPED_DISCLOSURE: &str = "A2A threads: items that carry a \
     capability still require it; items without one are served to every API-key caller \
     (server.threadReads = \"credential-scoped\"), so where peers share a key any of them can \
     read and add to such a thread. This setting asserts each peer has its own key.";

/// Story 19.18 `AC12(a)` — an unrecognised `threadReads` value fails closed.
const THREAD_ACCESS_UNKNOWN_WARNING: &str = "A2A threads: server.threadReads names a value this \
     build does not recognise; thread access for API-key callers is treated as off.";

/// The startup lines for the thread posture. Pure, like
/// [`retract_ownership_disclosures`], so the mapping is provable without a
/// socket. Story 19.18d `A18`: the posture line only while an unbound key is
/// configured (a bound key meets no `threadReads` gate).
fn thread_access_disclosures(
    auth: Option<&A2aServerAuth>,
    thread_reads: ThreadReads,
) -> Vec<&'static str> {
    let mut lines = Vec::new();
    if thread_reads == ThreadReads::Unknown {
        lines.push(THREAD_ACCESS_UNKNOWN_WARNING);
    }
    if auth.is_some_and(|auth| auth.keys().distinct_unbound_count() >= 1) {
        lines.push(match thread_reads {
            ThreadReads::CredentialScoped => THREAD_ACCESS_CREDENTIAL_SCOPED_DISCLOSURE,
            ThreadReads::Off | ThreadReads::Unknown => THREAD_ACCESS_OFF_DISCLOSURE,
        });
    }
    lines
}

/// Story 19.18d `A18` — `server.peerKeys` on a loopback listener has no
/// effect: `authenticate` answers every loopback caller as one local
/// principal before any key is read.
pub const PEER_BINDING_LOOPBACK_WARNING: &str = "A2A peer bindings: this server listens on \
     loopback, where every caller is one local principal; server.peerKeys takes effect only on \
     a non-loopback listener.";

/// The startup lines for the listener's peer-binding posture (Story 19.18d
/// `A18`). Pure, so the mapping is provable without a socket.
fn peer_binding_disclosures(auth: Option<&A2aServerAuth>, loopback: bool) -> Vec<&'static str> {
    if loopback && auth.is_some_and(|auth| auth.keys().has_bindings()) {
        vec![PEER_BINDING_LOOPBACK_WARNING]
    } else {
        Vec::new()
    }
}

/// Story 19.18d `A12`/`P23` — emitted once per startup when the startup fold
/// finds recipient items no caller of this listener can act as. `{count}` is
/// replaced with that number.
pub const ORPHANED_RECIPIENT_ITEMS_WARNING: &str = "A2A recipient items: {count} item(s) belong \
     to identities that no caller of this listener has, so no caller can list, read or retract \
     them; they stay in the journal. Off loopback, configuring the original API key again \
     (unbound, or in server.peerKeys with adoptPreBinding) or binding any key to the same \
     pinned peer makes them reachable.";

/// Story 19.18d `A12`/`P10`/`P23` — how many projected recipient items (each
/// counted once, Removed and retracted included; never reply turns or act
/// rows) no caller of this listener can reach:
///
/// - an API-key pseudonym item: on loopback always (keys are ignored there);
///   off loopback unless its key is configured unbound or adopted by a
///   binding. The loopback pseudonym is never counted — this warning is about
///   API-key items.
/// - a bound item: on loopback always; off loopback unless some configured
///   slot is bound to its pin.
///
/// RAP and unknown-provenance items are another rail's and never counted.
fn orphaned_recipient_items(
    projection: &JournalRecipientItemProjection,
    auth: Option<&A2aServerAuth>,
    loopback: bool,
) -> usize {
    let loopback_principal = SubmitterKey::loopback().principal();
    let (reachable_pseudonyms, bound_pins) = match auth.filter(|_| !loopback) {
        Some(auth) => {
            let keys = auth.keys();
            let bound_pins = keys.bound_pins();
            let reachable = keys
                .unbound_submitters()
                .into_iter()
                .chain(bound_pins.iter().flat_map(|pin| keys.adopted_for(pin)))
                .map(|submitter| submitter.principal())
                .collect::<Vec<_>>();
            (reachable, bound_pins)
        }
        None => (Vec::new(), BTreeSet::new()),
    };
    projection
        .snapshot()
        .items
        .keys()
        .filter(|address| match address.principal() {
            principal @ ItemPrincipal::A2aPseudonym(_) => {
                *principal != loopback_principal && !reachable_pseudonyms.contains(principal)
            }
            ItemPrincipal::A2aBound(pin) => !bound_pins.contains(pin),
            ItemPrincipal::Rap(_) | ItemPrincipal::Unknown => false,
        })
        .count()
}

async fn run_inner(
    addr: String,
    app_config: AppConfig,
    workspace: PathBuf,
    runtime: Option<Arc<dyn InboundPeerRuntime>>,
    transparency: Arc<TransparencySink>,
    roster: &[A2aPeerSpec],
    ready: &mut Option<oneshot::Sender<Result<(), String>>>,
) -> anyhow::Result<()> {
    let server_config = super::config::parse_workspace_a2a_server_config(
        &crate::infrastructure::paths::workspace_a2a_config_path(&workspace),
    )?;
    let policy = server_config
        .as_ref()
        .map(|config| config.admission)
        .unwrap_or_default();
    let advertised_host = server_config
        .as_ref()
        .and_then(|config| config.advertised_host.clone());

    // Story 19.18d `A16` — the one production resolver: names → roster
    // validation (`R1`–`R5`) → values → digest slots. `env_var_trimmed` is the
    // project's single env-read chokepoint; it also strips the trailing
    // newline a `export KEY=$(cat …)` leaves behind, which would otherwise
    // make every request fail the comparison for a reason no log line could
    // explain.
    let auth = match server_config.as_ref() {
        Some(config) => super::auth::resolve_server_auth(
            config,
            roster,
            crate::infrastructure::utils::env_var_trimmed,
        )?,
        None => None,
    };
    for line in retract_ownership_disclosures(auth.as_ref()) {
        tracing::warn!("{line}");
    }
    let thread_reads = server_config
        .as_ref()
        .and_then(|config| config.thread_reads)
        .unwrap_or_default();
    for line in thread_access_disclosures(auth.as_ref(), thread_reads) {
        tracing::warn!("{line}");
    }
    let tls = match server_config
        .as_ref()
        .and_then(|config| config.tls.as_ref())
    {
        Some(tls) => {
            let cert = workspace.join(&tls.cert);
            let key = workspace.join(&tls.key);
            Some(
                tokio::task::spawn_blocking(move || super::tls::load_tls_material(&cert, &key))
                    .await??,
            )
        }
        None => None,
    };
    let security = A2aServerSecurity {
        tls,
        auth,
        thread_reads,
    };

    let signer =
        IdentityKeyStore::new(crate::infrastructure::paths::data_dir()?).load_or_generate()?;
    match evaluate_bind_safety(&addr, BindEvidence::from_security(&security, true)) {
        BindDecision::Bind => {}
        BindDecision::RefuseWithReason(reason) => anyhow::bail!(reason),
    }

    let registry = Arc::new(CapabilityRegistry::new(None));
    // `SkillRegistry::discover` is blocking filesystem I/O and documents that
    // tokio callers MUST offload it (skill_registry.rs:79-81), as
    // event_loop.rs:813 and cli/catalog/mod.rs:119 already do.
    let discovered = {
        let home = dirs::home_dir();
        let disabled = app_config.skills.disabled.clone();
        let workspace = workspace.clone();
        tokio::task::spawn_blocking(move || {
            crate::adapters::skill_registry::SkillRegistry::discover(
                &workspace,
                home.as_deref(),
                &disabled,
            )
        })
        .await?
    };
    let activator = Arc::new(
        crate::adapters::skill_activation::SkillActivator::with_registry(Arc::new(
            tokio::sync::RwLock::new(discovered),
        )),
    );
    let provider = crate::adapters::skill_provider::SkillsProvider::new(activator);
    let _registrations = registry
        .discover_and_register_all(&provider, "skill")
        .await?;

    // `evaluate_bind_safety` cleared the *string*. Resolve it here, in the effect
    // shell, and bind a concrete address we have checked — binding the raw
    // hostname would let resolution disagree with the decision. `serve`
    // re-checks `local_addr` as the last line of defence.
    let candidates: Vec<std::net::SocketAddr> = tokio::net::lookup_host(&addr).await?.collect();
    anyhow::ensure!(
        !candidates.is_empty(),
        "A2A bind address {addr} resolved to no socket address"
    );
    let bind_addr = if security.is_network_ready() {
        candidates[0]
    } else {
        candidates
            .iter()
            .copied()
            .find(|candidate| candidate.ip().to_canonical().is_loopback())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "A2A bind address {addr} resolves only to non-loopback addresses \
                     ({candidates:?}); non-loopback serving requires TLS and API-key \
                     authentication (configure `server.tls` and `server.api_key_env`)"
                )
            })?
    };
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    let bound = listener.local_addr()?;
    let loopback = bound.ip().to_canonical().is_loopback();
    anyhow::ensure!(
        loopback || security.is_network_ready(),
        "refusing to serve A2A on non-loopback address {bound}: non-loopback serving requires \
         TLS and API-key authentication together"
    );
    anyhow::ensure!(
        !bound.ip().is_unspecified()
            || advertised_host
                .as_deref()
                .map(str::trim)
                .is_some_and(|host| !host.is_empty()),
        "refusing to serve A2A on wildcard address {bound} without `server.advertised_host`; \
         configure a client-reachable advertised_host in .rustain/a2a.json"
    );
    for line in peer_binding_disclosures(security.auth.as_ref(), loopback) {
        tracing::warn!("{line}");
    }

    // The listener exists and every pre-serve safety gate has passed. The daemon
    // may now publish readiness; later serving errors belong to its supervised
    // listener rather than a false successful startup.
    if let Some(sender) = ready.take() {
        let _ = sender.send(Ok(()));
    }

    let cancel = CancellationToken::new();
    let server = serve(
        listener,
        ServeConfig {
            registry,
            signer,
            security,
            runtime,
            transparency,
            policy,
            workspace,
            advertised_host,
            cards: Arc::new(SignedCardCache::new()),
        },
        cancel.child_token(),
    );
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => result,
        signal = tokio::signal::ctrl_c() => {
            signal?;
            cancel.cancel();
            server.await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn thread_status_fixture() -> (tempfile::TempDir, ServerState) {
        let workspace = tempfile::tempdir().unwrap();
        let signer = IdentityKeyStore::new(workspace.path())
            .load_or_generate()
            .unwrap();
        let state = ServerState {
            registry: Arc::new(CapabilityRegistry::new(None)),
            signer: Arc::new(signer),
            endpoint_url: Arc::from("http://localhost"),
            security: A2aServerSecurity::default(),
            runtime: None,
            tasks: Arc::new(InboundTaskStore::default()),
            cards: Arc::new(SignedCardCache::new()),
            transparency: Arc::new(TransparencySink::inert()),
            recipient_items: Arc::new(JournalRecipientItemProjection::default()),
            recipient_items_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            recipient_items_fold: Arc::new(tokio::sync::Mutex::new(())),
            item_allocator: Arc::new(RecipientItemAllocator::from_addresses(std::iter::empty())),
            policy: A2aAdmissionPolicy::Deny,
            workspace: Arc::new(workspace.path().to_path_buf()),
            scrub_fragments: vec!["private-host-fragment".to_owned()],
            pending_tasks: Arc::new(Mutex::new(())),
            requires_credential: false,
        };
        (workspace, state)
    }

    fn thread_status_reply(
        kind: crate::domain::models::ReplyKind,
        status: Option<crate::domain::models::StatusStatement>,
        content: &str,
    ) -> ThreadTurn {
        use crate::domain::models::{ItemAddress, ItemId, ReplyTurnId, ReplyTurnView, TurnAddress};

        let item = ItemAddress::from_a2a_ingress(
            PeerId::from_public_key(&[31; 32]).unwrap(),
            ItemId::from_replay("ri_status"),
        );
        ThreadTurn::Reply(ReplyTurnView {
            address: TurnAddress::answering(&item, ReplyTurnId::from_replay("rr_status")),
            answers: item.item().clone(),
            content: content.to_owned(),
            authorship: Authorship::HumanWritten,
            kind,
            status,
            via_node: None,
            journal_order: 1,
        })
    }

    #[test]
    fn thread_status_serves_only_canonical_statement_and_nothing() {
        use crate::domain::models::{ReplyKind, StatusStatement};

        let (_workspace, state) = thread_status_fixture();
        for wire in [
            serde_json::json!({"level":"minimal","stories":[{"key":"S7-03"}]}),
            serde_json::json!({
                "level":"status",
                "stories":[
                    {"key":"S7-03","state":"received"},
                    {"key":"S7-04","state":"acknowledged"}
                ]
            }),
            serde_json::json!({"stories":[]}),
        ] {
            let statement = StatusStatement::from_wire(&wire).unwrap();
            let turn = thread_status_reply(ReplyKind::Status, Some(statement), "Shared status");
            let value = thread_turn_json(&state, 2, &turn);
            assert_eq!(value["replyKind"], "status");
            assert_eq!(value["status"], wire);
            assert_eq!(value["content"], "Shared status");
            let object = value.as_object().unwrap();
            assert_eq!(object.len(), 8);
            assert!(!object.contains_key("sendAuthorization"));
            assert!(!object.contains_key("nothing_because"));
        }
    }

    #[test]
    fn thread_status_is_withheld_with_content_at_every_boundary() {
        use crate::domain::models::{ReplyKind, StatusStatement};

        let (_workspace, state) = thread_status_fixture();
        for content in [
            "private-host-fragment".to_owned(),
            state.workspace.to_string_lossy().into_owned(),
        ] {
            let turn = thread_status_reply(
                ReplyKind::Status,
                Some(StatusStatement::nothing()),
                &content,
            );
            let value = thread_turn_json(&state, 0, &turn);
            assert_eq!(value["replyKind"], "status");
            assert_eq!(value["contentWithheld"], true);
            assert!(value.get("content").is_none());
            assert!(value.get("status").is_none());
            assert!(!value.to_string().contains(&content));
        }
        // The outer size boundary uses the same helper after projection.
        let turn = thread_status_reply(
            ReplyKind::Status,
            Some(StatusStatement::nothing()),
            "Shared status",
        );
        let mut value = thread_turn_json(&state, 0, &turn);
        withhold_turn_content(&mut value);
        assert_eq!(value["contentWithheld"], true);
        assert!(value.get("content").is_none());
        assert!(value.get("status").is_none());
    }

    #[test]
    fn thread_status_preserves_legacy_answer_and_defer_fields() {
        use crate::domain::models::ReplyKind;

        let (_workspace, state) = thread_status_fixture();
        for kind in [ReplyKind::Answer, ReplyKind::Defer { minutes: 30 }] {
            let turn = thread_status_reply(kind, None, "Legacy reply");
            let value = thread_turn_json(&state, 0, &turn);
            let mut expected = serde_json::json!({
                "turnId":"rr_status",
                "kind":"reply",
                "inReplyTo":"ri_status",
                "authorship":"human-written",
                "ordinal":0,
                "content":"Legacy reply"
            });
            if let ReplyKind::Defer { minutes } = kind {
                expected["replyKind"] = serde_json::json!("defer");
                expected["deferMinutes"] = serde_json::json!(minutes);
            }
            assert_eq!(value, expected);
        }
        let turn = thread_status_reply(ReplyKind::Status, None, "No readable statement");
        let value = thread_turn_json(&state, 0, &turn);
        assert_eq!(value["replyKind"], "status");
        assert!(value.get("status").is_none());
    }

    #[test]
    fn a2a_version_resolution_obeys_source_order_and_numeric_components() {
        use axum::http::HeaderValue;

        let cases = [
            (None, None, true),
            (Some(""), None, true),
            (Some("  "), None, true),
            (Some("0.3"), None, true),
            (Some("0.3.0"), None, true),
            (Some("0.3.9"), None, true),
            (Some(" 0.3 "), None, true),
            (Some("00.3"), None, true),
            (Some("0.03"), None, true),
            (Some("0.3.999999999999999999999999"), None, true),
            (Some("99999999999.3"), None, false),
            (Some("0.99999999999"), None, false),
            (Some("v0.3"), None, false),
            (Some("0.3-rc1"), None, false),
            (Some("0.3."), None, false),
            (Some(".3"), None, false),
            (Some("1"), None, false),
            (Some("1.0"), None, false),
            (Some("0.4"), None, false),
            (Some("0.30"), None, false),
            (Some("0.3.0.1"), None, false),
            (Some("+0.3"), None, false),
            (Some("0.3.+1"), None, false),
            (Some("0.3.９"), None, false),
            (None, Some("a2a-version=1.0"), false),
            (None, Some("A2A-Version=0%2E3"), true),
            (None, Some("A2A-Version=0.3&A2A-Version=1.0"), true),
            (None, Some("A2A-Version=1.0&A2A-Version=0.3"), false),
            (None, Some("A2A-Version=&A2A-Version=1.0"), true),
            (None, Some("other=1.0&A2A-Version=+0.3+"), true),
            (Some(""), Some("A2A-Version=1.0"), false),
            (Some("  "), Some("A2A-Version=1.0"), false),
            (Some("0.3"), Some("A2A-Version=1.0"), true),
            (Some("1.0"), Some("A2A-Version=0.3"), false),
        ];
        for (header, query, expected) in cases {
            let header = header.map(|value| HeaderValue::from_bytes(value.as_bytes()).unwrap());
            assert_eq!(
                a2a_version_supported(header.as_ref(), query),
                expected,
                "header={header:?}, query={query:?}"
            );
        }
        let non_utf8 = HeaderValue::from_bytes(b"\xff").unwrap();
        assert!(!a2a_version_supported(Some(&non_utf8), None));
        assert!(!a2a_version_supported(
            Some(&non_utf8),
            Some("A2A-Version=0.3")
        ));
        // Valid UTF-8 is not enough: NBSP is not HTTP whitespace.
        let nbsp_wrapped = HeaderValue::from_bytes("\u{a0}0.3\u{a0}".as_bytes()).unwrap();
        assert!(!a2a_version_supported(Some(&nbsp_wrapped), None));
        assert!(!a2a_version_supported(
            Some(&nbsp_wrapped),
            Some("A2A-Version=0.3")
        ));

        let mut headers = HeaderMap::new();
        headers.append("a2a-version", HeaderValue::from_static("0.3"));
        headers.append("a2a-version", HeaderValue::from_static("1.0"));
        assert!(a2a_version_supported(headers.get("a2a-version"), None));
    }

    fn body(raw: &str) -> Bytes {
        Bytes::from(raw.to_owned())
    }

    #[test]
    fn an_absent_id_is_a_notification_and_an_explicit_null_id_is_refused() {
        // R8: the previous cut defaulted an absent `id` to `Value::Null`, which
        // made these two indistinguishable.
        let notification = parse_envelope(&body(r#"{"jsonrpc":"2.0","method":"tasks/get"}"#))
            .expect("absent id is a notification");
        assert!(matches!(notification, Envelope::Notification { .. }));

        let error = parse_envelope(&body(r#"{"jsonrpc":"2.0","id":null,"method":"tasks/get"}"#))
            .err()
            .expect("explicit null id is refused");
        let rendered = serde_json::to_string(&error).unwrap();
        assert!(rendered.contains("explicit null JSON-RPC id"), "{rendered}");
    }

    #[test]
    fn a_batch_is_refused_with_a_message_naming_the_profile() {
        let error = parse_envelope(&body(r#"[{"jsonrpc":"2.0","id":1,"method":"tasks/get"}]"#))
            .err()
            .expect("batch is refused");
        let rendered = serde_json::to_string(&error).unwrap();
        assert!(
            rendered.contains("batch requests are not supported"),
            "{rendered}"
        );
        assert!(
            rendered.contains(&CODE_INVALID_REQUEST.to_string()),
            "{rendered}"
        );
    }

    #[test]
    fn string_and_number_ids_are_accepted_and_echoed_verbatim() {
        for raw in [
            r#"{"jsonrpc":"2.0","id":7,"method":"tasks/get"}"#,
            r#"{"jsonrpc":"2.0","id":"abc","method":"tasks/get"}"#,
        ] {
            let Envelope::Call { id, method, .. } = parse_envelope(&body(raw)).expect(raw) else {
                panic!("expected a call for {raw}");
            };
            assert_eq!(method, "tasks/get");
            assert!(id.is_number() || id.is_string());
        }
    }

    #[test]
    fn a_non_scalar_id_and_a_wrong_version_are_invalid_requests() {
        for raw in [
            r#"{"jsonrpc":"2.0","id":{"a":1},"method":"tasks/get"}"#,
            r#"{"jsonrpc":"1.0","id":1,"method":"tasks/get"}"#,
            r#"{"jsonrpc":"2.0","id":1,"method":"  "}"#,
            r#"{"jsonrpc":"2.0","id":1}"#,
        ] {
            assert!(parse_envelope(&body(raw)).is_err(), "{raw}");
        }
    }

    #[test]
    fn message_text_is_extracted_from_text_parts_only() {
        let message = serde_json::json!({
            "role": "user",
            "parts": [
                { "kind": "text", "text": "summarize" },
                { "kind": "file", "file": { "uri": "file:///etc/passwd" } },
                { "kind": "text", "text": "the corpus" },
            ],
        });
        assert_eq!(
            well_formed_message(Some(&message)).as_deref(),
            Some("summarize\nthe corpus")
        );
        // A file part's URI is NOT smuggled into the instruction text.
        assert!(
            !well_formed_message(Some(&message))
                .unwrap()
                .contains("passwd")
        );
    }

    #[test]
    fn a_malformed_message_is_rejected_rather_than_fabricated_into_a_task() {
        for message in [
            serde_json::json!({}),
            serde_json::json!({ "role": "user" }),
            serde_json::json!({ "role": "", "parts": [{ "kind": "text", "text": "x" }] }),
            serde_json::json!({ "role": "user", "parts": [] }),
            serde_json::json!({ "role": "user", "parts": ["nope"] }),
            serde_json::json!({ "role": "user", "parts": [{ "kind": "file", "file": {} }] }),
            serde_json::json!({ "role": "user", "parts": [{ "kind": "text", "text": 7 }] }),
            serde_json::json!({ "role": "user", "parts": [{ "kind": "text", "text": "  " }] }),
        ] {
            assert!(well_formed_message(Some(&message)).is_none(), "{message}");
        }
        assert!(well_formed_message(None).is_none());
    }

    /// Story 19.16d AC2(c) — the collapse disclosure's one rule, with both
    /// `SubmitterTrust` values (`P13.3`/`Q5`: the in-crate positive control
    /// for `M09`). The wire can only ever produce `Loopback`; this is where the
    /// `ApiKey` branch is proven to answer `false`, so the flag discriminates
    /// rather than saying "collapsed" to everyone.
    #[test]
    fn the_collapse_disclosure_is_present_on_loopback_and_absent_behind_a_key() {
        assert!(principal_collapsed(SubmitterTrust::Loopback));
        assert!(!principal_collapsed(SubmitterTrust::ApiKey));
        assert!(!principal_collapsed(SubmitterTrust::BoundApiKey));
    }

    /// Story 19.18d `A8` — the bound flag's one rule over every trust.
    #[test]
    fn only_a_bound_key_is_flagged_principal_bound() {
        assert!(principal_bound(SubmitterTrust::BoundApiKey));
        assert!(!principal_bound(SubmitterTrust::ApiKey));
        assert!(!principal_bound(SubmitterTrust::Loopback));
    }

    /// Story 19.18d `K10` (lib table) — written `a2a.json` → both production
    /// parsers → the production resolver → the startup mappers: disclosures
    /// count only unbound keys, a bound-only set is network-ready, and the
    /// loopback line names an ineffective binding. `M19` (count every key)
    /// turns the bound-only rows RED.
    #[test]
    fn k10_startup_disclosures_count_only_unbound_keys() {
        use base64::Engine as _;
        let dir = tempfile::tempdir().unwrap();
        let resolve = |server: serde_json::Value| {
            let path = dir.path().join("a2a.json");
            std::fs::write(
                &path,
                serde_json::json!({
                    "agents": {
                        "p1": {
                            "url": "https://p1.example/",
                            "pinnedKey": {
                                "alg": "EdDSA",
                                "x": base64::engine::general_purpose::URL_SAFE_NO_PAD
                                    .encode([41_u8; 32]),
                            },
                        },
                    },
                    "server": server,
                })
                .to_string(),
            )
            .unwrap();
            let roster = super::super::config::parse_workspace_a2a_config(&path).unwrap();
            let config = super::super::config::parse_workspace_a2a_server_config(&path)
                .unwrap()
                .unwrap();
            super::super::auth::resolve_server_auth(&config, &roster, |name| {
                Some(format!("value-of-{name}"))
            })
            .unwrap()
            .unwrap()
        };
        let bound_only = resolve(serde_json::json!({ "peerKeys": { "K_A": "p1", "K_B": "p1" } }));
        assert!(retract_ownership_disclosures(Some(&bound_only)).is_empty());
        assert!(thread_access_disclosures(Some(&bound_only), ThreadReads::Off).is_empty());
        assert!(
            BindEvidence::from_security(
                &A2aServerSecurity {
                    auth: Some(bound_only.clone()),
                    ..A2aServerSecurity::default()
                },
                true
            )
            .api_key_auth,
            "a bound-only key set is network-ready (`M24`)"
        );
        let one_unbound = resolve(serde_json::json!({
            "peerKeys": { "K_A": "p1" },
            "apiKeys": ["K_U"],
        }));
        assert_eq!(
            retract_ownership_disclosures(Some(&one_unbound)),
            vec![CREDENTIAL_SCOPED_RETRACT_DISCLOSURE]
        );
        assert_eq!(
            thread_access_disclosures(Some(&one_unbound), ThreadReads::Off),
            vec![THREAD_ACCESS_OFF_DISCLOSURE]
        );
        let two_unbound = resolve(serde_json::json!({
            "peerKeys": { "K_A": "p1" },
            "apiKeys": ["K_U", "K_V"],
        }));
        assert_eq!(
            retract_ownership_disclosures(Some(&two_unbound)),
            vec![
                CREDENTIAL_SCOPED_RETRACT_DISCLOSURE,
                KEY_ROTATION_ORPHANS_RETRACT_WARNING
            ]
        );
        assert_eq!(
            peer_binding_disclosures(Some(&bound_only), true),
            vec![PEER_BINDING_LOOPBACK_WARNING]
        );
        assert!(peer_binding_disclosures(Some(&bound_only), false).is_empty());
        let unbound_only = resolve(serde_json::json!({ "apiKeys": ["K_U"] }));
        assert!(peer_binding_disclosures(Some(&unbound_only), true).is_empty());
        for line in [
            PEER_BINDING_LOOPBACK_WARNING,
            ORPHANED_RECIPIENT_ITEMS_WARNING,
        ] {
            let lower = line.to_ascii_lowercase();
            for banned in [
                "authenticated",
                "tamper",
                "signed",
                "audit",
                "proof",
                "evidence",
                "verified",
                "attested",
            ] {
                assert!(!lower.contains(banned), "{banned:?} in {line:?}");
            }
        }
    }

    /// Story 19.18c `AC3(c)`/`A6` — the pending record carries the capability
    /// HASH only; a record without it (pre-19.18c) replays as legacy and a
    /// legacy record serializes exactly as before.
    #[test]
    fn a_pending_record_keeps_the_capability_hash_and_stays_backward_compatible() {
        let legacy: PendingTaskRecord =
            serde_json::from_str(r#"{"task_id":"t","submitter":"apikey:x"}"#).expect("legacy");
        assert_eq!(legacy.capability_hash, None);
        assert_eq!(
            serde_json::to_string(&legacy).expect("serialize"),
            r#"{"task_id":"t","submitter":"apikey:x"}"#
        );
        let hashed = PendingTaskRecord {
            capability_hash: Some("ab".repeat(32)),
            ..legacy
        };
        let encoded = serde_json::to_string(&hashed).expect("serialize");
        let replayed: PendingTaskRecord = serde_json::from_str(&encoded).expect("replay");
        assert_eq!(replayed.capability_hash, hashed.capability_hash);
    }

    /// Story 19.18 `AC12(c)`/`K21(4)` — the thread posture lines: an unknown
    /// value always warns and is treated as off; with keys configured the
    /// posture is stated either way.
    #[test]
    fn the_thread_access_disclosure_follows_the_configured_posture() {
        let keys =
            A2aServerAuth::ApiKey(super::super::auth::ApiKeySet::unbound(vec!["key-0".into()]));
        assert!(thread_access_disclosures(None, ThreadReads::Off).is_empty());
        assert_eq!(
            thread_access_disclosures(None, ThreadReads::Unknown),
            vec![THREAD_ACCESS_UNKNOWN_WARNING]
        );
        assert_eq!(
            thread_access_disclosures(Some(&keys), ThreadReads::Off),
            vec![THREAD_ACCESS_OFF_DISCLOSURE]
        );
        assert_eq!(
            thread_access_disclosures(Some(&keys), ThreadReads::Unknown),
            vec![THREAD_ACCESS_UNKNOWN_WARNING, THREAD_ACCESS_OFF_DISCLOSURE]
        );
        assert_eq!(
            thread_access_disclosures(Some(&keys), ThreadReads::CredentialScoped),
            vec![THREAD_ACCESS_CREDENTIAL_SCOPED_DISCLOSURE]
        );
        for line in [
            THREAD_ACCESS_OFF_DISCLOSURE,
            THREAD_ACCESS_CREDENTIAL_SCOPED_DISCLOSURE,
            THREAD_ACCESS_UNKNOWN_WARNING,
        ] {
            let lower = line.to_ascii_lowercase();
            for banned in [
                "authenticated",
                "tamper",
                "signed",
                "audit",
                "proof",
                "evidence",
            ] {
                assert!(!lower.contains(banned), "{banned:?} in {line:?}");
            }
        }
    }

    /// Story 19.16d `Q1` + owner answer 5 — the startup posture lines: none
    /// without inbound keys, the credential-scoping disclosure with any key,
    /// and the rotation warning only when more than one distinct key value is
    /// configured (⛔ a warning, never a refusal). ⛔ The wording ceiling binds
    /// both.
    #[test]
    fn the_retract_ownership_disclosure_follows_the_configured_key_set() {
        let keys = |count: usize| {
            A2aServerAuth::ApiKey(super::super::auth::ApiKeySet::unbound(
                (0..count)
                    .map(|index| format!("key-{index}").into())
                    .collect(),
            ))
        };
        assert!(retract_ownership_disclosures(None).is_empty());
        assert_eq!(
            retract_ownership_disclosures(Some(&keys(1))),
            vec![CREDENTIAL_SCOPED_RETRACT_DISCLOSURE]
        );
        let duplicate_names_same_value =
            A2aServerAuth::ApiKey(super::super::auth::ApiKeySet::unbound(vec![
                "same-key".into(),
                "same-key".into(),
            ]));
        assert_eq!(
            retract_ownership_disclosures(Some(&duplicate_names_same_value)),
            vec![CREDENTIAL_SCOPED_RETRACT_DISCLOSURE],
            "two configured names resolving to one credential do not create a rotation boundary"
        );
        assert_eq!(
            retract_ownership_disclosures(Some(&keys(2))),
            vec![
                CREDENTIAL_SCOPED_RETRACT_DISCLOSURE,
                KEY_ROTATION_ORPHANS_RETRACT_WARNING
            ]
        );
        for line in [
            CREDENTIAL_SCOPED_RETRACT_DISCLOSURE,
            KEY_ROTATION_ORPHANS_RETRACT_WARNING,
        ] {
            let lower = line.to_ascii_lowercase();
            for banned in [
                "authenticated",
                "tamper",
                "signed",
                "audit",
                "proof",
                "evidence",
            ] {
                assert!(!lower.contains(banned), "{banned:?} in {line:?}");
            }
        }
    }
}
