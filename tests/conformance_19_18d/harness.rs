//! Story 19.18d — the SHARED keystone harness (`F14`).
//!
//! Every 19.18d front-door keystone module builds on this file
//! (`crate::harness::…`). Rule 2 (`A21`) is enforced HERE, once:
//!
//! 1. ONE `<workspace>/.rustain/a2a.json` is written per host start: the roster
//!    under `agents` and the caller's `server` block (`serde_json::Value`).
//!    The harness only ADDS `server.tls` (a self-signed `localhost` certificate
//!    kept in the workspace) and `server.advertisedHost` (`localhost:{port}`)
//!    when the block does not already carry them — the shape an operator
//!    writes.
//! 2. The file is parsed by BOTH production parsers
//!    (`parse_workspace_a2a_config`, `parse_workspace_a2a_server_config`) and
//!    the key set is built by the ONE production resolver
//!    (`resolve_server_auth`) over a closure on the fixed table
//!    [`KEY_TABLE`] (env-var NAME → value). ⛔ Never a hand-built key set,
//!    never a constructed `KeyBinding`, never a direct `items_*` call.
//! 3. `serve(ServeConfig…)` runs on `0.0.0.0:0` (or, explicitly, a loopback
//!    listener) with rcgen TLS; clients are ANCHORED on that certificate
//!    (⛔ never `danger_accept_invalid_certs`) and may present any value in
//!    `x-api-key`.
//!
//! Sources, all COPIED (never imported — their helpers are file-private):
//! `conformance_19_18d0_a2a_typed_policy_principal.rs` `HostOptions`/`Host`/
//! `compose` (`:337-526`), `restart` (`:531-551`), `attach_operator`
//! (`:555-605`), `anchored` (`:750-771`), `answer_card` (`:913-938`),
//! `always_allow` (`:941-950`), `raw_payloads` (`:871-889`);
//! `conformance_19_18c_a2a_item_capability.rs` `LogBuffer`/`setup`
//! (`:1006-1066`), `tls_host` (`:1082-1133`), `Raw` (`:1149-1185`), the body
//! builders (`:1187-1224`), `Sender`/`new_sender`/`sender_over`/`app_state`/
//! `settle_send`/`read_thread_block` (`:624-875`), `waiting_node`/`pending_card`
//! (`:815-965`), `Item`/`items`/`send_item` (`:1256-1319`).
//!
//! # Quick reference
//!
//! * [`setup`] — call FIRST in every test.
//! * [`HostSpec`] (`b1()`, `b2()`, `with_server(..)`, `.mode()`, `.consent()`,
//!   `.interaction(..)`, `.listen(..)`, `.roster(..)`) → [`BoundHost::start`].
//! * [`BoundHost`]: `.client(key)` (anchored raw client), `.restart(spec)`,
//!   `.restart_as(spec, listen)`, `.attach_operator()`, `.core`,
//!   `.approval_events()`, `.journal`, `.received_rows()`, …
//! * [`RawClient`]: `message_send`, `tasks_get`, `tasks_cancel`,
//!   `items_list`, `items_thread`, `items_retract`; every call returns a
//!   [`Reply`] (the whole body + parsed JSON).
//! * [`Operator`]: `reply`, `answer_card`, `always_allow`.
//! * [`Sender`]: the production egress (`A2aEgress::compose`) over the
//!   operator's `/team` rail.
//! * [`log_cursor`] / [`log_since`]: phase-local TRACE log reads.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Once};
use std::time::Duration;

use arc_swap::ArcSwap;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures::stream::BoxStream;
use rustain::adapters::a2a::auth::{
    A2aServerSecurity, API_KEY_HEADER, BindEvidence, resolve_server_auth,
};
use rustain::adapters::a2a::card_cache::SignedCardCache;
use rustain::adapters::a2a::config::{
    parse_workspace_a2a_config, parse_workspace_a2a_server_config,
};
use rustain::adapters::a2a::egress::A2aEgress;
use rustain::adapters::a2a::exec::INBOUND_SUBAGENT_TYPE;
use rustain::adapters::a2a::item_capability::{
    ItemCapabilityKey, ItemCapabilityKeyStore, derive_item_capability, load_item_capability_key,
};
use rustain::adapters::a2a::server::{ServeConfig, serve};
use rustain::adapters::a2a::transparency::{STATUS_QUERY_GATE_PREFIX, TransparencySink};
use rustain::adapters::daemon::protocol::{
    AttachMode, ClientFrame, ConnectionTier, DaemonFrame, PROTOCOL_VERSION, read_frame, write_frame,
};
use rustain::adapters::daemon::runtime::{DaemonCore, DaemonTurnRuntime};
use rustain::adapters::daemon::server::AttachServer;
use rustain::adapters::filesystem::FileSystemStorage;
use rustain::adapters::noop::{
    NoOpApprovalPersistence, NoOpMemory, NoOpPersona, NoOpSecurity, NoOpToolSet, NoOpUsageLedger,
};
use rustain::adapters::rap::{AgentSigner, IdentityKeyStore};
use rustain::adapters::tui::app::{InputAction, submit_message_for_test};
use rustain::adapters::tui::handlers::team_command::TEAM_THREAD_BLOCK_ID;
use rustain::adapters::tui::state::TuiState;
use rustain::domain::errors::ProviderError;
use rustain::domain::events::AppEvent;
use rustain::domain::models::capability_registry::CapabilityRegistry;
use rustain::domain::models::{
    A2aPeerSource, A2aPeerSpec, AppConfig, ApprovalOutcome, ApprovalScope, CompletionOptions,
    Conversation, EffectivePolicy, FeedbackLevel, JournalRecord, Message, PeerId, ResponseMode,
    RoomEvent, SemanticMessageType, StopReason, StreamChunk,
};
use rustain::domain::ports::{
    DeliveryPolicy, EffectiveDeliveryPolicy, PeerResponsePolicy, RoomJournal, RoomJournalReader,
    SecurityPort, StoragePort, StreamingProvider, ToolSetPort,
};
use rustain::domain::services::approval_runtime::{ApprovalRuntime, ApprovalRuntimeEvent};
use rustain::infrastructure::runtime::app_state::AppState;
use rustain::infrastructure::runtime::transparency_bridge::{
    team_command, team_send_settled, team_thread_ready,
};
use rustain::infrastructure::subagent::{NodeJournal, NodeRoomJournal, NodeTree};
use tokio_util::sync::CancellationToken;

/// `conformance_19_18c_a2a_item_capability.rs:100`.
pub const SETTLE: Duration = Duration::from_secs(45);

// ── Credentials: names, values, the fixed env table ─────────────────────────

/// Key VALUES (what a client presents in `x-api-key`).
pub const K_P1A: &str = "rustain-19-18d-harness-key-p1a";
pub const K_P1B: &str = "rustain-19-18d-harness-key-p1b";
pub const K_P1C: &str = "rustain-19-18d-harness-key-p1c";
pub const K_P2: &str = "rustain-19-18d-harness-key-p2";
pub const K_U: &str = "rustain-19-18d-harness-key-u";

/// Env-var NAMES the written `server` blocks carry (`peerKeys` keys,
/// `apiKeys` entries).
pub const K_P1A_ENV: &str = "RUSTAIN_19_18D_HARNESS_K_P1A";
pub const K_P1B_ENV: &str = "RUSTAIN_19_18D_HARNESS_K_P1B";
pub const K_P1C_ENV: &str = "RUSTAIN_19_18D_HARNESS_K_P1C";
pub const K_P2_ENV: &str = "RUSTAIN_19_18D_HARNESS_K_P2";
pub const K_U_ENV: &str = "RUSTAIN_19_18D_HARNESS_K_U";

/// The fixed map the resolver reads (NAME → VALUE). The server side NEVER
/// reads the process environment; [`setup`] mirrors the same table into the
/// process environment ONLY so a production SENDER (`A2aPeerSpec.auth`) can
/// present a key by name.
pub const KEY_TABLE: &[(&str, &str)] = &[
    (K_P1A_ENV, K_P1A),
    (K_P1B_ENV, K_P1B),
    (K_P1C_ENV, K_P1C),
    (K_P2_ENV, K_P2),
    (K_U_ENV, K_U),
];

/// The resolver's env closure: a lookup on [`KEY_TABLE`].
pub fn key_env(name: &str) -> Option<String> {
    KEY_TABLE
        .iter()
        .find(|(env, _)| *env == name)
        .map(|(_, value)| (*value).to_owned())
}

// ── The one global TRACE buffer with phase-local cursors ────────────────────

static SETUP: Once = Once::new();
static LOG: LazyLock<Arc<std::sync::Mutex<Vec<u8>>>> = LazyLock::new(Default::default);

/// `conformance_19_18c_a2a_item_capability.rs:1011-1034` — the one global
/// `tracing` buffer: TRACE, unfiltered, so a spawned `serve` handler's line
/// lands here too.
#[derive(Clone)]
struct LogBuffer(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = LogBuffer;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Call FIRST in every test. One `Once` mirrors [`KEY_TABLE`] into the process
/// environment (a parallel `set_var` races `getenv`) and installs the one
/// global TRACE subscriber (`conformance_19_18c…:1039-1056`).
pub fn setup() {
    SETUP.call_once(|| {
        // SAFETY: set exactly once, before any test in this binary spawns a
        // client that reads them; the names are unique to this harness.
        unsafe {
            for (name, value) in KEY_TABLE {
                std::env::set_var(name, value);
            }
        }
        let buffer = LOG.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(LogBuffer(buffer))
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("one global subscriber");
    });
}

fn log_bytes() -> String {
    String::from_utf8_lossy(
        &LOG.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
    .into_owned()
}

/// Snapshot the buffer length BEFORE a phase; read only what follows with
/// [`log_since`]. (Other tests in the same process also write to the buffer —
/// assert on distinctive lines, never on absence of a generic line.)
pub fn log_cursor() -> usize {
    LOG.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .len()
}

/// Everything logged after `cursor` (lossy UTF-8).
pub fn log_since(cursor: usize) -> String {
    let bytes = LOG
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    String::from_utf8_lossy(&bytes[cursor.min(bytes.len())..]).into_owned()
}

/// The whole buffer.
pub fn captured_log() -> String {
    log_bytes()
}

// ── Roster ──────────────────────────────────────────────────────────────────

/// Raw ed25519 `x` bytes behind the standard pins (the bytes are arbitrary
/// 32-byte strings: the roster only ever pins them).
pub const P1_X: [u8; 32] = [0x11; 32];
pub const P2_X: [u8; 32] = [0x22; 32];

/// `{ "alg": "EdDSA", "x": "<b64url>" }`.
pub fn pinned_key(x: [u8; 32]) -> serde_json::Value {
    serde_json::json!({ "alg": "EdDSA", "x": URL_SAFE_NO_PAD.encode(x) })
}

/// The pin's identity exactly as production derives it
/// (`PeerId::from_jwk_ed25519_x` = `A2aPeerSpec::pinned_identity`).
pub fn pin_of(x: [u8; 32]) -> PeerId {
    PeerId::from_jwk_ed25519_x(&URL_SAFE_NO_PAD.encode(x)).expect("a 32-byte x is a pin")
}

/// `pin_of(x)` as the wire string (`1220…`).
pub fn pin_hex(x: [u8; 32]) -> String {
    pin_of(x).as_str().to_owned()
}

/// The `agents` object of the written `a2a.json`.
#[derive(Clone, Debug)]
pub struct Roster(serde_json::Map<String, serde_json::Value>);

impl Roster {
    /// `p1` and `p1-alt` pin the SAME key ([`P1_X`]); `p2` pins a distinct
    /// key ([`P2_X`]).
    pub fn standard() -> Self {
        Self(serde_json::Map::new())
            .pinned("p1", P1_X)
            .pinned("p1-alt", P1_X)
            .pinned("p2", P2_X)
    }

    /// Add / replace a pinned alias with a caller-chosen raw key (K14 (ii)'s
    /// `pc`).
    pub fn pinned(mut self, alias: &str, x: [u8; 32]) -> Self {
        self.0.insert(
            alias.to_owned(),
            serde_json::json!({
                "url": format!("https://{alias}.example/"),
                "pinnedKey": pinned_key(x),
            }),
        );
        self
    }

    /// Add / replace an UNPINNED alias (refusal tests).
    pub fn unpinned(mut self, alias: &str) -> Self {
        self.0.insert(
            alias.to_owned(),
            serde_json::json!({ "url": format!("https://{alias}.example/") }),
        );
        self
    }

    /// Drop an alias (config-change restarts).
    pub fn without(mut self, alias: &str) -> Self {
        self.0.remove(alias);
        self
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::Value::Object(self.0.clone())
    }
}

// ── Host presets and specs ──────────────────────────────────────────────────

/// B1's `server.peerKeys` (`K_P1A→p1`, `K_P1B→p1`, `K_P1C→p1-alt`,
/// `K_P2→p2`), without `admission`.
pub fn b1_peer_keys() -> serde_json::Value {
    serde_json::json!({
        K_P1A_ENV: "p1",
        K_P1B_ENV: "p1",
        K_P1C_ENV: "p1-alt",
        K_P2_ENV: "p2",
    })
}

/// Host B1 (bound-only): `admission: allow` + [`b1_peer_keys`].
pub fn b1_server() -> serde_json::Value {
    serde_json::json!({ "admission": "allow", "peerKeys": b1_peer_keys() })
}

/// Host B2 (B1 + the unbound key `K_U`).
pub fn b2_server() -> serde_json::Value {
    serde_json::json!({
        "admission": "allow",
        "peerKeys": b1_peer_keys(),
        "apiKeys": [K_U_ENV],
    })
}

/// Where the listener binds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Listen {
    /// `0.0.0.0:0` — the TLS, network-ready posture (the default).
    Wildcard,
    /// `127.0.0.1:0` — every caller is `loopback` (keys ignored). Only for
    /// claims ABOUT loopback (K05 phases 0 and 6); ⛔ never for separation
    /// claims.
    Loopback,
}

/// Everything a host start is built from.
#[derive(Clone, Debug)]
pub struct HostSpec {
    pub roster: Roster,
    /// The `server` block of the written `a2a.json`. `tls` and
    /// `advertisedHost` are added when absent.
    pub server: serde_json::Value,
    /// `ModePolicy` response mode — ignored when [`Self::interaction`] is set
    /// (then the written TOML decides).
    pub mode: ResponseMode,
    /// Compose the `AttachServer` with a `JournalConsentProjection` (always
    /// true when [`Self::interaction`] is set, as production composes it).
    pub consent: bool,
    /// Optional production policy composition: this TOML is written as
    /// `<workspace>/.rustain/<INDIVIDUAL_POLICY_FILE>`, resolved ONCE into an
    /// `EffectivePolicy`, installed in the peer bus AND the `AttachServer` and
    /// passed to `configure_consent_policy` (`P17`, `F14(a)`; mirrors
    /// `daemon/mod.rs:555-578`).
    pub interaction: Option<String>,
    pub listen: Listen,
    /// Serve TLS (default). `false` = a plaintext listener (loopback only;
    /// a wildcard bind needs TLS).
    pub tls: bool,
}

impl HostSpec {
    /// Host B1: bound-only, `admission: allow`, `NotifyAndWait`.
    pub fn b1() -> Self {
        Self::with_server(b1_server())
    }

    /// Host B2: B1 + `apiKeys: [K_U]`.
    pub fn b2() -> Self {
        Self::with_server(b2_server())
    }

    /// Arbitrary `server` block over the standard roster.
    pub fn with_server(server: serde_json::Value) -> Self {
        Self {
            roster: Roster::standard(),
            server,
            mode: ResponseMode::NotifyAndWait,
            consent: false,
            interaction: None,
            listen: Listen::Wildcard,
            tls: true,
        }
    }

    pub fn roster(mut self, roster: Roster) -> Self {
        self.roster = roster;
        self
    }

    pub fn mode(mut self, mode: ResponseMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn consent(mut self, consent: bool) -> Self {
        self.consent = consent;
        self
    }

    pub fn interaction(mut self, toml: &str) -> Self {
        self.interaction = Some(toml.to_owned());
        self
    }

    pub fn listen(mut self, listen: Listen) -> Self {
        self.listen = listen;
        self
    }

    pub fn tls(mut self, tls: bool) -> Self {
        self.tls = tls;
        self
    }

    /// Replace one `server` key (e.g. `.server_key("threadReads",
    /// "credential-scoped".into())`).
    pub fn server_key(mut self, key: &str, value: serde_json::Value) -> Self {
        self.server
            .as_object_mut()
            .expect("the server block is an object")
            .insert(key.to_owned(), value);
        self
    }
}

// ── Journal ─────────────────────────────────────────────────────────────────

/// `conformance_19_18d0…:74-98` (`Journal`, `open_journal`).
#[derive(Clone)]
pub struct Journal {
    pub workspace: Arc<tempfile::TempDir>,
    pub path: PathBuf,
    pub node: Arc<NodeJournal>,
    pub room: Arc<dyn RoomJournal>,
}

pub async fn open_journal(workspace: Arc<tempfile::TempDir>) -> Journal {
    let node = Arc::new(
        NodeJournal::open_workspace(workspace.path())
            .await
            .expect("open node journal"),
    );
    let room: Arc<dyn RoomJournal> = Arc::new(NodeRoomJournal::new(node.clone(), None));
    Journal {
        path: workspace.path().to_path_buf(),
        workspace,
        node,
        room,
    }
}

/// `conformance_19_18c…:791-801` (`room_events`).
pub async fn room_events(journal: &Journal) -> Vec<(u64, RoomEvent)> {
    RoomJournalReader::load_entries(journal.node.as_ref())
        .await
        .expect("read durable journal")
        .into_iter()
        .filter_map(|entry| match entry.record {
            JournalRecord::Room(event) => Some((entry.seq, event)),
            _ => None,
        })
        .collect()
}

/// `conformance_19_18d0…:871-889` (`raw_payloads`): the durable journal's raw
/// JSONL `record.payload` objects whose `event` tag is `event` (what an older
/// or newer reader would actually see on disk), in journal order.
pub fn raw_payloads(journal: &Journal, event: &str) -> Vec<serde_json::Value> {
    let rooms = journal.path.join(".rustain").join("rooms");
    let files: Vec<PathBuf> = std::fs::read_dir(&rooms)
        .expect("rooms dir")
        .map(|entry| entry.expect("entry").path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .collect();
    assert_eq!(files.len(), 1, "one room journal: {files:?}");
    std::fs::read_to_string(&files[0])
        .expect("journal bytes")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("a JSON line"))
        .filter_map(|line| {
            let payload = line["record"]["payload"].clone();
            (payload["event"] == event).then_some(payload)
        })
        .collect()
}

/// Every on-disk `recipient_item_received` row: `address` (the wire address
/// as written), `alias`, `task`, `content`, … in journal order.
pub fn received_rows(journal: &Journal) -> Vec<serde_json::Value> {
    raw_payloads(journal, "recipient_item_received")
}

/// The on-disk `recipient_item_received` row for sender task `task`
/// (panics when absent or ambiguous).
pub fn received_row_for_task(journal: &Journal, task: &str) -> serde_json::Value {
    let mut rows: Vec<_> = received_rows(journal)
        .into_iter()
        .filter(|row| row["task"] == task)
        .collect();
    assert_eq!(
        rows.len(),
        1,
        "exactly one received row for {task}: {rows:?}"
    );
    rows.remove(0)
}

/// The item id the recipient allocated for sender task `task`.
pub fn item_id_for_task(journal: &Journal, task: &str) -> String {
    received_row_for_task(journal, task)["address"]["item"]
        .as_str()
        .expect("an item id")
        .to_owned()
}

/// Every on-disk `recipient_item_replied` row (`address` = the `TurnAddress`
/// wire form, `answers`, `content`, …), in journal order.
pub fn replied_rows(journal: &Journal) -> Vec<serde_json::Value> {
    raw_payloads(journal, "recipient_item_replied")
}

/// The wire address of a bound item / turn:
/// `{"principal":{"kind":"a2a_bound"},"item":<item>,"boundPeer":<pin>}`.
pub fn bound_item_address(item: &str, pin: &str) -> serde_json::Value {
    serde_json::json!({
        "principal": { "kind": "a2a_bound" },
        "item": item,
        "boundPeer": pin,
    })
}

/// The wire address of a bound reply turn:
/// `{"principal":{"kind":"a2a_bound"},"turn":<turn>,"boundPeer":<pin>}`.
pub fn bound_turn_address(turn: &str, pin: &str) -> serde_json::Value {
    serde_json::json!({
        "principal": { "kind": "a2a_bound" },
        "turn": turn,
        "boundPeer": pin,
    })
}

// ── Scripted provider / runtime (copied) ────────────────────────────────────

/// `conformance_19_18d0…:100-124` (`ModePolicy`).
#[derive(Debug)]
struct ModePolicy(ResponseMode);

impl DeliveryPolicy for ModePolicy {
    fn decide(
        &self,
        _header: &rustain::domain::models::MessageHeader,
        ownership: rustain::domain::models::OwnershipKind,
    ) -> rustain::domain::models::DeliveryDisposition {
        rustain::domain::models::relationship_disposition(ownership)
    }

    fn response_policy_for_peer(
        &self,
        _principal: &rustain::domain::models::PolicyPrincipal,
        _message_type: SemanticMessageType,
    ) -> PeerResponsePolicy {
        PeerResponsePolicy {
            mode: self.0,
            auto_response: None,
            ..Default::default()
        }
    }
}

/// `conformance_19_18c…:154-196` (`ScriptedProvider`).
struct ScriptedProvider {
    text: String,
}

#[async_trait::async_trait]
impl StreamingProvider for ScriptedProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<Message>,
        _options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        use futures::StreamExt;
        Ok(futures::stream::iter(vec![
            StreamChunk::Text {
                content: self.text.clone(),
                parent_tool_use_id: None,
            },
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
        ])
        .boxed())
    }
    async fn abort(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    fn provider_id(&self) -> String {
        "reply-scripted".to_owned()
    }
    fn list_models(&self) -> Vec<rustain::domain::models::provider::ModelDescriptor> {
        Vec::new()
    }
    async fn health_check(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    async fn connectivity_probe(
        &self,
    ) -> Result<rustain::domain::ports::ProbeOutcome, ProviderError> {
        Ok(rustain::domain::ports::ProbeOutcome {
            latency: Duration::ZERO,
        })
    }
}

/// `conformance_19_18c…:250-291` (`build_runtime`).
fn build_runtime(
    provider: Arc<dyn StreamingProvider>,
    storage: Arc<dyn StoragePort>,
    workspace: &Path,
) -> Arc<DaemonTurnRuntime> {
    let security: Arc<dyn SecurityPort> = Arc::new(NoOpSecurity);
    let tools: Arc<dyn ToolSetPort> = Arc::new(NoOpToolSet);
    let approval = ApprovalRuntime::new(64, Arc::new(NoOpApprovalPersistence));
    let tool_scheduler = rustain::domain::services::tool_scheduler::ToolScheduler::new(
        security.clone(),
        tools.clone(),
        approval.clone(),
        64,
    );
    Arc::new(DaemonTurnRuntime {
        provider,
        app_config: Arc::new(ArcSwap::from_pointee(AppConfig::default())),
        security,
        tools,
        tool_scheduler,
        persona: Arc::new(NoOpPersona),
        context_assembler: Arc::new(ArcSwap::from_pointee(None)),
        context: Arc::new(ArcSwap::from_pointee(
            Arc::new(rustain::adapters::noop::NoOpContext)
                as Arc<dyn rustain::domain::ports::ContextPort>,
        )),
        storage: storage.clone(),
        fs_storage: Arc::new(FileSystemStorage::with_workspace_root(
            rustain::infrastructure::paths::sessions_dir(workspace),
            workspace.to_path_buf(),
        )),
        usage_ledger: Arc::new(NoOpUsageLedger),
        telemetry: rustain::infrastructure::telemetry::ActiveRatioWindow::new_in_memory(),
        plan_injector: Arc::new(
            rustain::domain::services::plan_mode_injector::DefaultPlanInjector::new(),
        ),
        approval,
        workspace: workspace.to_path_buf(),
        #[cfg(feature = "mcp")]
        mcp_task_runtimes: Vec::new(),
    })
}

/// `conformance_19_18c…:293`.
pub const CONVERSATION_ID: &str = "reply-recipient";

// ── The host ────────────────────────────────────────────────────────────────

/// Relative (to the workspace) TLS material the harness keeps beside the
/// written config; `server.tls` points at these.
const TLS_CERT: &str = ".rustain/harness-tls/cert.pem";
const TLS_KEY: &str = ".rustain/harness-tls/key.pem";

/// A served 19.18d host: written `a2a.json` → production parsers → production
/// resolver → `serve(ServeConfig…)` over the production `AttachServer`.
pub struct BoundHost {
    /// The host's workspace + durable journal (survives [`Self::restart`]).
    pub journal: Journal,
    /// The composed `Arc<DaemonCore>` (`F14(c)`): e.g.
    /// `host.core.ensure_runtime().await?.approval.subscribe()`, or
    /// [`Self::approval_events`].
    pub core: Arc<DaemonCore>,
    pub server: Arc<AttachServer>,
    pub node_tree: NodeTree,
    /// The spec this process was started from.
    pub spec: HostSpec,
    pub listen: Listen,
    /// The resolved effective policy when the spec carries `interaction`.
    pub effective_policy: Option<Arc<EffectivePolicy>>,
    /// The security the resolver + written config produced (what `serve`
    /// got).
    pub security: A2aServerSecurity,
    keys: Arc<tempfile::TempDir>,
    listener: std::net::TcpListener,
    cancel: CancellationToken,
    http: tokio::task::JoinHandle<anyhow::Result<()>>,
    domain_rx: Option<tokio::sync::mpsc::UnboundedReceiver<AppEvent>>,
    _singleton: Option<rustain::infrastructure::subagent::DaemonSingletonLock>,
}

fn bind(listen: Listen) -> std::net::TcpListener {
    let listener = std::net::TcpListener::bind(match listen {
        Listen::Wildcard => "0.0.0.0:0",
        Listen::Loopback => "127.0.0.1:0",
    })
    .expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    listener
}

/// Self-signed `localhost` certificate under the workspace, generated once
/// (a restart reuses it, so an anchored client keeps working).
fn ensure_tls_material(workspace: &Path) {
    let (cert_path, key_path) = (workspace.join(TLS_CERT), workspace.join(TLS_KEY));
    if cert_path.exists() && key_path.exists() {
        return;
    }
    std::fs::create_dir_all(cert_path.parent().expect("tls dir")).expect("tls dir");
    let cert =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("certificate");
    std::fs::write(&cert_path, cert.cert.pem()).expect("cert");
    std::fs::write(&key_path, cert.signing_key.serialize_pem()).expect("key");
}

impl BoundHost {
    /// Start a fresh host on a fresh workspace.
    pub async fn start(spec: HostSpec) -> Self {
        let keys = Arc::new(tempfile::tempdir().expect("keys"));
        let journal = open_journal(Arc::new(tempfile::tempdir().expect("workspace"))).await;
        let listener = bind(spec.listen);
        Self::compose(journal, listener, keys, spec, false).await
    }

    /// `conformance_19_18d0…:377-523` (`compose`) with the 19.18d front door:
    /// the config is WRITTEN, parsed by both production parsers and resolved
    /// by `resolve_server_auth`; the policy composition mirrors
    /// `daemon/mod.rs:555-578`. `restarted` runs the production recovery
    /// (`with_host_binding` + `NodeRecovery::reconcile`, `:388-410`).
    async fn compose(
        journal: Journal,
        listener: std::net::TcpListener,
        keys: Arc<tempfile::TempDir>,
        spec: HostSpec,
        restarted: bool,
    ) -> Self {
        let address = listener.local_addr().expect("address");
        let listen = if address.ip().is_loopback() {
            Listen::Loopback
        } else {
            Listen::Wildcard
        };

        // 1. ONE written a2a.json.
        let mut server_block = spec.server.clone();
        {
            let object = server_block
                .as_object_mut()
                .expect("the server block is an object");
            let absent =
                |object: &serde_json::Map<String, serde_json::Value>, camel: &str, snake: &str| {
                    !object.contains_key(camel) && !object.contains_key(snake)
                };
            if spec.tls && absent(object, "tls", "tls") {
                ensure_tls_material(&journal.path);
                object.insert(
                    "tls".to_owned(),
                    serde_json::json!({ "cert": TLS_CERT, "key": TLS_KEY }),
                );
            }
            if absent(object, "advertisedHost", "advertised_host") {
                object.insert(
                    "advertisedHost".to_owned(),
                    format!("localhost:{}", address.port()).into(),
                );
            }
        }
        let config_path = rustain::infrastructure::paths::workspace_a2a_config_path(&journal.path);
        std::fs::create_dir_all(config_path.parent().expect("rustain dir")).expect("dir");
        std::fs::write(
            &config_path,
            serde_json::json!({ "agents": spec.roster.to_json(), "server": server_block })
                .to_string(),
        )
        .expect("a2a.json");

        // 2. Both production parsers, then the one production resolver.
        let roster = parse_workspace_a2a_config(&config_path).expect("roster parses");
        let config = parse_workspace_a2a_server_config(&config_path)
            .expect("server block parses")
            .expect("server block present");
        let auth =
            resolve_server_auth(&config, &roster, key_env).expect("the written a2a.json resolves");
        let tls = match config.tls.as_ref() {
            Some(tls) => Some(
                rustain::adapters::a2a::tls::load_tls_material(
                    &journal.path.join(&tls.cert),
                    &journal.path.join(&tls.key),
                )
                .expect("tls"),
            ),
            None => None,
        };
        let security = A2aServerSecurity {
            tls,
            auth,
            thread_reads: config.thread_reads.unwrap_or_default(),
        };
        if listen == Listen::Wildcard {
            // The `run_inner` gate (`server.rs:2847`): a bound-only host must
            // be network-ready (`M24`).
            assert!(
                security.is_network_ready(),
                "a wildcard host needs TLS + a configured key set"
            );
            assert_eq!(
                rustain::adapters::a2a::auth::evaluate_bind_safety(
                    "0.0.0.0:0",
                    BindEvidence::from_security(&security, true)
                ),
                rustain::adapters::a2a::auth::BindDecision::Bind,
            );
        }

        // 3. Node tree, restart recovery (`conformance_19_18d0…:384-412`).
        let (domain_tx, domain_rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
        let node_tree = NodeTree::with_event_tx(
            domain_tx.clone(),
            Arc::new(|| chrono::Utc::now().timestamp_millis()),
        )
        .with_journal(journal.node.clone())
        .with_host_binding(rustain::infrastructure::subagent::current_host_binding(
            &journal.path,
        ));
        let singleton = if restarted {
            let lock =
                rustain::infrastructure::subagent::DaemonSingletonLock::try_acquire(&journal.path)
                    .await
                    .expect("singleton");
            rustain::infrastructure::subagent::NodeRecovery::reconcile(
                &journal.node,
                &node_tree,
                &lock,
                &rustain::infrastructure::subagent::current_host_id(&journal.path),
            )
            .await
            .expect("recovery");
            Some(lock)
        } else {
            None
        };

        // 4. Core, storage, conversation.
        let storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
            rustain::infrastructure::paths::sessions_dir(&journal.path),
            journal.path.clone(),
        ));
        let conversation = match storage.load_conversation(CONVERSATION_ID).await {
            Ok(Some(conversation)) => conversation,
            _ => Conversation {
                id: CONVERSATION_ID.to_owned(),
                ..Conversation::default()
            },
        };
        let core = {
            let workspace = journal.path.clone();
            let storage = storage.clone();
            Arc::new(DaemonCore::new(
                workspace.clone(),
                Arc::new(ArcSwap::from_pointee(AppConfig::default())),
                Arc::new(NoOpMemory),
                storage.clone(),
                Arc::new(NoOpSecurity),
                Arc::new(NoOpPersona),
                Arc::new(rustain::adapters::rap::PeerTopicStore::new()),
                Box::new(move || {
                    let provider: Arc<dyn StreamingProvider> = Arc::new(ScriptedProvider {
                        text: "automatic reply".to_owned(),
                    });
                    Ok(build_runtime(provider, storage.clone(), &workspace))
                }),
            ))
        };

        // 5. Policy composition: either production's (resolve ONE
        // EffectivePolicy from the written interaction TOML, install it in the
        // bus AND the AttachServer, `configure_consent_policy`) or the plain
        // `ModePolicy`.
        let entries = RoomJournalReader::load_entries(journal.node.as_ref())
            .await
            .expect("consent entries");
        let projection =
            Arc::new(rustain::adapters::policy::JournalConsentProjection::from_entries(&entries));
        let (delivery, effective_policy, consent): (
            Arc<dyn DeliveryPolicy>,
            Option<Arc<EffectivePolicy>>,
            _,
        ) = match spec.interaction.as_deref() {
            Some(toml) => {
                let dir = journal.path.join(".rustain");
                std::fs::create_dir_all(&dir).expect("rustain dir");
                std::fs::write(
                    dir.join(rustain::domain::models::INDIVIDUAL_POLICY_FILE),
                    toml,
                )
                .expect("interaction policy file");
                let (policy, _explanation) = rustain::adapters::policy::resolve_workspace_policy(
                    &journal.path,
                    &roster,
                    projection.as_ref(),
                )
                .expect("the written interaction policy resolves");
                let policy = Arc::new(policy);
                (
                    Arc::new(EffectiveDeliveryPolicy::new(policy.clone())),
                    Some(policy),
                    Some(projection.clone()),
                )
            }
            None => (
                Arc::new(ModePolicy(spec.mode)),
                None,
                spec.consent.then(|| projection.clone()),
            ),
        };
        let bus = rustain::adapters::daemon::server::peer_bus_slot_with_policy(
            &node_tree,
            delivery.clone(),
        );
        let server = AttachServer::new_with_node_tree_bus_policy_and_journal(
            core.clone(),
            Arc::new(tokio::sync::Mutex::new(conversation)),
            domain_tx,
            node_tree.clone(),
            bus,
            delivery,
            journal.room.clone(),
            journal.node.clone(),
            consent,
        );
        if let Some(policy) = &effective_policy {
            server.configure_consent_policy(policy.clone()).await;
        }

        // 6. serve (`conformance_19_18d0…:469-509`).
        let signer = IdentityKeyStore::new(keys.path())
            .load_or_generate()
            .expect("identity");
        let tokio_listener = tokio::net::TcpListener::from_std(
            listener
                .try_clone()
                .expect("clone the one listening socket"),
        )
        .expect("tokio listener");
        let cancel = CancellationToken::new();
        // Every line `serve` logs (startup warnings included) carries the
        // workspace in its span, so [`host_log_since`] can tell this host's
        // lines from a parallel test's (K05's exact orphan-count lines).
        let http = tokio::spawn(tracing::Instrument::instrument(
            serve(
                tokio_listener,
                ServeConfig {
                    registry: Arc::new(CapabilityRegistry::new(None)),
                    signer,
                    security: security.clone(),
                    runtime: Some(
                        server.clone() as Arc<dyn rustain::domain::ports::InboundPeerRuntime>
                    ),
                    transparency: Arc::new(
                        TransparencySink::new(journal.room.clone())
                            .with_reader(journal.node.clone()),
                    ),
                    policy: config.admission,
                    workspace: journal.path.clone(),
                    advertised_host: config.advertised_host.clone(),
                    cards: Arc::new(SignedCardCache::new()),
                },
                cancel.child_token(),
            ),
            tracing::info_span!("a2a_host", workspace = %journal.path.display()),
        ));
        let host = Self {
            journal,
            core,
            server,
            node_tree,
            spec,
            listen,
            effective_policy,
            security,
            keys,
            listener,
            cancel,
            http,
            domain_rx: Some(domain_rx),
            _singleton: singleton,
        };
        host.await_listening().await;
        host
    }

    /// `serve` reconciles after a restart BEFORE it accepts: any HTTP answer
    /// means startup (reconcile + startup log lines) is done.
    async fn await_listening(&self) {
        let probe = self.client_with(None);
        let deadline = tokio::time::Instant::now() + SETTLE;
        loop {
            if probe
                .client
                .post(&probe.url)
                .json(&serde_json::json!({}))
                .send()
                .await
                .is_ok()
            {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the listener never answered"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    pub fn port(&self) -> u16 {
        self.listener.local_addr().expect("address").port()
    }

    pub fn is_tls(&self) -> bool {
        self.security.tls.is_some()
    }

    /// The workspace TLS certificate the anchored clients trust (`None` for a
    /// plaintext listener).
    pub fn cert_path(&self) -> Option<PathBuf> {
        self.is_tls().then(|| self.journal.path.join(TLS_CERT))
    }

    pub fn endpoint(&self) -> String {
        format!(
            "{}://localhost:{}/",
            if self.is_tls() { "https" } else { "http" },
            self.port()
        )
    }

    /// An ANCHORED raw JSON-RPC client presenting `key` in `x-api-key`
    /// (any value: a configured key, a retired one, a stranger's).
    pub fn client(&self, key: &str) -> RawClient {
        self.client_with(Some(key))
    }

    /// Like [`Self::client`]; `None` presents no `x-api-key` header.
    pub fn client_with(&self, key: Option<&str>) -> RawClient {
        let client = match self.cert_path() {
            // `conformance_19_18d0…:750-763` (`anchored`).
            Some(cert) => {
                let pem = std::fs::read(cert).expect("anchor pem");
                let anchor = reqwest::Certificate::from_pem(&pem).expect("anchor certificate");
                reqwest::Client::builder()
                    .use_rustls_tls()
                    .tls_built_in_root_certs(false)
                    .add_root_certificate(anchor)
                    .build()
                    .expect("anchored client")
            }
            None => reqwest::Client::new(),
        };
        RawClient {
            url: self.endpoint(),
            key: key.map(str::to_owned),
            client,
        }
    }

    /// `conformance_19_18c…:1137-1147` (`credentialed`): a roster entry for a
    /// production SENDER that presents the key stored under env name `env`
    /// (`A2aPeerSpec.auth`), anchored on this host's certificate.
    pub fn credentialed(&self, alias: &str, env: &str) -> A2aPeerSpec {
        let mut spec = A2aPeerSpec::new(
            alias,
            rustain::domain::models::RedactedUrl::from(self.endpoint()),
            A2aPeerSource::Workspace,
        );
        spec.auth = Some(env.to_owned());
        spec.ca_cert = self.cert_path();
        spec
    }

    /// The approval runtime's event stream (`F14(c)`; lib precedent
    /// `daemon/server.rs:6143`).
    pub async fn approval_events(&self) -> tokio::sync::broadcast::Receiver<ApprovalRuntimeEvent> {
        self.core
            .ensure_runtime()
            .await
            .expect("the daemon runtime builds")
            .approval
            .subscribe()
    }

    /// Restart on the SAME workspace, journal and identity with a (possibly
    /// different) written config, through production-shaped recovery
    /// (`with_host_binding`, `NodeRecovery::reconcile`, the pending-file
    /// pass inside `serve`). Keeps the same listening socket (so the port and
    /// the `advertisedHost` stay valid) unless the spec's `listen` differs,
    /// in which case a fresh listener of that kind is bound.
    pub async fn restart(self, spec: HostSpec) -> Self {
        let wanted = spec.listen;
        let listener = if wanted == self.listen {
            Some(self.listener.try_clone().expect("keep the one socket"))
        } else {
            None
        };
        self.cancel.cancel();
        self.http.await.expect("server task").expect("shutdown");
        let Self {
            journal,
            listener: old_listener,
            keys,
            server,
            node_tree,
            core,
            _singleton,
            ..
        } = self;
        drop(old_listener);
        drop(server);
        drop(node_tree);
        drop(core);
        drop(_singleton);
        let workspace = journal.workspace.clone();
        drop(journal);
        let _ = std::fs::remove_file(workspace.path().join("daemon.sock"));
        let listener = listener.unwrap_or_else(|| bind(wanted));
        Self::compose(open_journal(workspace).await, listener, keys, spec, true).await
    }

    /// [`Self::restart`] with the SAME spec but a different listener kind.
    pub async fn restart_as(self, listen: Listen) -> Self {
        let spec = self.spec.clone().listen(listen);
        self.restart(spec).await
    }

    /// `conformance_19_18d0…:555-605` (`attach_operator`): the operator's
    /// attach socket (card answers, `ReplyToRecipientItem`).
    pub async fn attach_operator(&mut self) -> Operator {
        let socket = self.journal.path.join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind unix socket");
        let shutdown = CancellationToken::new();
        tokio::spawn(
            self.server.clone().run(
                listener,
                self.domain_rx
                    .take()
                    .expect("domain rx (one operator per start)"),
                None,
                None,
                shutdown.child_token(),
            ),
        );
        let mut stream = tokio::net::UnixStream::connect(&socket)
            .await
            .expect("connect");
        let nonce = match read_frame::<_, DaemonFrame>(&mut stream)
            .await
            .expect("challenge")
        {
            Some(DaemonFrame::AttachChallenge { nonce }) => nonce,
            other => panic!("expected AttachChallenge, got {other:?}"),
        };
        let keys = tempfile::tempdir().expect("operator keys");
        let signer: AgentSigner = IdentityKeyStore::new(keys.path())
            .load_or_generate()
            .expect("operator identity");
        let tier = ConnectionTier::TrustedLocal;
        let proof = signer.attach_proof(&nonce, PROTOCOL_VERSION, tier.proof_tag(), false);
        write_frame(
            &mut stream,
            &ClientFrame::Attach {
                protocol_version: PROTOCOL_VERSION,
                read_only_ok: false,
                tier,
                challenge_nonce: nonce,
                identity: signer.identity().clone(),
                proof,
            },
        )
        .await
        .expect("send attach");
        match read_frame::<_, DaemonFrame>(&mut stream)
            .await
            .expect("ack")
        {
            Some(DaemonFrame::AttachAck { granted_mode, .. }) => {
                assert_eq!(granted_mode, AttachMode::ReadWrite);
            }
            other => panic!("expected AttachAck, got {other:?}"),
        }
        Operator { stream, shutdown }
    }

    /// Bounded wait for the waiting inbound node of sender task `task`
    /// (`conformance_19_18c…:815-834`).
    pub async fn waiting_node(&self, task: &str) -> String {
        let suffix = format!("/t-{}", URL_SAFE_NO_PAD.encode(task));
        let deadline = tokio::time::Instant::now() + SETTLE;
        loop {
            if let Some(entry) = self.node_tree.list().await.into_iter().find(|entry| {
                entry.subagent_type == rustain::adapters::a2a::exec::INBOUND_SUBAGENT_TYPE
                    && entry.current_status == rustain::domain::models::NodeState::Waiting
                    && entry.agent_id.as_str().ends_with(&suffix)
            }) {
                return entry.agent_id.as_str().to_owned();
            }
            assert!(tokio::time::Instant::now() < deadline, "no waiting node");
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The persisted conversation's peer-response card for `node`
    /// (`conformance_19_18c…:934-948`).
    pub async fn persisted_card(&self, node: &str) -> Option<String> {
        let storage = FileSystemStorage::with_workspace_root(
            rustain::infrastructure::paths::sessions_dir(&self.journal.path),
            self.journal.path.clone(),
        );
        storage
            .load_conversation(CONVERSATION_ID)
            .await
            .ok()
            .flatten()?
            .messages
            .into_iter()
            .find(|message| message.id == format!("peer-response-{node}"))
            .map(|message| message.content)
    }

    /// Wait (bounded) until `node`'s card is persisted in a pending shape
    /// (`conformance_19_18c…:951-965`): the moment an operator reply lands.
    pub async fn pending_card(&self, node: &str) {
        let deadline = tokio::time::Instant::now() + SETTLE;
        loop {
            if let Some(content) = self.persisted_card(node).await
                && (content == "[awaiting your response]" || content.starts_with("[y] Approve"))
            {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "card for {node} never persisted pending"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Send `task` through `client` and wait until its inbound node is parked
    /// with a pending card (so the operator can answer it). Returns the
    /// recipient item id the host journaled for it.
    ///
    /// Not for a host composed with a consent projection (`interaction` or
    /// `consent(true)` and no standing grant): there the task first raises an
    /// `a2a/sender-consent` card — send with [`RawClient::message_send`],
    /// answer with [`Operator::always_allow`] / [`Operator::answer_card`],
    /// then call [`Self::waiting_node`] / [`Self::pending_card`] yourself.
    pub async fn send_and_park(&self, client: &RawClient, task: &str) -> String {
        let sent = client
            .message_send(1, task, task, serde_json::json!({}))
            .await;
        assert!(sent.json["error"].is_null(), "{task}: {}", sent.body);
        let node = self.waiting_node(task).await;
        self.pending_card(&node).await;
        item_id_for_task(&self.journal, task)
    }
}

// ── Operator (attach socket) ────────────────────────────────────────────────

/// The operator's attach connection. Dropping it stops the attach loop.
pub struct Operator {
    pub stream: tokio::net::UnixStream,
    shutdown: CancellationToken,
}

impl Drop for Operator {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

impl Operator {
    /// `conformance_19_18c…:902-914` (`next_answer`): the next frame that
    /// answers an act — forwarded turn events / approval requests skipped.
    pub async fn next_answer(&mut self) -> DaemonFrame {
        loop {
            match tokio::time::timeout(SETTLE, read_frame::<_, DaemonFrame>(&mut self.stream))
                .await
                .expect("an operator frame before the deadline")
                .expect("readable frame")
                .expect("open socket")
            {
                DaemonFrame::Event(_) | DaemonFrame::ApprovalRequest { .. } => {}
                frame => return frame,
            }
        }
    }

    /// `conformance_19_18c…:916-931` (`operator_reply`): the operator reply
    /// path (`ReplyToRecipientItem`). Wait for the item's card first
    /// ([`BoundHost::pending_card`]).
    pub async fn reply(&mut self, item_id: &str, text: &str) -> DaemonFrame {
        write_frame(
            &mut self.stream,
            &ClientFrame::ReplyToRecipientItem {
                item_id: item_id.to_owned(),
                text: text.to_owned(),
            },
        )
        .await
        .expect("operator frame");
        self.next_answer().await
    }

    /// Answer the next forwarded approval card (`tool`) with `outcome` through
    /// the real frame (`conformance_19_18d0…:913-938`).
    pub async fn answer_card(&mut self, tool: &str, outcome: ApprovalOutcome) {
        let request_id = loop {
            if let DaemonFrame::ApprovalRequest {
                request_id,
                tool: raised,
                ..
            } = tokio::time::timeout(SETTLE, read_frame::<_, DaemonFrame>(&mut self.stream))
                .await
                .expect("an approval request before the deadline")
                .expect("frame")
                .expect("open")
            {
                assert_eq!(raised, tool, "the card the operator was shown");
                break request_id;
            }
        };
        write_frame(
            &mut self.stream,
            &ClientFrame::ApprovalResponse {
                request_id,
                outcome,
            },
        )
        .await
        .expect("answer the card");
    }

    /// `[a]` on a sender-consent card (`conformance_19_18d0…:941-950`).
    pub async fn always_allow(&mut self) {
        self.answer_card(
            "a2a/sender-consent",
            ApprovalOutcome::AlwaysAndSave {
                scope: ApprovalScope::Tool("a2a/sender-consent".to_owned()),
            },
        )
        .await;
    }
}

/// `conformance_19_18c…:987-992` (`frame_turn`): the `rr_…` turn id a
/// `RecipientItemReplyRecorded` answer carries.
pub fn frame_turn(frame: &DaemonFrame) -> String {
    let DaemonFrame::RecipientItemReplyRecorded { turn_id, .. } = frame else {
        panic!("expected RecipientItemReplyRecorded, got {frame:?}");
    };
    turn_id.clone()
}

// ── Raw JSON-RPC client ─────────────────────────────────────────────────────

/// The WHOLE response body (byte identity is `body == body`) and its parse.
#[derive(Clone, Debug)]
pub struct Reply {
    pub body: String,
    pub json: serde_json::Value,
}

/// `conformance_19_18c…:1149-1185` (`Raw`) over an anchored client.
pub struct RawClient {
    pub url: String,
    pub key: Option<String>,
    pub client: reqwest::Client,
}

impl RawClient {
    /// POST one JSON-RPC body; the whole response body + its parse.
    pub async fn post(&self, body: serde_json::Value) -> Reply {
        let mut request = self.client.post(&self.url).json(&body);
        if let Some(key) = &self.key {
            request = request.header(API_KEY_HEADER, key);
        }
        let body = request
            .send()
            .await
            .expect("post")
            .text()
            .await
            .expect("body");
        let json = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
        Reply { body, json }
    }

    /// The whole response body only.
    pub async fn text(&self, body: serde_json::Value) -> String {
        self.post(body).await.body
    }

    pub async fn call(&self, body: serde_json::Value) -> serde_json::Value {
        self.post(body).await.json
    }

    /// `message/send`; `message_id` is also the sender task id. `metadata`
    /// carries `x-rustain-item-capability` / `x-rustain-in-reply-to` /
    /// `x-rustain-reply-capability` for capability and follow-up cases.
    pub async fn message_send(
        &self,
        id: u64,
        message_id: &str,
        text: &str,
        metadata: serde_json::Value,
    ) -> Reply {
        self.post(send_body(id, message_id, text, metadata)).await
    }

    pub async fn tasks_get(&self, id: u64, task: &str, capability: Option<&str>) -> Reply {
        self.post(task_body(id, "tasks/get", task, capability))
            .await
    }

    pub async fn tasks_cancel(&self, id: u64, task: &str, capability: Option<&str>) -> Reply {
        self.post(task_body(id, "tasks/cancel", task, capability))
            .await
    }

    pub async fn items_list(&self, id: u64) -> Reply {
        self.post(rpc_body(
            id,
            rustain::adapters::a2a::ITEMS_LIST_METHOD,
            serde_json::json!({}),
        ))
        .await
    }

    pub async fn items_thread(&self, id: u64, item: &str, capability: Option<&str>) -> Reply {
        self.post(thread_body(id, item, capability)).await
    }

    pub async fn items_retract(&self, id: u64, item: &str, capability: Option<&str>) -> Reply {
        self.post(retract_body(id, item, capability)).await
    }
}

/// `conformance_19_18c…:1187-1189`.
pub fn rpc_body(id: u64, method: &str, params: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

/// `conformance_19_18c…:1191-1200` with the text a parameter.
pub fn send_body(
    id: u64,
    message_id: &str,
    text: &str,
    metadata: serde_json::Value,
) -> serde_json::Value {
    rpc_body(
        id,
        "message/send",
        serde_json::json!({ "message": {
            "kind": "message", "messageId": message_id, "role": "user",
            "parts": [{ "kind": "text", "text": text }], "metadata": metadata,
        }}),
    )
}

/// `{"x-rustain-in-reply-to": <item>}` — the follow-up reference on a legacy
/// (capability-less) item.
pub fn follow_up_metadata(item: &str) -> serde_json::Value {
    serde_json::json!({ rustain::adapters::a2a::REPLY_TO_METADATA_KEY: item })
}

pub fn thread_body(id: u64, item: &str, capability: Option<&str>) -> serde_json::Value {
    let mut params = serde_json::json!({ "itemId": item });
    if let Some(capability) = capability {
        params["capability"] = capability.into();
    }
    rpc_body(id, rustain::adapters::a2a::ITEMS_THREAD_METHOD, params)
}

pub fn retract_body(id: u64, item: &str, capability: Option<&str>) -> serde_json::Value {
    let mut params = serde_json::json!({ "itemId": item });
    if let Some(capability) = capability {
        params["capability"] = capability.into();
    }
    rpc_body(id, rustain::adapters::a2a::ITEMS_RETRACT_METHOD, params)
}

pub fn task_body(id: u64, method: &str, task: &str, capability: Option<&str>) -> serde_json::Value {
    let mut params = serde_json::json!({ "id": task });
    if let Some(capability) = capability {
        params["metadata"] = serde_json::json!({ "x-rustain-item-capability": capability });
    }
    rpc_body(id, method, params)
}

/// `conformance_19_18c…:978-985`: the `turnId`s of a served thread.
pub fn served_ids(thread: &serde_json::Value) -> Vec<String> {
    thread["result"]["turns"]
        .as_array()
        .expect("served turns")
        .iter()
        .map(|turn| turn["turnId"].as_str().expect("turn id").to_owned())
        .collect()
}

/// The item ids of a served `x-rustain-items/list` payload, in served order.
pub fn listed_ids(list: &serde_json::Value) -> Vec<String> {
    list["result"]["items"]
        .as_array()
        .expect("listed items")
        .iter()
        .map(|item| item["itemId"].as_str().expect("item id").to_owned())
        .collect()
}

// ── Production sender (operator `/team` rail) ───────────────────────────────

/// `conformance_19_18c…:624-683` (`Sender`, `new_sender`, `sender_over`): the
/// production egress (`A2aEgress::compose`, the same capability-key loader
/// both roots call) over the operator's `/team` command rail.
pub struct Sender {
    pub journal: Journal,
    /// This sender's data dir holding `item-capability.key`.
    pub keys: Arc<tempfile::TempDir>,
    pub app_state: AppState,
    pub domain_rx: tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
}

impl Sender {
    /// A sender with a fresh workspace and key dir, whose roster is `peers`
    /// (build entries with [`BoundHost::credentialed`]).
    pub async fn new(peers: Vec<A2aPeerSpec>) -> Self {
        let journal = open_journal(Arc::new(tempfile::tempdir().expect("sender workspace"))).await;
        let keys = Arc::new(tempfile::tempdir().expect("sender keys"));
        let (egress_tx, _egress_rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
        let egress = A2aEgress::compose(
            peers,
            NodeTree::new(),
            journal.room.clone(),
            journal.node.clone(),
            egress_tx,
            load_item_capability_key(Ok(keys.path().to_path_buf())),
        )
        .expect("compose production sender egress");
        tokio::time::timeout(SETTLE, egress.await_cards_settled())
            .await
            .expect("sender card fetches settle");
        let (mut app_state, domain_rx) = app_state(&journal.path);
        app_state.a2a_send = Some(egress.runtime().clone());
        Self {
            journal,
            keys,
            app_state,
            domain_rx,
        }
    }

    /// Type one `/team …` line through the dispatcher.
    pub async fn type_command(&mut self, state: &mut TuiState, line: &str) {
        state.input_buffer = line.to_owned();
        let InputAction::ExecuteCommand { name, args } = submit_message_for_test(state) else {
            panic!("{line:?} must reach the command dispatcher");
        };
        assert_eq!(name, "team");
        team_command(state, "conv", args.as_deref(), &self.app_state).await;
    }

    /// The next domain event the asker's spawn published.
    pub async fn next_event(&mut self) -> AppEvent {
        tokio::time::timeout(SETTLE, self.domain_rx.recv())
            .await
            .expect("an asker event before the deadline")
            .expect("domain event channel stays open")
    }

    /// `conformance_19_18c…:765-789` (`settle_send`).
    pub async fn settle_send(&mut self, state: &mut TuiState) {
        let mut tabs = rustain::domain::models::tab::TabManager::new(CancellationToken::new());
        let event = self.next_event().await;
        let AppEvent::TeamSendSettled {
            conversation_id,
            block_id,
            index,
            outcome,
        } = event
        else {
            panic!("expected TeamSendSettled, got {event:?}");
        };
        team_send_settled(
            "conv",
            state,
            &mut tabs,
            &conversation_id,
            &block_id,
            index,
            outcome,
        );
    }

    /// `conformance_19_18c…:1299-1319` (`send_item`): `/team send <peer>
    /// <text>`, settled; returns the item the recipient journaled for it.
    pub async fn send_item(
        &mut self,
        state: &mut TuiState,
        peer: &str,
        host: &BoundHost,
        text: &str,
    ) -> Item {
        let before = items(&room_events(&host.journal).await).len();
        self.type_command(state, &format!("/team send {peer} {text}"))
            .await;
        self.settle_send(state).await;
        let after = items(&room_events(&host.journal).await);
        assert_eq!(
            after.len(),
            before + 1,
            "exactly one item minted for {text:?}"
        );
        after.last().cloned().expect("item")
    }

    /// `conformance_19_18c…:846-875` (`read_thread_block`): `/team thread
    /// <peer> <item>` through the operator's rail, rendered.
    pub async fn read_thread_block(
        &mut self,
        state: &mut TuiState,
        peer: &str,
        item: &str,
    ) -> String {
        self.type_command(state, &format!("/team thread {peer} {item}"))
            .await;
        assert_eq!(
            state.feedback_blocks[TEAM_THREAD_BLOCK_ID].message, "  reading…",
            "the stable block shows its pending line before the read lands"
        );
        let event = self.next_event().await;
        let AppEvent::TeamThreadReady {
            conversation_id,
            message,
        } = event
        else {
            panic!("a thread read publishes TeamThreadReady, got {event:?}");
        };
        let mut tabs = rustain::domain::models::tab::TabManager::new(CancellationToken::new());
        team_thread_ready("conv", state, &mut tabs, &conversation_id, message);
        let block = &state.feedback_blocks[TEAM_THREAD_BLOCK_ID];
        assert_eq!(block.level, FeedbackLevel::Info);
        assert!(block.actions.is_empty());
        block.message.clone()
    }
}

/// This sender's capability key, loaded the way its root loaded it
/// (`conformance_19_18c…:1226-1231`).
pub fn sender_key(sender: &Sender) -> ItemCapabilityKey {
    ItemCapabilityKeyStore::new(sender.keys.path())
        .load()
        .expect("the sender's key")
}

/// The wire capability `key` derives for `task` (`:1233-1235`).
pub fn wire(key: &ItemCapabilityKey, task: &str) -> String {
    derive_item_capability(key, task).as_wire_str().to_owned()
}

/// A well-formed capability under a DIFFERENT key (`:1246-1249`): what a
/// co-holder of the credential can mint.
pub fn stranger(task: &str) -> String {
    wire(&ItemCapabilityKey::from_file_bytes(&[0xEE; 32]), task)
}

/// One recipient item as the host journaled it (`conformance_19_18c…:1256-1289`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub id: String,
    pub task: String,
    pub content: String,
    pub in_reply_to: Option<String>,
    pub capability_hash: Option<String>,
    pub matched: bool,
}

pub fn items(rows: &[(u64, RoomEvent)]) -> Vec<Item> {
    rows.iter()
        .filter_map(|(_, row)| match row {
            RoomEvent::RecipientItemReceived {
                address,
                task,
                content,
                in_reply_to,
                capability_hash,
                reply_capability_matched,
                ..
            } => Some(Item {
                id: address.item().as_str().to_owned(),
                task: task.clone(),
                content: content.clone(),
                in_reply_to: in_reply_to.clone(),
                capability_hash: capability_hash.clone(),
                matched: *reply_capability_matched,
            }),
            _ => None,
        })
        .collect()
}

/// `conformance_19_18c…:685-763` (`app_state`).
fn app_state(workspace: &Path) -> (AppState, tokio::sync::mpsc::UnboundedReceiver<AppEvent>) {
    use clap::Parser;
    use rustain::adapters::noop::{NoOpProvider, NoOpStorage};
    use rustain::domain::services::plan_manager::PlanManager;
    use rustain::domain::services::plan_mode_injector::DefaultPlanInjector;
    use rustain::infrastructure::composition::ComposeContext;
    use rustain::infrastructure::runtime::agent_core::AgentCore;
    use rustain::infrastructure::runtime::event_bus::EventBus;

    let approval_runtime = rustain::domain::services::approval_runtime::ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let provider_swap = Arc::new(ArcSwap::from_pointee(
        Arc::new(NoOpProvider) as Arc<dyn StreamingProvider>
    ));
    let (event_bus, domain_rx) = EventBus::new(64);
    let compose_snapshot = Arc::new(ComposeContext {
        workspace_path: workspace.to_path_buf(),
        project_context: rustain::domain::models::project_context::ProjectContext::empty(),
        storage: Arc::new(NoOpStorage) as Arc<dyn StoragePort>,
        skill_activator: Arc::new(rustain::adapters::skill_activation::SkillActivator::new()),
        mcp_servers: Vec::new(),
        include_builtin_tools: true,
        domain_tx: None,
        channel_turn_tx: None,
        tool_exposure: "static-full".into(),
        assembler: "passthrough".into(),
        skill_exposure: "l1-metadata".into(),
        skill_cache: Arc::new(rustain::infrastructure::skill_cache::SkillCache::new_in_memory()),
        sandbox_adapter: "noop".into(),
        sandbox_startup_policy: rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        sandbox_slot: Arc::new(ArcSwap::from_pointee(Arc::new(
            rustain::adapters::sandbox::NoOpSandbox,
        )
            as Arc<dyn rustain::domain::ports::SandboxManager>)),
        memory_slot: Arc::new(ArcSwap::from_pointee(
            Arc::new(rustain::adapters::noop::NoOpMemory)
                as Arc<dyn rustain::domain::ports::MemoryPort>,
        )),
        sandbox_policy: Arc::new(tokio::sync::RwLock::new(
            rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        )),
        memory_write_gate: Arc::new(tokio::sync::RwLock::new(())),
        peer_topic_store: Arc::new(rustain::adapters::rap::PeerTopicStore::new()),
        #[cfg(feature = "meta-search")]
        search_config: rustain::domain::models::SearchConfig::default(),
        #[cfg(feature = "meta-search")]
        meta_search_engine: None,
        a2a_peers: Vec::new(),
    });
    AppState::new(
        Arc::new(event_bus),
        domain_rx,
        approval_runtime,
        Arc::new(tokio::sync::RwLock::new(
            rustain::domain::models::SandboxPolicy::Permissive,
        )),
        Arc::new(PlanManager::new(workspace.to_path_buf())),
        Arc::new(DefaultPlanInjector::new()),
        provider_swap,
        Arc::new(rustain::adapters::provider::ProviderRegistry::new()),
        Arc::new(rustain::adapters::noop::NoOpUsageLedger),
        Arc::new(rustain::adapters::budget::BudgetStateStore::new()),
        Arc::new(ArcSwap::from_pointee(AppConfig::default())),
        Arc::new(AgentCore::test_noop()),
        None,
        compose_snapshot,
        Arc::new(ArcSwap::from_pointee(Arc::new(
            rustain::adapters::profile_resolver::noop::NoopProfileResolver,
        )
            as Arc<dyn rustain::domain::ports::ProfileResolver>)),
        rustain::adapters::cli::commands::Cli::try_parse_from(["rustain"]).expect("bare cli"),
        None,
        rustain::infrastructure::telemetry::ActiveRatioWindow::new_in_memory(),
        #[cfg(feature = "meta-search")]
        None,
    )
}

// ── K05 additions (append-only) ─────────────────────────────────────────────

/// [`log_since`] narrowed to the lines this host's `serve` task logged (its
/// span carries the workspace), so a parallel test's lines never leak in.
pub fn host_log_since(cursor: usize, host: &BoundHost) -> String {
    let marker = host.journal.path.display().to_string();
    log_since(cursor)
        .lines()
        .filter(|line| line.contains(&marker))
        .collect::<Vec<_>>()
        .join("\n")
}

impl Operator {
    /// `/team remove <item>` over the attach socket: the production removal
    /// frame (`RemoveRecipientItem`). The daemon answers a success with no
    /// frame, so callers wait for the `recipient_item_removed` journal row.
    pub async fn remove_item(&mut self, item_id: &str) {
        write_frame(
            &mut self.stream,
            &ClientFrame::RemoveRecipientItem {
                item_id: item_id.to_owned(),
            },
        )
        .await
        .expect("operator frame");
    }
}

// ── K11/K14 additions (append-only) ─────────────────────────────────────────

/// The operator's `AppState` over `workspace` with `peers` as its composed
/// roster (`ComposeContext.a2a_peers`): what `/team untrust <alias>` resolves
/// the alias against. The host's own workspace makes the command write to the
/// host's durable journal (`change_sender_consent`).
pub fn operator_app_state(
    workspace: &Path,
    peers: Vec<A2aPeerSpec>,
) -> (AppState, tokio::sync::mpsc::UnboundedReceiver<AppEvent>) {
    let (mut state, rx) = app_state(workspace);
    let mut context = (*state.compose_snapshot).clone();
    context.a2a_peers = peers;
    state.compose_snapshot = Arc::new(context);
    (state, rx)
}

// ── K05 crash restart (append-only) ─────────────────────────────────────────

impl BoundHost {
    /// [`Self::restart`] as a CRASH would leave the host: this in-process
    /// teardown drops the approval runtime, which declines every parked
    /// admission and so removes its `a2a-pending.json` record — something a
    /// dead process never does. The file's bytes from BEFORE the teardown are
    /// put back once the teardown has settled, so the new `serve` finds
    /// exactly what a killed process left on disk.
    ///
    /// Story 19.18e `P6` — each declined watcher removes its own record in a
    /// separate read-modify-write that shares no lock with the new server, so
    /// with several parked records a late writer could clobber the restored
    /// bytes. Settle (bounded: the file has changed and then held still for
    /// five 20 ms polls, or 1 s passed — some seeds never change it), restore,
    /// then verify five reads at 20 ms; any difference rewrites, at most three
    /// rounds, else the restart panics rather than serve a lost seed.
    pub async fn restart_after_crash(self, spec: HostSpec) -> Self {
        let pending_path = self.journal.path.join(".rustain/a2a-pending.json");
        let snapshot = std::fs::read(&pending_path).ok();
        let wanted = spec.listen;
        let listener = if wanted == self.listen {
            Some(self.listener.try_clone().expect("keep the one socket"))
        } else {
            None
        };
        self.cancel.cancel();
        self.http.await.expect("server task").expect("shutdown");
        let Self {
            journal,
            listener: old_listener,
            keys,
            server,
            node_tree,
            core,
            _singleton,
            ..
        } = self;
        drop(old_listener);
        drop(server);
        drop(node_tree);
        drop(core);
        drop(_singleton);
        let workspace = journal.workspace.clone();
        drop(journal);
        let _ = std::fs::remove_file(workspace.path().join("daemon.sock"));
        if let Some(bytes) = snapshot {
            let read = || std::fs::read(&pending_path).ok();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
            let mut last = read();
            let mut changed = last.as_deref() != Some(bytes.as_slice());
            let mut stable = 0;
            while !(changed && stable >= 5) && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(20)).await;
                let now = read();
                changed |= now.as_deref() != Some(bytes.as_slice());
                if now == last {
                    stable += 1;
                } else {
                    stable = 0;
                    last = now;
                }
            }
            let mut rounds = 0;
            loop {
                std::fs::write(&pending_path, &bytes).expect("restore the pending file");
                let mut clobbered = false;
                for _ in 0..5 {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    if read().as_deref() != Some(bytes.as_slice()) {
                        clobbered = true;
                        break;
                    }
                }
                if !clobbered {
                    break;
                }
                rounds += 1;
                if rounds == 3 {
                    panic!("late pending-file writer clobbered the crash snapshot");
                }
            }
        }
        let listener = listener.unwrap_or_else(|| bind(wanted));
        Self::compose(open_journal(workspace).await, listener, keys, spec, true).await
    }
}

// ── Promoted from k05/k06/k11 (Story 19.18e `A11`; bodies unchanged) ────────

/// `admission` + `peerKeys` (env NAME → alias or `{peer, adoptPreBinding}`)
/// + `apiKeys` (unbound env NAMEs).
pub fn server(
    admission: &str,
    peer_keys: serde_json::Value,
    api_keys: &[&str],
) -> serde_json::Value {
    let mut block = serde_json::json!({ "admission": admission });
    if peer_keys.as_object().is_some_and(|keys| !keys.is_empty()) {
        block["peerKeys"] = peer_keys;
    }
    if !api_keys.is_empty() {
        block["apiKeys"] = serde_json::json!(api_keys);
    }
    block
}

/// A `peerKeys` value binding to `peer` with the pre-binding adoption opt-in.
pub fn adopting(peer: &str) -> serde_json::Value {
    serde_json::json!({ "peer": peer, "adoptPreBinding": true })
}

/// A [`server`] block over the standard roster.
pub fn spec(server: serde_json::Value) -> HostSpec {
    HostSpec::with_server(server).roster(Roster::standard())
}

/// [`BoundHost::restart_after_crash`] returning the host plus its
/// phase-local log: the pending record a killed process leaves behind
/// survives into the new `serve`.
pub async fn restart_phase_after_crash(host: BoundHost, spec: HostSpec) -> (BoundHost, String) {
    let cursor = log_cursor();
    let host = host.restart_after_crash(spec).await;
    let window = host_log_since(cursor, &host);
    (host, window)
}

/// Bounded wait until `a2a-pending.json` names `task` (an approval-pending
/// task; ask mode, no durable item yet).
pub async fn wait_pending_record(host: &BoundHost, task: &str) {
    let path = host.journal.path.join(".rustain/a2a-pending.json");
    let deadline = tokio::time::Instant::now() + SETTLE;
    loop {
        if std::fs::read_to_string(&path).is_ok_and(|raw| raw.contains(task)) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no pending record for {task}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Seed `K`'s in-flight task WITHOUT a durable item: `admission: ask` parks
/// the task on an operator decision and writes only `a2a-pending.json`.
pub async fn park_in_flight(host: &BoundHost, client: &RawClient, task: &str) {
    let items_before = received_rows(&host.journal).len();
    let sent = client
        .message_send(1, task, task, serde_json::json!({}))
        .await;
    assert_eq!(
        sent.json["result"]["status"]["state"], "auth-required",
        "{task} must park on the operator decision: {}",
        sent.body
    );
    wait_pending_record(host, task).await;
    assert_eq!(
        received_rows(&host.journal).len(),
        items_before,
        "an approval-pending task has no durable item"
    );
}

/// The restart warn lines the two passes emit per restored task
/// (`server.rs`, `reconcile_after_restart` / `reconcile_pending_after_restart`).
pub const ORPHAN_WARN: &str = "A2A inbound task failed by host restart";
pub const PENDING_WARN: &str = "A2A approval-pending task failed by host restart";

pub fn wire_state(reply: &Reply) -> &str {
    reply.json["result"]["status"]["state"]
        .as_str()
        .unwrap_or("<no state>")
}

pub fn detail(reply: &Reply) -> Option<&str> {
    reply.json["result"]["status"]["message"]["parts"][0]["text"].as_str()
}

/// Log lines (after `cursor`) carrying `message` AND `needle`: other tests in
/// this binary write to the same buffer, so a count is only ever taken over a
/// token unique to this test (a task id, a node id, this workspace's path).
pub fn lines_with(cursor: usize, message: &str, needle: &str) -> usize {
    log_since(cursor)
        .lines()
        .filter(|line| line.contains(message) && line.contains(needle))
        .count()
}

/// Send `task` through `client` and release it with `[o]nce` on the plain
/// approval card: the task launches, journals its item and parks `Waiting`
/// (`NotifyAndWait`). Returns the durable item id.
pub async fn send_and_admit(
    host: &BoundHost,
    operator: &mut Operator,
    client: &RawClient,
    task: &str,
) -> String {
    let sent = client
        .message_send(1, task, task, serde_json::json!({}))
        .await;
    assert_eq!(wire_state(&sent), "auth-required", "{task}: {}", sent.body);
    operator
        .answer_card("a2a/message.send", ApprovalOutcome::Once)
        .await;
    let node = host.waiting_node(task).await;
    host.pending_card(&node).await;
    item_id_for_task(&host.journal, task)
}

/// Send `task` and leave the approval unanswered: the task is
/// `auth-required`, has a pending-file record and NO node.
pub async fn send_and_leave_pending(host: &BoundHost, client: &RawClient, task: &str) {
    let sent = client
        .message_send(1, task, task, serde_json::json!({}))
        .await;
    assert_eq!(wire_state(&sent), "auth-required", "{task}: {}", sent.body);
    assert!(
        !has_inbound_node(host, task).await,
        "{task}: an unanswered approval has no node (only the pending file can restore it)"
    );
}

pub async fn has_inbound_node(host: &BoundHost, task: &str) -> bool {
    let suffix = format!("/t-{}", URL_SAFE_NO_PAD.encode(task));
    host.node_tree.list().await.iter().any(|entry| {
        entry.subagent_type == INBOUND_SUBAGENT_TYPE && entry.agent_id.as_str().ends_with(&suffix)
    })
}

/// No inbound node of this host is still live: recovery failed every orphan
/// and nothing resumed.
pub async fn no_live_inbound_node(host: &BoundHost) -> bool {
    host.node_tree.list().await.iter().all(|entry| {
        entry.subagent_type != INBOUND_SUBAGENT_TYPE || entry.current_status.is_terminal()
    })
}

/// `tasks/get` answers the same refusal a never-seen task id gets: an error,
/// no result, and the same code and message as `control`.
pub fn assert_not_found_like(reply: &Reply, control: &Reply, what: &str) {
    assert!(reply.json["result"].is_null(), "{what}: {}", reply.body);
    assert!(!reply.json["error"].is_null(), "{what}: {}", reply.body);
    assert_eq!(
        reply.json["error"]["code"], control.json["error"]["code"],
        "{what}: {}",
        reply.body
    );
    assert_eq!(
        reply.json["error"]["message"], control.json["error"]["message"],
        "{what}: {}",
        reply.body
    );
}

/// `(spoke, principal)` of every first-status-query row for `task`.
pub fn status_rows(host: &BoundHost, task: &str) -> Vec<(String, Option<String>)> {
    let gate = format!("{STATUS_QUERY_GATE_PREFIX}{task}");
    raw_payloads(&host.journal, "admission_deferred")
        .into_iter()
        .filter(|row| row["gate"] == gate)
        .map(|row| {
            (
                row["spoke"].as_str().expect("spoke").to_owned(),
                row["principal"].as_str().map(str::to_owned),
            )
        })
        .collect()
}
