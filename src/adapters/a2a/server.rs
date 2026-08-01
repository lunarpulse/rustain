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
//!             └─ tasks/cancel  ─▶ submitter-scoped lookup ─▶ CancellationToken
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
//! * **An explicit `"id": null`** is rejected with `-32600`. It is legal but
//!   discouraged, and accepting it would make a notification and a null-id call
//!   indistinguishable in every response we emit.
//! * **Batch arrays** are rejected with a single `-32600` naming the profile.
//!
//! The same three statements appear in the ADR, and
//! `the_declared_jsonrpc_profile_matches_the_adr` fails if they drift apart.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Extension, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, oneshot};
use tokio_util::sync::CancellationToken;

use crate::adapters::rap::{AgentSigner, IdentityKeyStore};
use crate::domain::models::{AppConfig, CapabilityRegistry, PeerId, RapTaskState};
use crate::domain::ports::{InboundApprovalTicket, InboundPeerRuntime, InboundPeerTask};

use super::admission::{
    A2aAdmissionPolicy, AdmissionRequest, AdmissionVerdict, SubmitterTrust, admit,
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
use super::jsonrpc::{
    CODE_INTERNAL_ERROR, CODE_INVALID_PARAMS, CODE_INVALID_REQUEST, CODE_METHOD_NOT_FOUND,
    CODE_PARSE_ERROR, CODE_TASK_NOT_FOUND, JsonRpcErrorResponse, JsonRpcResponse,
};
use super::projection::{RemotePeerViewer, RoomProjection};
use super::transparency::{InboundOutcome, TransparencySink};

const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CANCEL_WAIT_TIMEOUT: Duration = Duration::from_secs(25);
const MAX_TASK_ID_BYTES: usize = 256;
const PENDING_TASK_FILE: &str = "a2a-pending.json";

/// The one message a non-owner or unknown task id ever receives.
const TASK_NOT_FOUND: &str = "Task not found";

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
    let state = ServerState {
        registry: config.registry,
        signer: Arc::new(config.signer),
        endpoint_url,
        runtime: config.runtime,
        tasks: Arc::new(InboundTaskStore::default()),
        cards: config.cards,
        transparency: config.transparency,
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
    reconcile_after_restart(&state).await;
    reconcile_pending_after_restart(&state).await;

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
async fn reconcile_after_restart(state: &ServerState) {
    let Some(runtime) = state.runtime.as_ref() else {
        return;
    };
    for node_id in runtime
        .reconcile_orphaned_tasks(INBOUND_SUBAGENT_TYPE)
        .await
    {
        let Some((submitter, task_id)) = parse_inbound_node_id(&node_id) else {
            continue;
        };
        let peer_id = submitter.pseudonymous_peer_id();
        let Some(task) = state
            .tasks
            .insert(
                task_id.clone(),
                node_id.clone(),
                submitter,
                RapTaskState::Submitted,
            )
            .await
        else {
            continue;
        };
        if state
            .transparency
            .has_recorded_status_query(&peer_id, &task_id)
            .await
        {
            task.restore_status_query();
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

/// Rehydrate approval-pending tasks after their owning process disappeared.
async fn reconcile_pending_after_restart(state: &ServerState) {
    let path = pending_tasks_path(state);
    let records = match tokio::fs::read(&path).await {
        Ok(raw) => match serde_json::from_slice::<Vec<PendingTaskRecord>>(&raw) {
            Ok(records) => records,
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "skipping corrupt A2A pending task file");
                return;
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(path = %path.display(), "A2A pending task file is missing; no approval-pending tasks restored");
            return;
        }
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "skipping unreadable A2A pending task file");
            return;
        }
    };

    for record in &records {
        if record.task_id.is_empty()
            || record.task_id.len() > MAX_TASK_ID_BYTES
            || record.submitter.is_empty()
        {
            tracing::warn!(path = %path.display(), "skipping malformed A2A pending task record");
            continue;
        }
        let submitter = SubmitterKey::from_encoded(record.submitter.clone());
        let peer_id = submitter.pseudonymous_peer_id();
        let Some(task) = state
            .tasks
            .insert(
                record.task_id.clone(),
                mint_inbound_node_id(&submitter, &record.task_id),
                submitter,
                RapTaskState::Submitted,
            )
            .await
        else {
            continue;
        };
        if state
            .transparency
            .has_recorded_status_query(&peer_id, &record.task_id)
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
        AuthOutcome::Authenticated => Ok(Caller {
            key: SubmitterKey::from_api_key(presented.unwrap_or_default()),
            trust: SubmitterTrust::ApiKey,
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
    body: Bytes,
) -> Response {
    match parse_envelope(&body) {
        Err(response) => json_response(&response, StatusCode::OK),
        Ok(Envelope::Notification { method, params }) => {
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
    let peer_id = caller.key.pseudonymous_peer_id();

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
        AdmissionVerdict::Accept => start_task(state, caller, task_id, message, false)
            .await
            .map_err(|reason| JsonRpcErrorResponse::new(echo, CODE_INTERNAL_ERROR, &reason)),
        AdmissionVerdict::AcceptPendingApproval => {
            start_task(state, caller, task_id, message, true)
                .await
                .map_err(|reason| JsonRpcErrorResponse::new(echo, CODE_INTERNAL_ERROR, &reason))
        }
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
    needs_approval: bool,
) -> Result<serde_json::Value, String> {
    let runtime = state
        .runtime
        .clone()
        .ok_or_else(|| "no execution runtime".to_owned())?;
    let peer_id = caller.key.pseudonymous_peer_id();
    // Keyed by the SUBMITTER, so a restart can rebuild scoping from the tree.
    let node_id = mint_inbound_node_id(&caller.key, &task_id);
    let task = state
        .tasks
        .insert(
            task_id.clone(),
            node_id,
            caller.key.clone(),
            RapTaskState::Submitted,
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
            peer_id,
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
    peer_id: PeerId,
    needs_approval: bool,
) -> Result<serde_json::Value, String> {
    if needs_approval {
        let ticket = match runtime.request_admission_approval(&peer_id, &text).await {
            Ok(ticket) => ticket,
            Err(error) => {
                tracing::error!(%error, task = %task_id, "failed to request A2A admission approval");
                terminalize_start_failure(&state, &task, &peer_id, None).await;
                return task_projection(&state, &task, &peer_id).await;
            }
        };

        if ticket.pending {
            let pending = PendingTaskRecord {
                task_id: task_id.clone(),
                submitter: task.submitter.as_str().to_owned(),
            };
            // Persist the admission outcome before exposing AuthRequired.
            let _ = state
                .transparency
                .record(InboundOutcome::AwaitingApproval {
                    peer: peer_id.clone(),
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
                peer_id,
                pending,
                ticket,
            ));
            return Ok(projection_for(&state, &task_id, RapTaskState::AuthRequired, None).await);
        }

        // Policy resolved it without a human. Still race cancellation so a
        // request canceled during setup cannot be registered after the fact.
        let granted = tokio::select! {
            biased;
            _ = task.cancel.cancelled() => {
                terminalize_canceled(&state, &task, &peer_id, None).await;
                return task_projection(&state, &task, &peer_id).await;
            }
            decision = ticket.decision => decision.unwrap_or(false),
        };
        if !granted {
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
        terminalize_canceled(&state, &task, &peer_id, None).await;
        return task_projection(&state, &task, &peer_id).await;
    }
    launch(&state, &runtime, &task, text, peer_id.clone(), None).await;
    task_projection(&state, &task, &peer_id).await
}

async fn task_projection(
    state: &ServerState,
    task: &Arc<super::exec::InboundTask>,
    peer_id: &PeerId,
) -> Result<serde_json::Value, String> {
    let snapshot = task.snapshot().await;
    let disclosing_result = snapshot.result.is_some();
    let text = snapshot.result.as_deref().or(snapshot.detail.as_deref());
    let projection = projection_for(state, &task.id, snapshot.state, text).await;
    if disclosing_result
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
    peer_id: PeerId,
    pending: PendingTaskRecord,
    ticket: InboundApprovalTicket,
) {
    // Cancellation wins an already-ready grant too. Dropping the ticket receiver
    // on this branch withdraws this task's interest in the operator decision.
    let granted = tokio::select! {
        biased;
        _ = task.cancel.cancelled() => {
            terminalize_canceled(&state, &task, &peer_id, Some(&pending)).await;
            return;
        }
        decision = ticket.decision => decision.unwrap_or(false),
    };

    task.set_pending_auth(PendingAuth::None).await;
    if !granted {
        let reason = "the operator declined this task".to_owned();
        task.set_detail(reason.clone()).await;
        task.advance(RapTaskState::Rejected).await;
        let _ = state
            .transparency
            .record(InboundOutcome::Refused {
                peer: peer_id,
                task_id: task.id.clone(),
                reason,
            })
            .await;
        remove_pending_task(&state, &pending).await;
        return;
    }
    if task.cancel.is_cancelled() {
        terminalize_canceled(&state, &task, &peer_id, Some(&pending)).await;
        return;
    }
    launch(&state, &runtime, &task, text, peer_id, Some(pending)).await;
}

/// Record acceptance durably, then register the peer node, drive the turn, and
/// watch the node's lifecycle back onto the task's wire state.
async fn launch(
    state: &ServerState,
    runtime: &Arc<dyn InboundPeerRuntime>,
    task: &Arc<super::exec::InboundTask>,
    text: String,
    peer_id: PeerId,
    pending: Option<PendingTaskRecord>,
) {
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
        return;
    }
    task.advance(RapTaskState::Working).await;
    let response_policy = runtime.response_policy(&peer_id);
    let started = runtime
        .start(
            InboundPeerTask {
                node_id: task.node_id.clone(),
                peer_id: peer_id.clone(),
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
            return;
        }
    };
    let node_state = *status.borrow();
    let pending_auth = task.pending_auth().await;
    let initial = project_node_to_rap(node_state, pending_auth);
    task.advance(initial).await;
    if initial.is_terminal() {
        if initial == RapTaskState::Completed {
            task.set_result(runtime.take_result_text(&task.node_id).await)
                .await;
        } else if initial == RapTaskState::Failed {
            task.set_detail(FAILURE_DETAIL).await;
        }
        if let Some(record) = pending.as_ref() {
            remove_pending_task(state, record).await;
        }
        return;
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
                let result = runtime.take_result_text(&task.node_id).await;
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
                let _ = runtime.take_result_text(&task.node_id).await;
                task.advance(RapTaskState::Failed).await;
                if let Some(record) = pending.as_ref() {
                    remove_pending_task(&state, record).await;
                }
                break;
            }
        }
    });
}

// ── tasks/get, tasks/cancel ─────────────────────────────────────────────────

/// Look up a task **as** the calling credential.
///
/// A task that exists but belongs to another credential produces the byte-identical
/// response of a task that does not exist. See `exec::InboundTaskStore::get_scoped`
/// and the ADR for why the imprecision is deliberate.
async fn scoped_task(
    state: &ServerState,
    caller: &Caller,
    params: &serde_json::Value,
    echo: &serde_json::Value,
) -> Result<Arc<super::exec::InboundTask>, JsonRpcErrorResponse> {
    let id = params
        .get("id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            JsonRpcErrorResponse::new(echo.clone(), CODE_INVALID_PARAMS, "Invalid params")
        })?;
    state
        .tasks
        .get_scoped(id, &caller.key)
        .await
        .ok_or_else(|| JsonRpcErrorResponse::new(echo.clone(), CODE_TASK_NOT_FOUND, TASK_NOT_FOUND))
}

async fn tasks_get(
    state: &ServerState,
    caller: &Caller,
    params: &serde_json::Value,
    echo: serde_json::Value,
) -> Result<serde_json::Value, JsonRpcErrorResponse> {
    let task = scoped_task(state, caller, params, &echo).await?;
    // FR92 (P-5): journal the FIRST status query observed for a task and
    // nothing thereafter. The task-local state has an in-flight claim, not a
    // boolean: a failed append releases the claim so a later poll can retry
    // rather than permanently hiding the observation.
    if task.claim_status_query() {
        let recorded = state
            .transparency
            .record(InboundOutcome::StatusQueried {
                peer: caller.key.pseudonymous_peer_id(),
                task_id: task.id.clone(),
            })
            .await;
        if recorded.is_ok() {
            task.commit_status_query();
        } else {
            task.release_status_query();
        }
    }
    let peer_id = caller.key.pseudonymous_peer_id();
    task_projection(state, &task, &peer_id)
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
    let peer_id = caller.key.pseudonymous_peer_id();
    task_projection(state, &task, &peer_id)
        .await
        .map_err(|error| {
            tracing::error!(%error, task = %task.id, "withholding unjournaled A2A result");
            JsonRpcErrorResponse::new(echo, CODE_INTERNAL_ERROR, "Internal error")
        })
}

// ── projection helpers ──────────────────────────────────────────────────────

/// Every served task payload goes through the one redaction boundary.
async fn projection_for(
    state: &ServerState,
    task_id: &str,
    task_state: RapTaskState,
    text: Option<&str>,
) -> serde_json::Value {
    RoomProjection::<RemotePeerViewer>::disclose(
        task_id,
        task_state,
        text,
        state.workspace.as_path(),
        &state.scrub_fragments,
        state.signer.identity().peer_id.to_string(),
    )
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
/// path once it has built its core).
pub async fn run(
    addr: String,
    app_config: AppConfig,
    workspace: PathBuf,
    runtime: Option<Arc<dyn InboundPeerRuntime>>,
    transparency: Arc<TransparencySink>,
    ready: Option<oneshot::Sender<Result<(), String>>>,
) -> anyhow::Result<()> {
    let mut ready = ready;
    let result = run_inner(
        addr,
        app_config,
        workspace,
        runtime,
        transparency,
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

async fn run_inner(
    addr: String,
    app_config: AppConfig,
    workspace: PathBuf,
    runtime: Option<Arc<dyn InboundPeerRuntime>>,
    transparency: Arc<TransparencySink>,
    ready: &mut Option<oneshot::Sender<Result<(), String>>>,
) -> anyhow::Result<()> {
    let server_config = super::config::parse_workspace_a2a_server_config(
        &workspace.join(".rustain").join("a2a.json"),
    )?;
    let policy = server_config
        .as_ref()
        .map(|config| config.admission)
        .unwrap_or_default();
    let advertised_host = server_config
        .as_ref()
        .and_then(|config| config.advertised_host.clone());

    // The legacy name remains valid and the additional names are a set union:
    // duplicate env-var names do not create duplicate comparisons or principals.
    let mut key_names = BTreeSet::new();
    if let Some(config) = server_config.as_ref() {
        if let Some(name) = config
            .api_key_env
            .as_deref()
            .filter(|name| !name.is_empty())
        {
            key_names.insert(name.to_owned());
        }
        for name in config.api_keys.as_deref().unwrap_or_default() {
            if !name.is_empty() {
                key_names.insert(name.clone());
            }
        }
    }
    let auth = if key_names.is_empty() {
        None
    } else {
        let mut keys = Vec::with_capacity(key_names.len());
        for name in key_names {
            // `env_var_trimmed` is the project's single env-read chokepoint; it
            // also strips the trailing newline a `export KEY=$(cat …)` leaves
            // behind, which would otherwise make every request fail the
            // comparison for a reason no log line could explain.
            let key = crate::infrastructure::utils::env_var_trimmed(&name).ok_or_else(|| {
                anyhow::anyhow!(
                    "A2A API-key configuration names {name}, but that environment variable is \
                     unset or empty"
                )
            })?;
            keys.push(key.into());
        }
        Some(A2aServerAuth::ApiKey { keys })
    };
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
    let security = A2aServerSecurity { tls, auth };

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

    /// AC6b / R8: a *stated* narrow profile is interop-honest only while the
    /// statement and the code agree. This is the test that keeps them together.
    #[test]
    fn the_declared_jsonrpc_profile_matches_the_adr() {
        let adr = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../_bmad-output/planning-artifacts/architecture/adr/",
            "ADR-17-4a-01-a2a-agentcard-discovery-in-repo-adapter.md"
        ))
        .expect("ADR-17-4a-01 must exist");
        for claim in [
            "batch requests are not supported",
            "explicit null JSON-RPC id",
            "204 No Content",
            "non-owner",
            "RoomProjection",
        ] {
            assert!(
                adr.contains(claim),
                "ADR-17-4a-01 must state {claim:?} — the enforced profile and the ADR have drifted"
            );
        }
    }
}
