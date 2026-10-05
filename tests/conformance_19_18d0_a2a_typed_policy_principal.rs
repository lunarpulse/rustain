//! Story 19.18d0 — typed policy principal, A2A front-door keystones.
//!
//! `K04` drives the production `serve(ServeConfig)` over the production
//! `AttachServer` composed with a `JournalConsentProjection` (loopback and TLS,
//! two distinct presenting API keys), through raw JSON-RPC and the operator's
//! attach socket, and proves:
//!
//! - the principal the handler hands to `request_admission_approval`,
//!   `response_policy` and `InboundPeerTask.principal` is exactly
//!   `Pseudonymous(<presenter's own pseudonymous_peer_id()>)`;
//! - the typed consent grant, the approval-parked row and the first-status-query
//!   row are journaled `"principal":"pseudonymous"` with the presenter's
//!   pseudonym as `spoke`/`sender`, exactly one status-query row per task;
//! - a cold restart reseeds the status-query observation through BOTH restart
//!   blocks (orphan reconcile and pending-file reconcile): a `Pseudonymous` row
//!   and a legacy (`principal` absent) row deduplicate, a planted `Pinned` or
//!   `Unknown` row with the same `spoke`/gate does not.
//!
//! Mutants this target must turn RED: `M02`, `M31`–`M37`, `M52`.
//!
//! The harness below is COPIED (never imported — the source file's helpers are
//! file-private) from `tests/conformance_19_18c_a2a_item_capability.rs`, with
//! only what `K04` needs; the line numbers are that file's.

#![cfg(feature = "a2a")]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Once};
use std::time::Duration;

use arc_swap::ArcSwap;
use futures::stream::BoxStream;
use rustain::adapters::a2a::admission::{A2aAdmissionPolicy, ThreadReads};
use rustain::adapters::a2a::auth::{A2aServerSecurity, API_KEY_HEADER};
use rustain::adapters::a2a::card_cache::SignedCardCache;
use rustain::adapters::a2a::config::{A2aServerConfig, parse_workspace_a2a_server_config};
use rustain::adapters::a2a::exec::{SubmitterKey, mint_inbound_node_id};
use rustain::adapters::a2a::server::{ServeConfig, serve};
use rustain::adapters::a2a::transparency::{
    APPROVAL_GATE_PREFIX, STATUS_QUERY_GATE_PREFIX, TransparencySink,
};
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
use rustain::domain::errors::ProviderError;
use rustain::domain::events::AppEvent;
use rustain::domain::models::capability_registry::CapabilityRegistry;
use rustain::domain::models::{
    AgentId, AppConfig, ApprovalOutcome, ApprovalScope, CompletionOptions, Conversation,
    JournalRecord, Message, NodeState, PeerId, PolicyPrincipal, PrincipalKind, ResponseMode,
    RoomEvent, SemanticMessageType, StopReason, StreamChunk,
};
use rustain::domain::ports::{
    DeliveryPolicy, InboundApprovalTicket, InboundPeerError, InboundPeerRuntime, InboundPeerTask,
    PeerResponsePolicy, RoomJournal, RoomJournalReader, SecurityPort, StoragePort,
    StreamingProvider, ToolSetPort,
};
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::infrastructure::subagent::{NodeJournal, NodeRoomJournal, NodeTree};
use tokio_util::sync::CancellationToken;

// `conformance_19_18c_a2a_item_capability.rs:100`.
const SETTLE: Duration = Duration::from_secs(45);

// ── Copied harness ──────────────────────────────────────────────────────────

/// `conformance_19_18c_a2a_item_capability.rs:102-127` (`Journal`,
/// `open_journal`) — unchanged except that the zero-argument `journal()` helper
/// is inlined into the host builders.
#[derive(Clone)]
struct Journal {
    workspace: Arc<tempfile::TempDir>,
    path: PathBuf,
    node: Arc<NodeJournal>,
    room: Arc<dyn RoomJournal>,
}

async fn open_journal(workspace: Arc<tempfile::TempDir>) -> Journal {
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

/// `conformance_19_18c_a2a_item_capability.rs:129-152` (`ModePolicy`).
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
        _principal: &PolicyPrincipal,
        _message_type: SemanticMessageType,
    ) -> PeerResponsePolicy {
        PeerResponsePolicy {
            mode: self.0,
            auto_response: None,
            ..Default::default()
        }
    }
}

/// `conformance_19_18c_a2a_item_capability.rs:154-196` (`ScriptedProvider`).
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

/// `conformance_19_18c_a2a_item_capability.rs:250-291` (`build_runtime`).
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

/// `conformance_19_18c_a2a_item_capability.rs:293`.
const CONVERSATION_ID: &str = "reply-recipient";

// ── The runtime recorder ────────────────────────────────────────────────────

/// One `InboundPeerRuntime::start` the handler made.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Start {
    node: String,
    text: String,
    principal: PolicyPrincipal,
}

/// What the `serve` handler PASSED to the runtime. The wrapper below only
/// records the arguments it receives (never constructs a principal of its own)
/// and delegates every call to the real `AttachServer`.
#[derive(Default)]
struct Observed {
    admissions: std::sync::Mutex<Vec<(String, PolicyPrincipal)>>,
    policies: std::sync::Mutex<Vec<PolicyPrincipal>>,
    starts: std::sync::Mutex<Vec<Start>>,
}

fn snapshot<T: Clone>(slot: &std::sync::Mutex<Vec<T>>) -> Vec<T> {
    slot.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

fn record<T>(slot: &std::sync::Mutex<Vec<T>>, value: T) {
    slot.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(value);
}

struct Recording {
    inner: Arc<AttachServer>,
    observed: Arc<Observed>,
}

#[async_trait::async_trait]
impl InboundPeerRuntime for Recording {
    fn response_policy(
        &self,
        principal: &PolicyPrincipal,
        message_type: SemanticMessageType,
    ) -> PeerResponsePolicy {
        record(&self.observed.policies, principal.clone());
        InboundPeerRuntime::response_policy(self.inner.as_ref(), principal, message_type)
    }

    async fn start(
        &self,
        task: InboundPeerTask,
        cancel: CancellationToken,
    ) -> Result<tokio::sync::watch::Receiver<NodeState>, InboundPeerError> {
        record(
            &self.observed.starts,
            Start {
                node: task.node_id.as_str().to_owned(),
                text: task.text.clone(),
                principal: task.principal.clone(),
            },
        );
        InboundPeerRuntime::start(self.inner.as_ref(), task, cancel).await
    }

    async fn park_pending_consent(
        &self,
        node_id: &AgentId,
        subagent_type: &str,
        cancel: CancellationToken,
    ) -> Result<(), InboundPeerError> {
        InboundPeerRuntime::park_pending_consent(
            self.inner.as_ref(),
            node_id,
            subagent_type,
            cancel,
        )
        .await
    }

    async fn discard_pending_consent_node(&self, node_id: &AgentId) {
        InboundPeerRuntime::discard_pending_consent_node(self.inner.as_ref(), node_id).await;
    }

    fn enforces_sender_consent(&self) -> bool {
        InboundPeerRuntime::enforces_sender_consent(self.inner.as_ref())
    }

    async fn request_admission_approval(
        &self,
        principal: &PolicyPrincipal,
        summary: &str,
    ) -> Result<InboundApprovalTicket, InboundPeerError> {
        record(
            &self.observed.admissions,
            (summary.to_owned(), principal.clone()),
        );
        InboundPeerRuntime::request_admission_approval(self.inner.as_ref(), principal, summary)
            .await
    }

    async fn take_result_text(&self, node_id: &AgentId) -> Option<String> {
        InboundPeerRuntime::take_result_text(self.inner.as_ref(), node_id).await
    }

    async fn disclosure_forbidden_fragments(&self) -> Vec<String> {
        InboundPeerRuntime::disclosure_forbidden_fragments(self.inner.as_ref()).await
    }

    async fn reconcile_orphaned_tasks(&self, subagent_type: &str) -> Vec<AgentId> {
        InboundPeerRuntime::reconcile_orphaned_tasks(self.inner.as_ref(), subagent_type).await
    }
}

// ── Host ────────────────────────────────────────────────────────────────────

/// `conformance_19_18c_a2a_item_capability.rs:295-331` (`HostOptions`),
/// reduced to what `K04` composes; `observed` is the runtime recorder.
#[derive(Clone)]
struct HostOptions {
    policy: A2aAdmissionPolicy,
    mode: ResponseMode,
    security: A2aServerSecurity,
    bind: &'static str,
    /// Compose the AttachServer with a consent projection, as production does.
    consent: bool,
    observed: Arc<Observed>,
}

/// `conformance_19_18c_a2a_item_capability.rs:333-345` (`Host`).
struct Host {
    journal: Journal,
    endpoint: String,
    listener: std::net::TcpListener,
    keys: Arc<tempfile::TempDir>,
    options: HostOptions,
    cancel: CancellationToken,
    http: tokio::task::JoinHandle<anyhow::Result<()>>,
    server: Arc<AttachServer>,
    node_tree: NodeTree,
    domain_rx: Option<tokio::sync::mpsc::UnboundedReceiver<AppEvent>>,
    _singleton: Option<rustain::infrastructure::subagent::DaemonSingletonLock>,
}

impl Host {
    /// `conformance_19_18c_a2a_item_capability.rs:352-357` (`serve_with`).
    async fn serve_with(options: HostOptions) -> Self {
        let listener = std::net::TcpListener::bind(options.bind).expect("bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let keys = Arc::new(tempfile::tempdir().expect("keys"));
        let journal = open_journal(Arc::new(tempfile::tempdir().expect("workspace"))).await;
        Self::compose(journal, listener, keys, options, false).await
    }

    /// `conformance_19_18c_a2a_item_capability.rs:359-523` (`compose`): the
    /// node tree bound to this host (`:367-377`), the restart recovery
    /// (`:378-394`), the consent projection folded from the journal
    /// (`:450-459`) and the `serve` wiring (`:487-509`). The one deliberate
    /// difference: `ServeConfig.runtime` is the recording wrapper around the
    /// same `AttachServer`.
    async fn compose(
        journal: Journal,
        listener: std::net::TcpListener,
        keys: Arc<tempfile::TempDir>,
        options: HostOptions,
        restarted: bool,
    ) -> Self {
        let (domain_tx, domain_rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
        // Bound to this host as the daemon composes it (`daemon/mod.rs:381`):
        // recovery restores only this host's nodes, so without the binding a
        // restart would see no orphan.
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
        let delivery: Arc<dyn DeliveryPolicy> = Arc::new(ModePolicy(options.mode));
        let bus = rustain::adapters::daemon::server::peer_bus_slot_with_policy(
            &node_tree,
            delivery.clone(),
        );
        let consent = if options.consent {
            let entries = RoomJournalReader::load_entries(journal.node.as_ref())
                .await
                .expect("consent entries");
            Some(Arc::new(
                rustain::adapters::policy::JournalConsentProjection::from_entries(&entries),
            ))
        } else {
            None
        };
        let server = AttachServer::new_with_node_tree_bus_policy_and_journal(
            core,
            Arc::new(tokio::sync::Mutex::new(conversation)),
            domain_tx,
            node_tree.clone(),
            bus,
            delivery,
            journal.room.clone(),
            journal.node.clone(),
            consent,
        );
        let signer = IdentityKeyStore::new(keys.path())
            .load_or_generate()
            .expect("identity");
        let tokio_listener = tokio::net::TcpListener::from_std(
            listener
                .try_clone()
                .expect("clone the one listening socket"),
        )
        .expect("tokio listener");
        let address = listener.local_addr().expect("address");
        let tls = options.security.tls.is_some();
        let endpoint = if tls {
            format!("https://localhost:{}/", address.port())
        } else {
            format!("http://{address}/")
        };
        let runtime: Arc<dyn InboundPeerRuntime> = Arc::new(Recording {
            inner: server.clone(),
            observed: options.observed.clone(),
        });
        let cancel = CancellationToken::new();
        let http = tokio::spawn(serve(
            tokio_listener,
            ServeConfig {
                registry: Arc::new(CapabilityRegistry::new(None)),
                signer,
                security: options.security.clone(),
                runtime: Some(runtime),
                // The sink carries the READER, as production composes it
                // (`daemon/mod.rs:596-599`): the restart blocks read the
                // status-query rows through it.
                transparency: Arc::new(
                    TransparencySink::new(journal.room.clone()).with_reader(journal.node.clone()),
                ),
                policy: options.policy,
                workspace: journal.path.clone(),
                advertised_host: tls.then(|| format!("localhost:{}", address.port())),
                cards: Arc::new(SignedCardCache::new()),
            },
            cancel.child_token(),
        ));
        Self {
            journal,
            endpoint,
            listener,
            keys,
            options,
            cancel,
            http,
            server,
            node_tree,
            domain_rx: Some(domain_rx),
            _singleton: singleton,
        }
    }

    /// `conformance_19_18c_a2a_item_capability.rs:541-569` (`restart`,
    /// `restart_with`): same socket, keys and workspace; the old process's
    /// singleton lock is released first.
    async fn restart(self) -> Self {
        let listener = self.listener.try_clone().expect("keep the one socket");
        self.cancel.cancel();
        self.http.await.expect("server task").expect("shutdown");
        let Self {
            journal,
            listener: old_listener,
            keys,
            options,
            server,
            node_tree,
            _singleton,
            ..
        } = self;
        drop(old_listener);
        drop(server);
        drop(node_tree);
        drop(_singleton);
        let workspace = journal.workspace.clone();
        drop(journal);
        let _ = std::fs::remove_file(workspace.path().join("daemon.sock"));
        Self::compose(open_journal(workspace).await, listener, keys, options, true).await
    }
}

/// `conformance_19_18c_a2a_item_capability.rs:572-622` (`attach_operator`).
async fn attach_operator(host: &mut Host) -> (tokio::net::UnixStream, CancellationToken) {
    let socket = host.journal.path.join("daemon.sock");
    let listener = tokio::net::UnixListener::bind(&socket).expect("bind unix socket");
    let shutdown = CancellationToken::new();
    tokio::spawn(host.server.clone().run(
        listener,
        host.domain_rx.take().expect("domain rx"),
        None,
        None,
        shutdown.child_token(),
    ));
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
    (stream, shutdown)
}

// ── Environment, log capture ────────────────────────────────────────────────

/// Two configured API keys: two distinct presenting principals. The server
/// block of the host's written `a2a.json` names the environment variables
/// (`apiKeyEnv` + `apiKeys`), exactly as the daemon reads them.
const KEY_ONE: &str = "rustain-19-18d0-key-one";
const KEY_TWO: &str = "rustain-19-18d0-key-two";
const KEY_ONE_ENV: &str = "RUSTAIN_19_18D0_KEY_ONE";
const KEY_TWO_ENV: &str = "RUSTAIN_19_18D0_KEY_TWO";

static SETUP: Once = Once::new();

/// One `Once` sets the file-private credential variables before ingress reads them.
fn setup() {
    SETUP.call_once(|| {
        // SAFETY: set exactly once, before any test in this binary reads them;
        // the names are unique to this file.
        unsafe {
            std::env::set_var(KEY_ONE_ENV, KEY_ONE);
            std::env::set_var(KEY_TWO_ENV, KEY_TWO);
        }
    });
}

// ── Peers: written a2a.json, TLS material, anchored client ──────────────────

/// `conformance_19_18c_a2a_item_capability.rs:1068-1080` (`TlsPeer`),
/// generalised: a served host plus the directory holding its written
/// `a2a.json` (and, for the TLS peer, its certificate).
struct Peer {
    host: Host,
    _dir: tempfile::TempDir,
    cert_path: Option<PathBuf>,
}

impl Peer {
    fn port(&self) -> u16 {
        self.host.listener.local_addr().expect("address").port()
    }

    async fn restart(self) -> Self {
        let Self {
            host,
            _dir,
            cert_path,
        } = self;
        Self {
            host: host.restart().await,
            _dir,
            cert_path,
        }
    }
}

/// Write `a2a.json` with `server` as its server block and parse it the way the
/// daemon does (⛔ the settings are never constructed).
fn written_server_config(dir: &Path, server: serde_json::Value) -> A2aServerConfig {
    let config_path = dir.join("a2a.json");
    std::fs::write(
        &config_path,
        serde_json::json!({ "server": server }).to_string(),
    )
    .expect("a2a.json");
    parse_workspace_a2a_server_config(&config_path)
        .expect("parse")
        .expect("server block")
}

/// `conformance_19_18c_a2a_item_capability.rs:1082-1131` (`tls_host`): a
/// non-loopback TLS host with API-key auth. The admission policy and the key
/// names come out of the written `a2a.json`; the key values come from the
/// environment `setup()` installed.
async fn keyed_peer(mode: ResponseMode, consent: bool) -> Peer {
    let dir = tempfile::tempdir().expect("tls dir");
    let config = written_server_config(
        dir.path(),
        serde_json::json!({
            "admission": "allow",
            "apiKeyEnv": KEY_ONE_ENV,
            "apiKeys": [KEY_TWO_ENV],
        }),
    );
    // Story 19.18d `F12` — the production resolver reads the written names
    // and the values `setup()` installed; ⛔ never a hand-built key set.
    let auth = rustain::adapters::a2a::auth::resolve_server_auth(&config, &[], |name| {
        std::env::var(name).ok()
    })
    .expect("the written a2a.json resolves")
    .expect("the written a2a.json names keys");
    let cert =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("certificate");
    let (cert_path, key_path) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
    std::fs::write(&cert_path, cert.cert.pem()).expect("cert");
    std::fs::write(&key_path, cert.signing_key.serialize_pem()).expect("key");
    let material =
        rustain::adapters::a2a::tls::load_tls_material(&cert_path, &key_path).expect("tls");
    let options = HostOptions {
        policy: config.admission,
        mode,
        security: A2aServerSecurity {
            tls: Some(material),
            auth: Some(auth),
            thread_reads: config.thread_reads.unwrap_or(ThreadReads::Off),
        },
        bind: "0.0.0.0:0",
        consent,
        observed: Arc::new(Observed::default()),
    };
    Peer {
        host: Host::serve_with(options).await,
        _dir: dir,
        cert_path: Some(cert_path),
    }
}

/// A plaintext loopback host whose admission policy comes out of its written
/// `a2a.json` (`ask`: every task parks on an operator decision).
async fn loopback_peer(mode: ResponseMode, consent: bool) -> Peer {
    let dir = tempfile::tempdir().expect("loopback dir");
    let config = written_server_config(dir.path(), serde_json::json!({ "admission": "ask" }));
    let options = HostOptions {
        policy: config.admission,
        mode,
        security: A2aServerSecurity::default(),
        bind: "127.0.0.1:0",
        consent,
        observed: Arc::new(Observed::default()),
    };
    Peer {
        host: Host::serve_with(options).await,
        _dir: dir,
        cert_path: None,
    }
}

/// `conformance_19_18c_a2a_item_capability.rs:1147-1183` (`Raw`): a raw
/// JSON-RPC client. The TLS client is ANCHORED on the host's own certificate
/// (⛔ never `danger_accept_invalid_certs`); the loopback client is plain.
struct Presenter {
    url: String,
    key: Option<&'static str>,
    client: reqwest::Client,
}

fn anchored(peer: &Peer, key: &'static str) -> Presenter {
    let pem = std::fs::read(peer.cert_path.as_ref().expect("a TLS peer")).expect("anchor pem");
    let anchor = reqwest::Certificate::from_pem(&pem).expect("anchor certificate");
    Presenter {
        url: format!("https://localhost:{}/", peer.port()),
        key: Some(key),
        client: reqwest::Client::builder()
            .use_rustls_tls()
            .tls_built_in_root_certs(false)
            .add_root_certificate(anchor)
            .build()
            .expect("anchored client"),
    }
}

fn loopback(peer: &Peer) -> Presenter {
    Presenter {
        url: peer.host.endpoint.clone(),
        key: None,
        client: reqwest::Client::new(),
    }
}

impl Presenter {
    async fn call(&self, body: serde_json::Value) -> serde_json::Value {
        let mut request = self.client.post(&self.url).json(&body);
        if let Some(key) = self.key {
            request = request.header(API_KEY_HEADER, key);
        }
        request
            .send()
            .await
            .expect("post")
            .json()
            .await
            .expect("json")
    }

    async fn send(&self, task: &str) -> serde_json::Value {
        self.call(rpc_body(
            1,
            "message/send",
            serde_json::json!({ "message": {
                "kind": "message", "messageId": task, "role": "user",
                "parts": [{ "kind": "text", "text": task }], "metadata": {},
            }}),
        ))
        .await
    }

    async fn get(&self, task: &str) -> serde_json::Value {
        self.call(rpc_body(2, "tasks/get", serde_json::json!({ "id": task })))
            .await
    }
}

/// `conformance_19_18c_a2a_item_capability.rs:1185-1187`.
fn rpc_body(id: u64, method: &str, params: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

fn wire_state(response: &serde_json::Value) -> &str {
    response["result"]["status"]["state"]
        .as_str()
        .unwrap_or("<no state>")
}

// ── Journal readers and operator answers ────────────────────────────────────

/// `conformance_19_18c_a2a_item_capability.rs:791-801` (`room_events`).
async fn room_events(journal: &Journal) -> Vec<(u64, RoomEvent)> {
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

/// Every `AdmissionDeferred` row under `prefix` for `task`, as
/// `(spoke, principal)`, in journal order.
async fn deferred_rows(
    journal: &Journal,
    prefix: &str,
    task: &str,
) -> Vec<(String, Option<PrincipalKind>)> {
    let wanted = format!("{prefix}{task}");
    room_events(journal)
        .await
        .into_iter()
        .filter_map(|(_, row)| match row {
            RoomEvent::AdmissionDeferred {
                spoke,
                gate,
                principal,
                ..
            } if gate == wanted => Some((spoke, principal)),
            _ => None,
        })
        .collect()
}

/// Every `ConsentGranted` row, as `(sender, principal)`, in journal order.
async fn grants(journal: &Journal) -> Vec<(Option<PeerId>, Option<PrincipalKind>)> {
    room_events(journal)
        .await
        .into_iter()
        .filter_map(|(_, row)| match row {
            RoomEvent::ConsentGranted {
                sender, principal, ..
            } => Some((sender, principal)),
            _ => None,
        })
        .collect()
}

/// The durable journal's raw JSONL lines whose `payload.event` is `event`
/// (`AC6(b)`: what an older reader would actually see).
fn raw_payloads(journal: &Journal, event: &str) -> Vec<serde_json::Value> {
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

/// Plant a status-query row as an older or a different build would have
/// written it (the reader, not the writer, is the question).
async fn plant_status_row(
    journal: &Journal,
    spoke: &PeerId,
    task: &str,
    principal: Option<PrincipalKind>,
) {
    journal
        .room
        .record_event(RoomEvent::AdmissionDeferred {
            coordinator: AgentId::root(),
            spoke: spoke.as_str().to_owned(),
            gate: format!("{STATUS_QUERY_GATE_PREFIX}{task}"),
            principal,
        })
        .await
        .expect("plant a status-query row");
}

/// Answer the next forwarded approval card (`tool`) the operator's socket
/// carries, with `outcome`, through the real frame (19.18's `approve_next`).
async fn answer_card(operator: &mut tokio::net::UnixStream, tool: &str, outcome: ApprovalOutcome) {
    let request_id = loop {
        if let DaemonFrame::ApprovalRequest {
            request_id,
            tool: raised,
            ..
        } = tokio::time::timeout(SETTLE, read_frame::<_, DaemonFrame>(operator))
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
        operator,
        &ClientFrame::ApprovalResponse {
            request_id,
            outcome,
        },
    )
    .await
    .expect("answer the card");
}

/// `[a]` on a sender-consent card: the attach protocol an operator uses.
async fn always_allow(operator: &mut tokio::net::UnixStream) {
    answer_card(
        operator,
        "a2a/sender-consent",
        ApprovalOutcome::AlwaysAndSave {
            scope: ApprovalScope::Tool("a2a/sender-consent".to_owned()),
        },
    )
    .await;
}

/// Bounded wait (never a bare sleep) for the handler to have started `text`.
async fn launched(observed: &Observed, text: &str) -> Start {
    let deadline = tokio::time::Instant::now() + SETTLE;
    loop {
        if let Some(start) = snapshot(&observed.starts)
            .into_iter()
            .find(|start| start.text == text)
        {
            return start;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{text:?} was never started"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ── Expectations (the test's own, never the product funnel) ─────────────────

/// What the handler must pass for a presenter: `Pseudonymous` over THAT
/// presenter's `pseudonymous_peer_id()`. ⛔ Deliberately not
/// `SubmitterKey::policy_principal()` — the funnel under test (`M02`).
fn presenter_principal(key: &SubmitterKey) -> PolicyPrincipal {
    PolicyPrincipal::Pseudonymous(key.pseudonymous_peer_id())
}

fn expected_start(key: &SubmitterKey, task: &str) -> Start {
    Start {
        node: mint_inbound_node_id(key, task).as_str().to_owned(),
        text: task.to_owned(),
        principal: presenter_principal(key),
    }
}

async fn node_status(host: &Host, key: &SubmitterKey, task: &str) -> Option<NodeState> {
    let wanted = mint_inbound_node_id(key, task);
    host.node_tree
        .list()
        .await
        .into_iter()
        .find(|entry| entry.agent_id.as_str() == wanted.as_str())
        .map(|entry| entry.current_status)
}

/// Bounded wait for `task`'s node to be parked `Waiting` (the
/// `NotifyAndWait` shape), i.e. a live node a restart will have to recover.
async fn await_waiting(host: &Host, key: &SubmitterKey, task: &str) {
    let deadline = tokio::time::Instant::now() + SETTLE;
    while node_status(host, key, task).await != Some(NodeState::Waiting) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{task}: no waiting node"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The pending file's records as `(task_id, submitter)`.
fn pending_records(host: &Host) -> Vec<(String, String)> {
    let path = host.journal.path.join(".rustain/a2a-pending.json");
    let records: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(&path).expect("the pending file"))
            .expect("the pending file is a JSON array");
    records
        .iter()
        .map(|record| {
            (
                record["task_id"].as_str().expect("task_id").to_owned(),
                record["submitter"].as_str().expect("submitter").to_owned(),
            )
        })
        .collect()
}

/// A task's FIRST `tasks/get` as its owner, twice: exactly one status-query row,
/// journaled `pseudonymous` under the owner's pseudonym (`AC6(b)`).
async fn first_query_writes_one_pseudonymous_row(
    host: &Host,
    caller: &Presenter,
    key: &SubmitterKey,
    task: &str,
) {
    let spoke = key.pseudonymous_peer_id().as_str().to_owned();
    assert!(
        deferred_rows(&host.journal, STATUS_QUERY_GATE_PREFIX, task)
            .await
            .is_empty(),
        "{task}: no status query yet"
    );
    for _ in 0..2 {
        let answer = caller.get(task).await;
        assert!(answer["error"].is_null(), "{task}: {answer}");
        assert_eq!(
            deferred_rows(&host.journal, STATUS_QUERY_GATE_PREFIX, task).await,
            vec![(spoke.clone(), Some(PrincipalKind::Pseudonymous))],
            "{task}: exactly one pseudonymous status-query row, however often it is polled"
        );
    }
    let raw = raw_payloads(&host.journal, "admission_deferred");
    let line = raw
        .iter()
        .find(|payload| payload["gate"] == format!("{STATUS_QUERY_GATE_PREFIX}{task}"))
        .expect("the raw status-query line");
    assert_eq!(line["principal"], "pseudonymous", "{line}");
    assert_eq!(line["spoke"], spoke.as_str(), "{line}");
}

/// After a restart: the owner's first `tasks/get` of a restored task reads
/// `failed`, and the status-query rows for the task go from `[prior]` to
/// `[prior]` (the observation reseeded: no duplicate) or to
/// `[prior, pseudonymous]` (the prior row is not this principal's observation).
async fn reseeded(
    host: &Host,
    caller: &Presenter,
    key: &SubmitterKey,
    task: &str,
    prior: Option<PrincipalKind>,
    appends: bool,
) {
    let spoke = key.pseudonymous_peer_id().as_str().to_owned();
    let mut expected = vec![(spoke.clone(), prior)];
    assert_eq!(
        deferred_rows(&host.journal, STATUS_QUERY_GATE_PREFIX, task).await,
        expected,
        "{task}: the row that survived the restart"
    );
    if appends {
        expected.push((spoke.clone(), Some(PrincipalKind::Pseudonymous)));
    }
    for _ in 0..2 {
        let answer = caller.get(task).await;
        assert_eq!(wire_state(&answer), "failed", "{task}: {answer}");
        assert_eq!(
            deferred_rows(&host.journal, STATUS_QUERY_GATE_PREFIX, task).await,
            expected,
            "{task}: prior {prior:?} after the restart (a repeat poll never appends)"
        );
    }
    if appends {
        let raw = raw_payloads(&host.journal, "admission_deferred");
        let written = raw
            .iter()
            .rfind(|payload| payload["gate"] == format!("{STATUS_QUERY_GATE_PREFIX}{task}"))
            .expect("the post-restart raw line");
        assert_eq!(written["principal"], "pseudonymous", "{written}");
        assert_eq!(written["spoke"], spoke.as_str(), "{written}");
    }
}

// ── K04 ─────────────────────────────────────────────────────────────────────

const KEYED_ONE_REAL: &str = "d0-keyed-one-real";
const KEYED_TWO_REAL: &str = "d0-keyed-two-real";
const KEYED_TWO_NULL: &str = "d0-keyed-two-null";
const KEYED_TWO_PINNED: &str = "d0-keyed-two-pinned";
const KEYED_TWO_UNKNOWN: &str = "d0-keyed-two-unknown";
const LOCAL_ONCE: &str = "d0-local-once";
const LOCAL_REAL: &str = "d0-local-real";
const LOCAL_NULL: &str = "d0-local-null";
const LOCAL_PINNED: &str = "d0-local-pinned";
const LOCAL_UNKNOWN: &str = "d0-local-unknown";

/// `K04` (`AC6(a)`–`(c)`, `A2`, `A16`) — the A2A ingress carries the PRESENTER's
/// pseudonymous principal end to end, and a cold restart re-seeds the
/// first-status-query observation through both restart blocks.
///
/// Front door: written `a2a.json` → `serve(ServeConfig)` → raw JSON-RPC
/// (`message/send`, `tasks/get`) over the production `AttachServer` composed
/// with a consent projection; the operator answers through the attach socket
/// (`AlwaysAndSave` on the sender-consent card, `Once` on the plain card).
///
/// - TLS host (consent on, two API keys, anchored client): key one and key two
///   each get a card; `[a]` releases the task and writes a typed
///   `pseudonymous` grant; key two's later tasks run without a card (the
///   positive control) while key one's grant never released key two.
/// - Loopback host (`ask`, no consent): the `loopback` handle's principal.
/// - Restart both hosts: the TLS host's tasks come back through the ORPHAN
///   block (a durable node + item; no pending record for the trusted ones),
///   the loopback host's unanswered approvals through the PENDING-FILE block
///   (a pending record, no node).
///
/// `M02` → RED: `policy_principal()` returns `rap(..)`. `M31` → RED: `launch`
/// builds `InboundPeerTask.principal` with `rap(..)`. `M32` → RED: `setup_task`
/// passes `rap(..)` to `request_admission_approval`. `M33` → RED: `launch`
/// passes `rap(..)` to `response_policy`. `M34` → RED: `tasks_get` builds
/// `StatusQueried` with `rap(..)`. `M35` → RED: the orphan block queries
/// `has_recorded_status_query` with `rap(..)`. `M36` → RED: the pending-file
/// block does. `M37` → RED: `record(AwaitingApproval)` writes `principal:
/// None`. `M52` → RED: `start_task` builds a fixed placeholder `Pseudonymous`.
#[tokio::test]
async fn k04_a2a_ingress_carries_the_presenters_pseudonymous_principal_through_both_restarts() {
    setup();
    let one = SubmitterKey::from_api_key(KEY_ONE);
    let two = SubmitterKey::from_api_key(KEY_TWO);
    let local = SubmitterKey::loopback();
    let (p_one, p_two, p_local) = (
        presenter_principal(&one),
        presenter_principal(&two),
        presenter_principal(&local),
    );
    assert!(
        p_one != p_two && p_one != p_local && p_two != p_local,
        "three presenters, three distinct principals"
    );
    let (spoke_one, spoke_two, spoke_local) = (
        one.pseudonymous_peer_id(),
        two.pseudonymous_peer_id(),
        local.pseudonymous_peer_id(),
    );

    // ── Leg 1: TLS, consent on, two distinct presenting keys ────────────────
    let mut keyed = keyed_peer(ResponseMode::NotifyAndWait, true).await;
    let (mut keyed_operator, keyed_shutdown) = attach_operator(&mut keyed.host).await;
    let (c_one, c_two) = (anchored(&keyed, KEY_ONE), anchored(&keyed, KEY_TWO));
    let observed = keyed.host.options.observed.clone();
    assert!(
        InboundPeerRuntime::enforces_sender_consent(keyed.host.server.as_ref()),
        "the consent projection is composed"
    );

    // Key one: a card; its first task parks; `[a]` releases it and writes a
    // typed grant under key one's pseudonym.
    let answer = c_one.send(KEYED_ONE_REAL).await;
    assert_eq!(wire_state(&answer), "auth-required", "{answer}");
    assert_eq!(
        deferred_rows(&keyed.host.journal, APPROVAL_GATE_PREFIX, KEYED_ONE_REAL).await,
        vec![(
            spoke_one.as_str().to_owned(),
            Some(PrincipalKind::Pseudonymous)
        )],
        "the approval-parked row is typed and carries the presenter's pseudonym"
    );
    always_allow(&mut keyed_operator).await;
    launched(&observed, KEYED_ONE_REAL).await;
    assert_eq!(
        grants(&keyed.host.journal).await,
        vec![(Some(spoke_one.clone()), Some(PrincipalKind::Pseudonymous))],
        "the attach `[a]` wrote a typed pseudonymous grant for key one"
    );

    // Key two: key one's grant does not release it — its own card, its own
    // parked row, its own grant.
    let answer = c_two.send(KEYED_TWO_REAL).await;
    assert_eq!(wire_state(&answer), "auth-required", "{answer}");
    assert_eq!(
        deferred_rows(&keyed.host.journal, APPROVAL_GATE_PREFIX, KEYED_TWO_REAL).await,
        vec![(
            spoke_two.as_str().to_owned(),
            Some(PrincipalKind::Pseudonymous)
        )],
        "key one's grant must not release key two"
    );
    always_allow(&mut keyed_operator).await;
    launched(&observed, KEYED_TWO_REAL).await;
    assert_eq!(
        grants(&keyed.host.journal).await,
        vec![
            (Some(spoke_one.clone()), Some(PrincipalKind::Pseudonymous)),
            (Some(spoke_two.clone()), Some(PrincipalKind::Pseudonymous)),
        ]
    );

    // Positive control: key two is trusted now — its next tasks run with no
    // card and no parked row.
    for task in [KEYED_TWO_NULL, KEYED_TWO_PINNED, KEYED_TWO_UNKNOWN] {
        c_two.send(task).await;
        launched(&observed, task).await;
        assert!(
            deferred_rows(&keyed.host.journal, APPROVAL_GATE_PREFIX, task)
                .await
                .is_empty(),
            "{task}: a trusted pseudonymous sender is not asked again"
        );
    }

    // (a) what the handler passed to the runtime, per presenting key.
    assert_eq!(
        snapshot(&observed.admissions),
        vec![
            (KEYED_ONE_REAL.to_owned(), p_one.clone()),
            (KEYED_TWO_REAL.to_owned(), p_two.clone()),
            (KEYED_TWO_NULL.to_owned(), p_two.clone()),
            (KEYED_TWO_PINNED.to_owned(), p_two.clone()),
            (KEYED_TWO_UNKNOWN.to_owned(), p_two.clone()),
        ],
        "request_admission_approval receives each presenter's own pseudonymous principal"
    );
    assert_eq!(
        snapshot(&observed.policies),
        vec![
            p_one.clone(),
            p_two.clone(),
            p_two.clone(),
            p_two.clone(),
            p_two.clone()
        ],
        "response_policy receives the original ingress principal"
    );
    assert_eq!(
        snapshot(&observed.starts),
        vec![
            expected_start(&one, KEYED_ONE_REAL),
            expected_start(&two, KEYED_TWO_REAL),
            expected_start(&two, KEYED_TWO_NULL),
            expected_start(&two, KEYED_TWO_PINNED),
            expected_start(&two, KEYED_TWO_UNKNOWN),
        ],
        "InboundPeerTask.principal is the presenter's pseudonymous principal"
    );

    // (b) the raw journal lines an older reader would see.
    let raw_grants = raw_payloads(&keyed.host.journal, "consent_granted");
    assert_eq!(raw_grants.len(), 2, "{raw_grants:?}");
    for (payload, spoke) in raw_grants.iter().zip([&spoke_one, &spoke_two]) {
        assert_eq!(payload["principal"], "pseudonymous", "{payload}");
        assert_eq!(payload["sender"], spoke.as_str(), "{payload}");
    }
    for (task, spoke) in [(KEYED_ONE_REAL, &spoke_one), (KEYED_TWO_REAL, &spoke_two)] {
        let parked: Vec<_> = raw_payloads(&keyed.host.journal, "admission_deferred")
            .into_iter()
            .filter(|payload| payload["gate"] == format!("{APPROVAL_GATE_PREFIX}{task}"))
            .collect();
        assert_eq!(parked.len(), 1, "{task}: {parked:?}");
        assert_eq!(parked[0]["principal"], "pseudonymous", "{}", parked[0]);
        assert_eq!(parked[0]["spoke"], spoke.as_str(), "{}", parked[0]);
    }

    // Each key's FIRST poll writes one `pseudonymous` row (the other tasks are
    // deliberately not polled before the restart).
    first_query_writes_one_pseudonymous_row(&keyed.host, &c_one, &one, KEYED_ONE_REAL).await;
    first_query_writes_one_pseudonymous_row(&keyed.host, &c_two, &two, KEYED_TWO_REAL).await;
    plant_status_row(&keyed.host.journal, &spoke_two, KEYED_TWO_NULL, None).await;
    plant_status_row(
        &keyed.host.journal,
        &spoke_two,
        KEYED_TWO_PINNED,
        Some(PrincipalKind::Pinned),
    )
    .await;
    plant_status_row(
        &keyed.host.journal,
        &spoke_two,
        KEYED_TWO_UNKNOWN,
        Some(PrincipalKind::Unknown),
    )
    .await;
    for (key, task) in [
        (&one, KEYED_ONE_REAL),
        (&two, KEYED_TWO_REAL),
        (&two, KEYED_TWO_NULL),
        (&two, KEYED_TWO_PINNED),
        (&two, KEYED_TWO_UNKNOWN),
    ] {
        await_waiting(&keyed.host, key, task).await;
    }
    // The two parked-then-released tasks still have their pending records;
    // the three trusted ones never had one — only the orphan block can
    // restore those.
    let pending = pending_records(&keyed.host);
    assert_eq!(
        pending,
        vec![
            (KEYED_ONE_REAL.to_owned(), one.as_str().to_owned()),
            (KEYED_TWO_REAL.to_owned(), two.as_str().to_owned()),
        ],
        "pending records of the consent-parked tasks"
    );

    // ── Leg 2: loopback, plain card, unanswered approvals ───────────────────
    let mut plain = loopback_peer(ResponseMode::NotifyAndWait, false).await;
    let (mut plain_operator, plain_shutdown) = attach_operator(&mut plain.host).await;
    let c_local = loopback(&plain);
    let plain_observed = plain.host.options.observed.clone();
    assert!(
        !InboundPeerRuntime::enforces_sender_consent(plain.host.server.as_ref()),
        "no consent projection on the loopback host"
    );

    let answer = c_local.send(LOCAL_ONCE).await;
    assert_eq!(wire_state(&answer), "auth-required", "{answer}");
    answer_card(
        &mut plain_operator,
        "a2a/message.send",
        ApprovalOutcome::Once,
    )
    .await;
    launched(&plain_observed, LOCAL_ONCE).await;
    for task in [LOCAL_REAL, LOCAL_NULL, LOCAL_PINNED, LOCAL_UNKNOWN] {
        let answer = c_local.send(task).await;
        assert_eq!(wire_state(&answer), "auth-required", "{task}: {answer}");
    }
    for task in [
        LOCAL_ONCE,
        LOCAL_REAL,
        LOCAL_NULL,
        LOCAL_PINNED,
        LOCAL_UNKNOWN,
    ] {
        assert_eq!(
            deferred_rows(&plain.host.journal, APPROVAL_GATE_PREFIX, task).await,
            vec![(
                spoke_local.as_str().to_owned(),
                Some(PrincipalKind::Pseudonymous)
            )],
            "{task}: the approval-parked row is typed and carries the loopback pseudonym"
        );
    }
    assert_eq!(
        snapshot(&plain_observed.admissions),
        [
            LOCAL_ONCE,
            LOCAL_REAL,
            LOCAL_NULL,
            LOCAL_PINNED,
            LOCAL_UNKNOWN
        ]
        .map(|task| (task.to_owned(), p_local.clone()))
        .to_vec(),
        "request_admission_approval receives the loopback principal"
    );
    assert_eq!(snapshot(&plain_observed.policies), vec![p_local.clone()]);
    assert_eq!(
        snapshot(&plain_observed.starts),
        vec![expected_start(&local, LOCAL_ONCE)],
        "only the answered task started"
    );
    assert!(
        grants(&plain.host.journal).await.is_empty(),
        "a one-time `Once` writes no grant"
    );
    first_query_writes_one_pseudonymous_row(&plain.host, &c_local, &local, LOCAL_REAL).await;
    plant_status_row(&plain.host.journal, &spoke_local, LOCAL_NULL, None).await;
    plant_status_row(
        &plain.host.journal,
        &spoke_local,
        LOCAL_PINNED,
        Some(PrincipalKind::Pinned),
    )
    .await;
    plant_status_row(
        &plain.host.journal,
        &spoke_local,
        LOCAL_UNKNOWN,
        Some(PrincipalKind::Unknown),
    )
    .await;
    await_waiting(&plain.host, &local, LOCAL_ONCE).await;
    let pending = pending_records(&plain.host);
    for task in [
        LOCAL_ONCE,
        LOCAL_REAL,
        LOCAL_NULL,
        LOCAL_PINNED,
        LOCAL_UNKNOWN,
    ] {
        assert!(
            pending.contains(&(task.to_owned(), local.as_str().to_owned())),
            "{task}: a pending record exists before the restart: {pending:?}"
        );
    }
    for task in [LOCAL_REAL, LOCAL_NULL, LOCAL_PINNED, LOCAL_UNKNOWN] {
        assert_eq!(
            node_status(&plain.host, &local, task).await,
            None,
            "{task}: an unanswered approval has no node (only the pending file can restore it)"
        );
    }

    // Approved TLS tasks have durable waiting nodes and item identity. Remove
    // their pending-file fallback so only orphan reconciliation can restore them.
    tokio::fs::write(
        keyed.host.journal.path.join(".rustain/a2a-pending.json"),
        b"[]",
    )
    .await
    .expect("isolate the orphan-only restart fixture");

    // ── Restart both hosts cold (the 19.18c `K10` shape) ────────────────────
    keyed_shutdown.cancel();
    plain_shutdown.cancel();
    drop(keyed_operator);
    drop(plain_operator);
    let keyed = keyed.restart().await;
    let plain = plain.restart().await;
    let (c_one, c_two) = (anchored(&keyed, KEY_ONE), anchored(&keyed, KEY_TWO));
    let c_local = loopback(&plain);
    // A response proves each new listener completed reconciliation. Both rows
    // already belong to this pseudonym, so these reads must remain deduplicated.
    assert_eq!(wire_state(&c_one.get(KEYED_ONE_REAL).await), "failed");
    assert_eq!(wire_state(&c_local.get(LOCAL_REAL).await), "failed");

    // Durable nodes, with no pending-file fallback, prove the orphan path.
    for (key, task) in [
        (&one, KEYED_ONE_REAL),
        (&two, KEYED_TWO_REAL),
        (&two, KEYED_TWO_NULL),
        (&two, KEYED_TWO_PINNED),
        (&two, KEYED_TWO_UNKNOWN),
    ] {
        assert!(
            matches!(
                node_status(&keyed.host, key, task).await,
                None | Some(NodeState::Failed)
            ),
            "{task}: node recovery handed the orphan to the reconcile pass, which failed it"
        );
    }

    // These unanswered loopback approvals have no node; only the pending file
    // can restore them. The answered task remains an ordinary orphan.
    for task in [LOCAL_REAL, LOCAL_NULL, LOCAL_PINNED, LOCAL_UNKNOWN] {
        assert_eq!(
            node_status(&plain.host, &local, task).await,
            None,
            "{task}: no node — the task came from the pending file"
        );
    }
    assert!(
        matches!(
            node_status(&plain.host, &local, LOCAL_ONCE).await,
            None | Some(NodeState::Failed)
        ),
        "the answered task is an ordinary orphan"
    );
    assert!(
        pending_records_cleared(&plain.host),
        "the pending file was truncated after the reconcile"
    );

    // (c) the status-query observation is reseeded by both blocks: the
    // `pseudonymous` row and the legacy (absent) row deduplicate; a planted
    // `pinned` or `unknown` row with the same spoke and gate does not.
    reseeded(
        &keyed.host,
        &c_one,
        &one,
        KEYED_ONE_REAL,
        Some(PrincipalKind::Pseudonymous),
        false,
    )
    .await;
    reseeded(
        &keyed.host,
        &c_two,
        &two,
        KEYED_TWO_REAL,
        Some(PrincipalKind::Pseudonymous),
        false,
    )
    .await;
    reseeded(&keyed.host, &c_two, &two, KEYED_TWO_NULL, None, false).await;
    reseeded(
        &keyed.host,
        &c_two,
        &two,
        KEYED_TWO_PINNED,
        Some(PrincipalKind::Pinned),
        true,
    )
    .await;
    reseeded(
        &keyed.host,
        &c_two,
        &two,
        KEYED_TWO_UNKNOWN,
        Some(PrincipalKind::Unknown),
        true,
    )
    .await;

    reseeded(
        &plain.host,
        &c_local,
        &local,
        LOCAL_REAL,
        Some(PrincipalKind::Pseudonymous),
        false,
    )
    .await;
    reseeded(&plain.host, &c_local, &local, LOCAL_NULL, None, false).await;
    reseeded(
        &plain.host,
        &c_local,
        &local,
        LOCAL_PINNED,
        Some(PrincipalKind::Pinned),
        true,
    )
    .await;
    reseeded(
        &plain.host,
        &c_local,
        &local,
        LOCAL_UNKNOWN,
        Some(PrincipalKind::Unknown),
        true,
    )
    .await;
}

/// The pending file reads as an empty JSON array.
fn pending_records_cleared(host: &Host) -> bool {
    let path = host.journal.path.join(".rustain/a2a-pending.json");
    serde_json::from_slice::<Vec<serde_json::Value>>(&std::fs::read(&path).expect("pending file"))
        .expect("the pending file is a JSON array")
        .is_empty()
}

// ===========================================================================
// K05 (Story 19.18d0, AC2) — durable encoding is additive and downgrade-safe.
//
// Golden provenance (Task 0, rustain HEAD e70db869f1abd323dc1c10f6f3e263aaf6ec7b7f;
// recorded in Story 19.18d0's Dev Agent Record, Debug Log References):
//   G1, G3, G4          <- throwaway repro run 1 (attach `[a]` grant + revoke)
//   G1serve, G2 (x2)    <- throwaway repro run 2 (real `serve()` rows)
//   The byte strings below are those runs' captured lines, verbatim;
//   `recorded_at_ms`/`seq` are the captured values and are re-stamped here
//   through the journal's injected clock (`NodeJournal::with_clock`) or by
//   explicit `JournalEntry::new`.
//
// CLAIM CEILING (A14) — do not blur:
//   * G4 is a TYPE EXERCISE of `RoomEvent::ApprovalResolved` +
//     `ApprovalSource::RemotePeer` through `NodeJournal::append_room`. NO
//     production code writes `ApprovalResolved`; G4 is a serialization-
//     compatibility golden, not evidence of a production RemotePeer approval
//     writer.
//   * G1 (`PendingConsentManager::persist_grant`) and G3
//     (`persist_sender_consent` revoke) are production writers; G2 rows are
//     production writers observed in a real `serve()` run. This test does NOT
//     re-drive those writers (K02/K03/K04/K07 own that); it pins the durable
//     wire contract their bytes must keep. The `/team trust` grant branch has no
//     production caller and is not exercised.
//   * The "old reader" is a verbatim HEAD copy of the pre-change types and of
//     `parse_entries` (R2), not a build of the previous release.
//
// Mutant oracles (each must turn this test RED): M06 / M07 (drop
// `skip_serializing_if` on `ConsentGranted` / `AdmissionDeferred`), M08
// (`#[serde(rename = "peer")]` on `RemotePeer.peer_id`), M09 (drop
// `#[serde(other)]`), M10 (drop `rename_all = "snake_case"` on
// `PrincipalKind`), M11 (`policy_principal()` fabricates a `Pinned` principal
// from the serialized `peer_id`).
// ===========================================================================

mod k05_durable {
    #![allow(dead_code, unused_imports)]

    pub use std::path::Path;
    pub use std::sync::Arc;
    pub use std::sync::atomic::{AtomicUsize, Ordering};

    pub use rustain::domain::Clock;
    pub use rustain::domain::models::{
        AgentId, ApprovalOutcome, ApprovalSource, InvocationFingerprint, JournalEntry,
        JournalRecord, OrchestrationRoomId, PeerId, PolicyPrincipal, PrincipalKind, RoomEvent,
    };
    pub use rustain::domain::ports::RoomJournalReader;
    pub use rustain::infrastructure::subagent::{NodeJournal, WorkspaceJournalReader};

    // ── captured golden lines (verbatim, without the trailing \n) ──────────
    /// production writer: PendingConsentManager::persist_grant via attach [a] AlwaysAndSave, in-process AttachServer (run 1, seq 1)
    pub const K05_G1_CONSENT_GRANTED: &str = r##"{"schema_version":1,"seq":1,"recorded_at_ms":1790974662796,"record":{"kind":"room","payload":{"event":"consent_granted","sender":"1220fd4baad8bbc21b3d13fcc22ab21561129ab39d9c335fd4f7509cf26ba15f7569","granted_at":1790974662796}}}"##;

    /// same writer inside a real serve() run (run 2, seq 8)
    pub const K05_G1_SERVE_CONSENT_GRANTED: &str = r##"{"schema_version":1,"seq":8,"recorded_at_ms":1790974823216,"record":{"kind":"room","payload":{"event":"consent_granted","sender":"1220b6c75ae3aa2e16def7c816a0e90527fe8d1e30033ea46aba1c8170d39da2d637","granted_at":1790974823216}}}"##;

    /// production writer, real serve() run, InboundOutcome::AwaitingApproval (run 2, seq 6)
    pub const K05_G2_ADMISSION_DEFERRED_APPROVAL: &str = r##"{"schema_version":1,"seq":6,"recorded_at_ms":1790974823204,"record":{"kind":"room","payload":{"event":"admission_deferred","coordinator":"root","spoke":"1220b6c75ae3aa2e16def7c816a0e90527fe8d1e30033ea46aba1c8170d39da2d637","gate":"a2a-inbound-approval:g2-m1"}}}"##;

    /// production writer, real serve() run, first tasks/get = InboundOutcome::StatusQueried (run 2, seq 7)
    pub const K05_G2_ADMISSION_DEFERRED_STATUS_QUERY: &str = r##"{"schema_version":1,"seq":7,"recorded_at_ms":1790974823210,"record":{"kind":"room","payload":{"event":"admission_deferred","coordinator":"root","spoke":"1220b6c75ae3aa2e16def7c816a0e90527fe8d1e30033ea46aba1c8170d39da2d637","gate":"a2a-status-query:g2-m1"}}}"##;

    /// production writer: persist_sender_consent(trust=false, now=1_700_000_000_000) (run 1, seq 2)
    pub const K05_G3_CONSENT_REVOKED: &str = r##"{"schema_version":1,"seq":2,"recorded_at_ms":1790974663108,"record":{"kind":"room","payload":{"event":"consent_revoked","sender":"1220fd4baad8bbc21b3d13fcc22ab21561129ab39d9c335fd4f7509cf26ba15f7569","revoked_at":1700000000000}}}"##;

    /// TYPE EXERCISE ONLY (no production writer): RoomEvent::ApprovalResolved{node root, fingerprint [0xAB;32], source RemotePeer{conversation_id consent-gate-r1, peer_id X}, outcome Once} via NodeJournal::append_room (run 1, seq 3)
    pub const K05_G4_APPROVAL_RESOLVED_REMOTE_PEER: &str = r##"{"schema_version":1,"seq":3,"recorded_at_ms":1790974663113,"record":{"kind":"room","payload":{"event":"approval_resolved","node":"root","fingerprint":[171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171,171],"source":{"RemotePeer":{"conversation_id":"consent-gate-r1","peer_id":"1220fd4baad8bbc21b3d13fcc22ab21561129ab39d9c335fd4f7509cf26ba15f7569"}},"outcome":"Once"}}}"##;

    /// Run-1 sender / approval peer `X` (the pseudonym embedded in G1/G3/G4).
    pub const K05_X: &str = "1220fd4baad8bbc21b3d13fcc22ab21561129ab39d9c335fd4f7509cf26ba15f7569";
    /// Run-2 sender / spoke `Y` (the pseudonym embedded in G1serve/G2).
    pub const K05_Y: &str = "1220b6c75ae3aa2e16def7c816a0e90527fe8d1e30033ea46aba1c8170d39da2d637";

    // ── old-reader copy (R2) ───────────────────────────────────────────────
    pub mod old_reader {
        use rustain::domain::models::{AgentId, ApprovalOutcome, InvocationFingerprint, PeerId};
        use serde::{Deserialize, Serialize};

        // ── tool_call.rs:40-68 ─────────────────────────────────────────────────
        /// Source of an approval request.
        ///
        /// `#[non_exhaustive]` so 14-1 (A2A peer agents) can add a `RemotePeer`
        /// variant without breaking match sites.
        #[non_exhaustive]
        #[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
        pub enum ApprovalSource {
            ForegroundTurn {
                conversation_id: String,
            },
            ForegroundSubagent {
                conversation_id: String,
                parent_tool_call_id: String,
                subagent_type: String,
            },
            AcpSession {
                session_id: String,
                conversation_id: String,
            },
            RemotePeer {
                conversation_id: String,
                peer_id: PeerId,
            },
            BackgroundAgent {
                conversation_id: String,
                task_id: String,
                subagent_type: String,
            },
        }

        // ── orchestration_room.rs:424-427 (header), 449-453, 461-466, 662-680 ──
        #[non_exhaustive]
        #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case", tag = "event")]
        pub enum RoomEvent {
            AdmissionDeferred {
                coordinator: AgentId,
                spoke: String,
                gate: String,
            },
            ApprovalResolved {
                node: AgentId,
                fingerprint: InvocationFingerprint,
                source: ApprovalSource,
                outcome: ApprovalOutcome,
            },
            /// Durable operator grant allowing one authenticated sender to bypass the
            /// pending-consent gate. Duplicate grants are replay-idempotent.
            ConsentGranted {
                /// Stable authenticated sender identity. A missing value from a
                /// forward-written or malformed record fails closed during projection.
                #[serde(default)]
                sender: Option<PeerId>,
                #[serde(default)]
                granted_at: i64,
            },
            /// Durable operator revocation of a previously granted sender.
            ConsentRevoked {
                /// Stable authenticated sender identity. Missing and never-granted
                /// senders are projection no-ops; revocation never manufactures trust.
                #[serde(default)]
                sender: Option<PeerId>,
                #[serde(default)]
                revoked_at: i64,
            },
        }

        // ── node_journal.rs (model):8, 14-35, 107-112 ──────────────────────────
        pub const NODE_JOURNAL_SCHEMA_VERSION: u32 = 1;

        /// One durable line in a room journal. Sequence numbers establish the total
        /// order consumed by both recovery and room-projection folds.
        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
        pub struct JournalEntry {
            pub schema_version: u32,
            pub seq: u64,
            #[serde(default)]
            pub recorded_at_ms: i64,
            pub record: JournalRecord,
        }

        #[non_exhaustive]
        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case", tag = "kind", content = "payload")]
        pub enum JournalRecord {
            Room(RoomEvent),
        }

        // ── node_journal.rs (infra):1023-1068, 1070-1091 (JournalError reduced) ─
        // Deviation from HEAD copy: fixture enum is non_exhaustive (it is a new test-side type; HEAD's
        // JournalError is the production one).
        #[non_exhaustive]
        #[derive(Debug)]
        pub enum OldJournalError {
            CorruptRecord {
                line: usize,
                source: serde_json::Error,
            },
            UnsupportedSchema {
                found: u32,
                supported: u32,
            },
            InvalidSequence {
                expected: u64,
                found: u64,
            },
            SequenceExhausted,
        }

        pub fn parse_entries(
            bytes: &[u8],
        ) -> Result<(Vec<JournalEntry>, usize, usize), OldJournalError> {
            let file_len = bytes.len();
            let mut entries = Vec::new();
            let mut valid_len = 0usize;
            let mut expected_seq = 1u64;
            let mut lines = bytes.split_inclusive(|byte| *byte == b'\n').peekable();

            while let Some(raw_line) = lines.next() {
                let is_last = lines.peek().is_none();
                let newline_terminated = raw_line.last() == Some(&b'\n');
                if is_last && !newline_terminated {
                    break;
                }
                let line = raw_line.strip_suffix(b"\n").unwrap_or(raw_line);
                let parsed = serde_json::from_slice::<JournalEntry>(line);
                let entry = match parsed {
                    Ok(entry) => entry,
                    Err(_) if is_last => break,
                    Err(source) => {
                        return Err(OldJournalError::CorruptRecord {
                            line: entries.len() + 1,
                            source,
                        });
                    }
                };
                if entry.schema_version != NODE_JOURNAL_SCHEMA_VERSION {
                    return Err(OldJournalError::UnsupportedSchema {
                        found: entry.schema_version,
                        supported: NODE_JOURNAL_SCHEMA_VERSION,
                    });
                }
                if entry.seq != expected_seq {
                    return Err(OldJournalError::InvalidSequence {
                        expected: expected_seq,
                        found: entry.seq,
                    });
                }
                expected_seq = expected_seq
                    .checked_add(1)
                    .ok_or(OldJournalError::SequenceExhausted)?;
                entries.push(entry);
                valid_len += raw_line.len();
            }

            Ok((entries, valid_len, file_len))
        }
    }

    // ── deterministic clock for the real on-disk leg ───────────────────────
    /// Replays a fixed list of wall-clock stamps, one per `NodeJournal` append
    /// batch. Exhaustion panics inside the append task, so a journal that reads
    /// its clock more than once per append fails loudly instead of drifting.
    pub struct K05ScriptedClock {
        stamps: Vec<i64>,
        next: AtomicUsize,
    }

    impl K05ScriptedClock {
        pub fn new(stamps: Vec<i64>) -> Self {
            Self {
                stamps,
                next: AtomicUsize::new(0),
            }
        }

        pub fn reads(&self) -> usize {
            self.next.load(Ordering::SeqCst)
        }
    }

    impl Clock for K05ScriptedClock {
        fn now(&self) -> std::time::Instant {
            std::time::Instant::now()
        }

        fn frame(&self) -> u64 {
            0
        }

        fn wall_now_ms(&self) -> i64 {
            let index = self.next.fetch_add(1, Ordering::SeqCst);
            *self
                .stamps
                .get(index)
                .expect("K05 clock script exhausted: the journal stamped more than once per append")
        }
    }

    // ── builders ───────────────────────────────────────────────────────────
    pub fn k05_peer(hex: &str) -> PeerId {
        PeerId::parse(hex).expect("K05 peer id is canonical sha2-256 multihash hex")
    }

    pub fn k05_entry(seq: u64, recorded_at_ms: i64, event: RoomEvent) -> JournalEntry {
        JournalEntry::new(seq, JournalRecord::Room(event), recorded_at_ms)
    }

    pub fn k05_line(entry: &JournalEntry) -> String {
        serde_json::to_string(entry).expect("JournalEntry serializes")
    }

    /// Newline-terminated JSONL bytes, exactly the shape `NodeJournal` appends.
    pub fn k05_bytes(entries: &[JournalEntry]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for entry in entries {
            bytes.extend_from_slice(k05_line(entry).as_bytes());
            bytes.push(b'\n');
        }
        bytes
    }

    pub fn k05_granted_event(
        sender: &PeerId,
        granted_at: i64,
        principal: Option<PrincipalKind>,
    ) -> RoomEvent {
        RoomEvent::ConsentGranted {
            sender: Some(sender.clone()),
            granted_at,
            principal,
        }
    }

    pub fn k05_revoked_event(sender: &PeerId, revoked_at: i64) -> RoomEvent {
        RoomEvent::ConsentRevoked {
            sender: Some(sender.clone()),
            revoked_at,
        }
    }

    pub fn k05_deferred_event(
        spoke: &PeerId,
        gate: &str,
        principal: Option<PrincipalKind>,
    ) -> RoomEvent {
        RoomEvent::AdmissionDeferred {
            coordinator: AgentId::root(),
            spoke: spoke.as_str().to_string(),
            gate: gate.to_string(),
            principal,
        }
    }

    pub fn k05_approval_event(source: ApprovalSource) -> RoomEvent {
        RoomEvent::ApprovalResolved {
            node: AgentId::root(),
            fingerprint: InvocationFingerprint([0xAB; 32]),
            source,
            outcome: ApprovalOutcome::Once,
        }
    }

    /// A journal row that carries no policy meaning; used to reach a captured
    /// `seq` and as the known-good line that follows a line under test.
    pub fn k05_filler_event() -> RoomEvent {
        RoomEvent::ConsentRevoked {
            sender: None,
            revoked_at: 0,
        }
    }

    pub const K05_CONVERSATION: &str = "consent-gate-r1";

    pub fn k05_remote_source(principal: &PolicyPrincipal) -> ApprovalSource {
        ApprovalSource::remote_peer(K05_CONVERSATION.to_string(), principal)
    }

    /// The serialized `ApprovalSource` G4 embeds, byte for byte.
    pub fn k05_g4_source_json() -> String {
        format!(
            r#"{{"RemotePeer":{{"conversation_id":"{}","peer_id":"{}"}}}}"#,
            K05_CONVERSATION, K05_X
        )
    }

    /// One captured golden: the exact bytes plus the known entry that must
    /// serialize to them.
    pub struct K05Golden {
        pub name: &'static str,
        pub bytes: &'static str,
        pub entry: JournalEntry,
    }

    pub const K05_G2_APPROVAL_GATE: &str = "a2a-inbound-approval:g2-m1";
    pub const K05_G2_STATUS_GATE: &str = "a2a-status-query:g2-m1";

    /// Every captured golden with the `principal: None` / in-memory-`Some`
    /// value its writer produces, stamped with the captured `seq`/wall time.
    pub fn k05_goldens() -> Vec<K05Golden> {
        let x = k05_peer(K05_X);
        let y = k05_peer(K05_Y);
        vec![
            K05Golden {
                name: "G1",
                bytes: K05_G1_CONSENT_GRANTED,
                entry: k05_entry(1, 1790974662796, k05_granted_event(&x, 1790974662796, None)),
            },
            K05Golden {
                name: "G1serve",
                bytes: K05_G1_SERVE_CONSENT_GRANTED,
                entry: k05_entry(8, 1790974823216, k05_granted_event(&y, 1790974823216, None)),
            },
            K05Golden {
                name: "G2approval",
                bytes: K05_G2_ADMISSION_DEFERRED_APPROVAL,
                entry: k05_entry(
                    6,
                    1790974823204,
                    k05_deferred_event(&y, K05_G2_APPROVAL_GATE, None),
                ),
            },
            K05Golden {
                name: "G2status",
                bytes: K05_G2_ADMISSION_DEFERRED_STATUS_QUERY,
                entry: k05_entry(
                    7,
                    1790974823210,
                    k05_deferred_event(&y, K05_G2_STATUS_GATE, None),
                ),
            },
            K05Golden {
                name: "G3",
                bytes: K05_G3_CONSENT_REVOKED,
                entry: k05_entry(2, 1790974663108, k05_revoked_event(&x, 1700000000000)),
            },
            K05Golden {
                name: "G4",
                bytes: K05_G4_APPROVAL_RESOLVED_REMOTE_PEER,
                entry: k05_entry(
                    3,
                    1790974663113,
                    k05_approval_event(k05_remote_source(&PolicyPrincipal::rap(x.clone()))),
                ),
            },
        ]
    }

    pub fn k05_golden(name: &str) -> K05Golden {
        k05_goldens()
            .into_iter()
            .find(|golden| golden.name == name)
            .expect("K05 golden name exists")
    }

    /// The only principal-bearing goldens: both consent grants and both
    /// `admission_deferred` rows.
    pub const K05_PRINCIPAL_BEARING: [&str; 4] = ["G1", "G1serve", "G2approval", "G2status"];

    /// `(kind, wire string)` for the three values the new writer can emit.
    pub fn k05_kinds() -> [(PrincipalKind, &'static str); 3] {
        [
            (PrincipalKind::Pinned, "pinned"),
            (PrincipalKind::Pseudonymous, "pseudonymous"),
            (PrincipalKind::Unknown, "unknown"),
        ]
    }

    /// Append `,"principal":"<wire>"` as the last payload member of a golden
    /// (every principal-bearing golden ends `...}}}`: payload, record, entry).
    pub fn k05_with_principal(golden: &str, wire: &str) -> String {
        let body = golden
            .strip_suffix("}}}")
            .expect("K05 golden ends payload/record/entry braces");
        format!("{body},\"principal\":\"{wire}\"}}}}}}")
    }

    pub fn k05_set_principal(
        entry: &JournalEntry,
        principal: Option<PrincipalKind>,
    ) -> JournalEntry {
        let mut entry = entry.clone();
        match &mut entry.record {
            JournalRecord::Room(RoomEvent::ConsentGranted {
                principal: slot, ..
            })
            | JournalRecord::Room(RoomEvent::AdmissionDeferred {
                principal: slot, ..
            }) => {
                *slot = principal;
            }
            other => panic!("K05 row carries no principal: {other:?}"),
        }
        entry
    }

    pub fn k05_decode(line: &str) -> JournalEntry {
        serde_json::from_str::<JournalEntry>(line)
            .unwrap_or_else(|error| panic!("new reader must decode {line}: {error}"))
    }

    pub fn k05_room(entry: &JournalEntry) -> &RoomEvent {
        match &entry.record {
            JournalRecord::Room(event) => event,
            other => panic!("K05 row is not a room event: {other:?}"),
        }
    }

    pub fn k05_granted_fields(
        entry: &JournalEntry,
    ) -> (Option<PeerId>, i64, Option<PrincipalKind>) {
        match k05_room(entry) {
            RoomEvent::ConsentGranted {
                sender,
                granted_at,
                principal,
            } => (sender.clone(), *granted_at, *principal),
            other => panic!("expected ConsentGranted, got {other:?}"),
        }
    }

    pub fn k05_deferred_fields(
        entry: &JournalEntry,
    ) -> (AgentId, String, String, Option<PrincipalKind>) {
        match k05_room(entry) {
            RoomEvent::AdmissionDeferred {
                coordinator,
                spoke,
                gate,
                principal,
            } => (coordinator.clone(), spoke.clone(), gate.clone(), *principal),
            other => panic!("expected AdmissionDeferred, got {other:?}"),
        }
    }

    pub fn k05_revoked_fields(entry: &JournalEntry) -> (Option<PeerId>, i64) {
        match k05_room(entry) {
            RoomEvent::ConsentRevoked { sender, revoked_at } => (sender.clone(), *revoked_at),
            other => panic!("expected ConsentRevoked, got {other:?}"),
        }
    }

    pub fn k05_approval_source(entry: &JournalEntry) -> ApprovalSource {
        match k05_room(entry) {
            RoomEvent::ApprovalResolved { source, .. } => source.clone(),
            other => panic!("expected ApprovalResolved, got {other:?}"),
        }
    }

    // ── raw (hand-written) line builders for the negative controls ─────────
    pub fn k05_json_str(value: &str) -> String {
        format!("\"{value}\"")
    }

    pub fn k05_raw(schema_version: u32, seq: u64, payload: &str) -> String {
        format!(
            r#"{{"schema_version":{schema_version},"seq":{seq},"recorded_at_ms":{seq},"record":{{"kind":"room","payload":{payload}}}}}"#
        )
    }

    pub fn k05_payload_granted(sender_json: &str, granted_at: i64, extra: &str) -> String {
        format!(
            r#"{{"event":"consent_granted","sender":{sender_json},"granted_at":{granted_at}{extra}}}"#
        )
    }

    pub fn k05_payload_revoked(sender_json: &str, revoked_at: i64) -> String {
        format!(r#"{{"event":"consent_revoked","sender":{sender_json},"revoked_at":{revoked_at}}}"#)
    }

    pub fn k05_join(lines: &[String]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for line in lines {
            bytes.extend_from_slice(line.as_bytes());
            bytes.push(b'\n');
        }
        bytes
    }

    // ── old-reader expectations ────────────────────────────────────────────
    pub fn k05_old_granted(sender: &PeerId, granted_at: i64) -> old_reader::RoomEvent {
        old_reader::RoomEvent::ConsentGranted {
            sender: Some(sender.clone()),
            granted_at,
        }
    }

    pub fn k05_old_revoked(sender: Option<&PeerId>, revoked_at: i64) -> old_reader::RoomEvent {
        old_reader::RoomEvent::ConsentRevoked {
            sender: sender.cloned(),
            revoked_at,
        }
    }

    pub fn k05_old_deferred(spoke: &PeerId, gate: &str) -> old_reader::RoomEvent {
        old_reader::RoomEvent::AdmissionDeferred {
            coordinator: AgentId::root(),
            spoke: spoke.as_str().to_string(),
            gate: gate.to_string(),
        }
    }

    pub fn k05_old_approval(peer: &PeerId) -> old_reader::RoomEvent {
        old_reader::RoomEvent::ApprovalResolved {
            node: AgentId::root(),
            fingerprint: InvocationFingerprint([0xAB; 32]),
            source: old_reader::ApprovalSource::RemotePeer {
                conversation_id: K05_CONVERSATION.to_string(),
                peer_id: peer.clone(),
            },
            outcome: ApprovalOutcome::Once,
        }
    }

    pub fn k05_old_room(event: old_reader::RoomEvent) -> old_reader::JournalRecord {
        old_reader::JournalRecord::Room(event)
    }

    /// Feed `preceding` + `target` + one known-good revoke to the old reader.
    /// The target is therefore NON-tail; a defect in it would be a
    /// `CorruptRecord` for the whole load. Asserts the old reader keeps every
    /// row (no dropped line), reads exactly `expected_target`, and returns the
    /// bytes so callers can pin them.
    pub fn k05_old_reads_nontail(
        label: &str,
        preceding: &[JournalEntry],
        target: JournalEntry,
        expected_target: old_reader::RoomEvent,
        known_good_sender: &PeerId,
    ) -> Vec<u8> {
        let target_index = preceding.len();
        assert_eq!(
            target.seq,
            target_index as u64 + 1,
            "{label}: fixture seq must be contiguous"
        );
        let target_ms = target.recorded_at_ms;
        let mut rows = preceding.to_vec();
        rows.push(target);
        rows.push(k05_entry(
            target_index as u64 + 2,
            target_ms + 1,
            k05_revoked_event(known_good_sender, 7),
        ));
        let bytes = k05_bytes(&rows);
        let (entries, valid_len, file_len) = old_reader::parse_entries(&bytes)
            .unwrap_or_else(|error| panic!("{label}: old reader rejected the journal: {error:?}"));
        assert_eq!(
            entries.len(),
            rows.len(),
            "{label}: old reader dropped a row"
        );
        assert_eq!(
            (valid_len, file_len),
            (bytes.len(), bytes.len()),
            "{label}: old reader left a byte range unread"
        );
        for (index, entry) in entries.iter().enumerate() {
            assert_eq!(entry.seq, index as u64 + 1, "{label}: seq order");
            assert_eq!(entry.schema_version, 1, "{label}: schema version stays 1");
        }
        assert_eq!(
            entries[target_index].record,
            k05_old_room(expected_target),
            "{label}: old reader sees different sender/spoke/peer than before"
        );
        assert_eq!(
            entries[target_index + 1].record,
            k05_old_room(k05_old_revoked(Some(known_good_sender), 7)),
            "{label}: known-good follower"
        );
        bytes
    }

    // ── real on-disk helpers ───────────────────────────────────────────────
    pub async fn k05_open(
        workspace: &Path,
        room: &str,
        stamps: Vec<i64>,
    ) -> (NodeJournal, Arc<K05ScriptedClock>) {
        let clock = Arc::new(K05ScriptedClock::new(stamps));
        let journal = NodeJournal::open(
            workspace,
            OrchestrationRoomId::parse(room).expect("K05 room id"),
        )
        .await
        .expect("open a real NodeJournal file")
        .with_clock(clock.clone());
        (journal, clock)
    }

    pub async fn k05_load(workspace: &Path, room: &str) -> Vec<JournalEntry> {
        WorkspaceJournalReader::open(
            workspace,
            OrchestrationRoomId::parse(room).expect("K05 room id"),
        )
        .load_entries()
        .await
        .expect("WorkspaceJournalReader loads the real journal file")
    }

    pub fn k05_file_lines(path: &Path) -> (Vec<u8>, Vec<String>) {
        let bytes = std::fs::read(path).expect("read the journal file");
        let text = String::from_utf8(bytes.clone()).expect("journal is UTF-8");
        assert!(
            text.ends_with('\n'),
            "every durable line is newline-terminated"
        );
        let lines = text
            .strip_suffix('\n')
            .expect("checked above")
            .split('\n')
            .map(str::to_string)
            .collect();
        (bytes, lines)
    }
}

#[tokio::test]
async fn k05_durable_encoding_is_additive_and_downgrade_safe() {
    use k05_durable::*;

    let x = k05_peer(K05_X);
    let y = k05_peer(K05_Y);

    // The two pseudonyms are the ones the captured bytes embed.
    assert!(K05_G1_CONSENT_GRANTED.contains(K05_X) && K05_G3_CONSENT_REVOKED.contains(K05_X));
    assert!(K05_G2_ADMISSION_DEFERRED_APPROVAL.contains(K05_Y));
    assert_ne!(x, y);

    // ── AC2(b): `principal: None` lines are byte-identical to HEAD ─────────
    // Leg is an explicit reconstruction of the known entry with the captured
    // seq / wall stamp; it is serialized by the NEW code.
    for golden in k05_goldens() {
        assert_eq!(
            k05_line(&golden.entry),
            golden.bytes,
            "{}: new serializer must emit the HEAD bytes for principal None",
            golden.name
        );
        assert!(
            !golden.bytes.contains("principal"),
            "{}: no pre-change line carries a principal member",
            golden.name
        );
    }
    assert!(
        K05_G3_CONSENT_REVOKED.contains(r#""event":"consent_revoked""#),
        "ConsentRevoked has no principal member, before or after"
    );

    // ── AC2(a): the new reader decodes every pre-change line shape ─────────
    let g1 = k05_decode(K05_G1_CONSENT_GRANTED);
    assert_eq!(
        (g1.schema_version, g1.seq, g1.recorded_at_ms),
        (1, 1, 1790974662796)
    );
    assert_eq!(
        k05_granted_fields(&g1),
        (Some(x.clone()), 1790974662796, None),
        "fieldless consent_granted decodes with principal None"
    );
    let g1_serve = k05_decode(K05_G1_SERVE_CONSENT_GRANTED);
    assert_eq!(
        k05_granted_fields(&g1_serve),
        (Some(y.clone()), 1790974823216, None)
    );
    let g2_approval = k05_decode(K05_G2_ADMISSION_DEFERRED_APPROVAL);
    assert_eq!(
        k05_deferred_fields(&g2_approval),
        (
            AgentId::root(),
            y.as_str().to_string(),
            K05_G2_APPROVAL_GATE.to_string(),
            None
        ),
        "admission_deferred without principal decodes with principal None"
    );
    let g2_status = k05_decode(K05_G2_ADMISSION_DEFERRED_STATUS_QUERY);
    assert_eq!(
        k05_deferred_fields(&g2_status),
        (
            AgentId::root(),
            y.as_str().to_string(),
            K05_G2_STATUS_GATE.to_string(),
            None
        )
    );
    let g3 = k05_decode(K05_G3_CONSENT_REVOKED);
    assert_eq!(k05_revoked_fields(&g3), (Some(x.clone()), 1700000000000));
    for golden in k05_goldens() {
        let decoded = k05_decode(golden.bytes);
        assert_eq!(
            (decoded.schema_version, decoded.seq, decoded.recorded_at_ms),
            (1, golden.entry.seq, golden.entry.recorded_at_ms),
            "{}: captured seq / stamp survive decode",
            golden.name
        );
        assert_eq!(
            k05_line(&decoded),
            golden.bytes,
            "{}: decode -> encode is the identity on the captured line",
            golden.name
        );
    }

    // ── AC2(a)+(d): G4 decodes with an in-memory `principal: None` ─────────
    let g4 = k05_decode(K05_G4_APPROVAL_RESOLVED_REMOTE_PEER);
    let g4_source = k05_approval_source(&g4);
    let ApprovalSource::RemotePeer {
        conversation_id,
        peer_id,
        principal,
    } = &g4_source
    else {
        panic!("G4 decodes as ApprovalSource::RemotePeer, got {g4_source:?}");
    };
    assert_eq!(conversation_id, K05_CONVERSATION);
    assert_eq!(peer_id, &x, "the serialized peer_id member is kept");
    assert_eq!(principal, &None, "a deserialized source has no principal");
    assert_eq!(
        g4_source.policy_principal(),
        None,
        "a deserialized ApprovalSource never yields a PolicyPrincipal (no provenance is synthesized from peer_id)"
    );
    match k05_room(&g4) {
        RoomEvent::ApprovalResolved {
            node,
            fingerprint,
            outcome,
            ..
        } => {
            assert_eq!(node, &AgentId::root());
            assert_eq!(fingerprint, &InvocationFingerprint([0xAB; 32]));
            assert_eq!(outcome, &ApprovalOutcome::Once);
        }
        other => panic!("G4 is an approval_resolved row, got {other:?}"),
    }

    // ── AC2(b) D1: the source serializes as G4 for ANY in-memory principal ─
    let rap = PolicyPrincipal::rap(x.clone());
    let pseudonymous = PolicyPrincipal::Pseudonymous(x.clone());
    for (label, carried) in [("rap", &rap), ("pseudonymous", &pseudonymous)] {
        let source = k05_remote_source(carried);
        assert_eq!(
            source.policy_principal(),
            Some(carried),
            "{label}: the in-memory principal is carried (positive control for the None below)"
        );
        let source_json = serde_json::to_string(&source).expect("source serializes");
        assert_eq!(
            source_json,
            k05_g4_source_json(),
            "{label}: RemotePeer wire shape has no new member and keeps peer_id"
        );
        let decoded_source: ApprovalSource =
            serde_json::from_str(&source_json).expect("the bytes it wrote decode");
        assert_eq!(
            decoded_source.policy_principal(),
            None,
            "{label}: decoding the bytes it wrote recovers no principal"
        );
        assert_eq!(decoded_source.scope_agent_id(), source.scope_agent_id());
        let entry = k05_entry(3, 1790974663113, k05_approval_event(source));
        assert_eq!(
            k05_line(&entry),
            K05_G4_APPROVAL_RESOLVED_REMOTE_PEER,
            "{label}: whole G4 line, byte for byte"
        );
    }

    // ── AC2(a)+(b): typed values round-trip; new lines carry the member only
    //    when Some, as the last payload member; schema_version stays 1 ──────
    for name in K05_PRINCIPAL_BEARING {
        let golden = k05_golden(name);
        for (kind, wire) in k05_kinds() {
            let expected = k05_with_principal(golden.bytes, wire);
            let typed_entry = k05_set_principal(&golden.entry, Some(kind));
            assert_eq!(
                k05_line(&typed_entry),
                expected,
                "{name}/{wire}: new writer appends exactly one snake_case principal member"
            );
            assert!(expected.starts_with(r#"{"schema_version":1,"#));
            let decoded = k05_decode(&expected);
            assert_eq!(decoded, typed_entry, "{name}/{wire}: Some(..) round-trips");
            assert_eq!(
                k05_line(&decoded),
                expected,
                "{name}/{wire}: decode -> encode identity"
            );
        }
        // Explicit null is what a writer without the skip would emit; it still
        // means "no principal".
        let with_null = format!(
            "{},\"principal\":null}}}}}}",
            golden.bytes.strip_suffix("}}}").expect("braces")
        );
        assert_eq!(
            k05_set_principal(&k05_decode(&with_null), None),
            golden.entry,
            "{name}: principal:null decodes like an absent member"
        );
        match k05_room(&k05_decode(&with_null)) {
            RoomEvent::ConsentGranted { principal, .. }
            | RoomEvent::AdmissionDeferred { principal, .. } => assert_eq!(principal, &None),
            other => panic!("{name}: unexpected row {other:?}"),
        }
        // Any other string is a future kind: it quarantines to Some(Unknown)
        // and is never confused with a typed kind.
        for future in [
            "quarantined",
            "unknown",
            "Pinned",
            "Pseudonymous",
            "PINNED",
            "",
        ] {
            let decoded = k05_decode(&k05_with_principal(golden.bytes, future));
            let principal = match k05_room(&decoded) {
                RoomEvent::ConsentGranted { principal, .. }
                | RoomEvent::AdmissionDeferred { principal, .. } => *principal,
                other => panic!("{name}: unexpected row {other:?}"),
            };
            assert_eq!(
                principal,
                Some(PrincipalKind::Unknown),
                "{name}: principal {future:?} must quarantine to Some(Unknown)"
            );
        }
    }

    // ── AC2 real on-disk leg 1: run-1 goldens through a real NodeJournal ───
    let run1 = tempfile::tempdir().expect("tempdir");
    let run1_goldens = [k05_golden("G1"), k05_golden("G3"), k05_golden("G4")];
    let (journal, clock) = k05_open(
        run1.path(),
        "k05-run1",
        run1_goldens
            .iter()
            .map(|g| g.entry.recorded_at_ms)
            .collect(),
    )
    .await;
    for golden in &run1_goldens {
        let written = journal
            .append_room(k05_room(&golden.entry).clone())
            .await
            .expect("append through the real NodeJournal");
        assert_eq!(
            k05_line(&written),
            golden.bytes,
            "{}: the journal's own append emits the captured entry",
            golden.name
        );
    }
    assert_eq!(clock.reads(), 3, "exactly one stamp per append");
    let (run1_bytes, run1_lines) = k05_file_lines(journal.path());
    assert_eq!(
        run1_lines,
        vec![
            K05_G1_CONSENT_GRANTED.to_string(),
            K05_G3_CONSENT_REVOKED.to_string(),
            K05_G4_APPROVAL_RESOLVED_REMOTE_PEER.to_string(),
        ],
        "the file on disk is the captured run-1 bytes"
    );
    let loaded = k05_load(run1.path(), "k05-run1").await;
    assert_eq!(loaded.len(), 3);
    for (entry, golden) in loaded.iter().zip(&run1_goldens) {
        assert_eq!(
            k05_line(entry),
            golden.bytes,
            "{}: reload is lossless",
            golden.name
        );
    }
    assert_eq!(
        k05_granted_fields(&loaded[0]),
        (Some(x.clone()), 1790974662796, None)
    );
    assert_eq!(
        k05_revoked_fields(&loaded[1]),
        (Some(x.clone()), 1700000000000)
    );
    assert_eq!(
        k05_approval_source(&loaded[2]).policy_principal(),
        None,
        "the in-memory principal is never recovered from disk"
    );
    let (old_entries, old_valid, old_file) =
        old_reader::parse_entries(&run1_bytes).expect("old reader reads the real run-1 file");
    assert_eq!(
        (old_entries.len(), old_valid, old_file),
        (3, run1_bytes.len(), run1_bytes.len())
    );
    assert_eq!(
        old_entries[0].record,
        k05_old_room(k05_old_granted(&x, 1790974662796))
    );
    assert_eq!(
        old_entries[1].record,
        k05_old_room(k05_old_revoked(Some(&x), 1700000000000))
    );
    assert_eq!(old_entries[2].record, k05_old_room(k05_old_approval(&x)));

    // ── real on-disk leg 2: run-2 goldens at their captured seq 6/7/8 ──────
    // Seq 1..=5 are inert fillers (a revoke with no sender) only so the real
    // journal reaches the captured sequence numbers; they are not goldens.
    let run2 = tempfile::tempdir().expect("tempdir");
    let run2_goldens = [
        k05_golden("G2approval"),
        k05_golden("G2status"),
        k05_golden("G1serve"),
    ];
    let mut run2_stamps: Vec<i64> = (0..5).map(|n| 1790974823000 + n).collect();
    run2_stamps.extend(run2_goldens.iter().map(|g| g.entry.recorded_at_ms));
    let (journal2, clock2) = k05_open(run2.path(), "k05-run2", run2_stamps).await;
    for _ in 0..5 {
        journal2
            .append_room(k05_filler_event())
            .await
            .expect("append filler");
    }
    for golden in &run2_goldens {
        let written = journal2
            .append_room(k05_room(&golden.entry).clone())
            .await
            .expect("append through the real NodeJournal");
        assert_eq!(
            written.seq, golden.entry.seq,
            "{}: captured seq",
            golden.name
        );
        assert_eq!(
            k05_line(&written),
            golden.bytes,
            "{}: captured bytes",
            golden.name
        );
    }
    assert_eq!(clock2.reads(), 8);
    let (run2_bytes, run2_lines) = k05_file_lines(journal2.path());
    assert_eq!(run2_lines.len(), 8);
    assert_eq!(run2_lines[5], K05_G2_ADMISSION_DEFERRED_APPROVAL);
    assert_eq!(run2_lines[6], K05_G2_ADMISSION_DEFERRED_STATUS_QUERY);
    assert_eq!(run2_lines[7], K05_G1_SERVE_CONSENT_GRANTED);
    let loaded2 = k05_load(run2.path(), "k05-run2").await;
    assert_eq!(loaded2.len(), 8);
    assert_eq!(
        k05_deferred_fields(&loaded2[5]),
        (
            AgentId::root(),
            y.as_str().to_string(),
            K05_G2_APPROVAL_GATE.to_string(),
            None
        )
    );
    assert_eq!(
        k05_deferred_fields(&loaded2[6]),
        (
            AgentId::root(),
            y.as_str().to_string(),
            K05_G2_STATUS_GATE.to_string(),
            None
        )
    );
    assert_eq!(
        k05_granted_fields(&loaded2[7]),
        (Some(y.clone()), 1790974823216, None)
    );
    let (old2, valid2, file2) =
        old_reader::parse_entries(&run2_bytes).expect("old reader reads the real run-2 file");
    assert_eq!(
        (old2.len(), valid2, file2),
        (8, run2_bytes.len(), run2_bytes.len())
    );
    assert_eq!(
        old2[5].record,
        k05_old_room(k05_old_deferred(&y, K05_G2_APPROVAL_GATE))
    );
    assert_eq!(
        old2[6].record,
        k05_old_room(k05_old_deferred(&y, K05_G2_STATUS_GATE))
    );
    assert_eq!(
        old2[7].record,
        k05_old_room(k05_old_granted(&y, 1790974823216))
    );

    // ── real on-disk leg 3: NEW typed writers + Some(rap)/Some(pseudo) G4 ──
    let typed_dir = tempfile::tempdir().expect("tempdir");
    let mut typed_events: Vec<RoomEvent> = Vec::new();
    let mut typed_old: Vec<old_reader::RoomEvent> = Vec::new();
    for (kind, _) in k05_kinds() {
        typed_events.push(k05_granted_event(&x, 100, Some(kind)));
        typed_old.push(k05_old_granted(&x, 100));
    }
    for (kind, _) in k05_kinds() {
        typed_events.push(k05_deferred_event(&y, K05_G2_APPROVAL_GATE, Some(kind)));
        typed_old.push(k05_old_deferred(&y, K05_G2_APPROVAL_GATE));
    }
    typed_events.push(k05_approval_event(k05_remote_source(&rap)));
    typed_old.push(k05_old_approval(&x));
    typed_events.push(k05_approval_event(k05_remote_source(&pseudonymous)));
    typed_old.push(k05_old_approval(&x));
    typed_events.push(k05_revoked_event(&x, 200));
    typed_old.push(k05_old_revoked(Some(&x), 200));
    let (typed_journal, typed_clock) = k05_open(
        typed_dir.path(),
        "k05-typed",
        (0..typed_events.len() as i64)
            .map(|n| 1790975000000 + n)
            .collect(),
    )
    .await;
    for event in &typed_events {
        let written = typed_journal
            .append_room(event.clone())
            .await
            .expect("append typed row");
        if let RoomEvent::ApprovalResolved { source, .. } = event {
            assert!(
                source.policy_principal().is_some(),
                "the in-memory source handed to the journal carries a principal"
            );
            assert!(
                k05_line(&written).ends_with(&format!(
                    r#""source":{},"outcome":"Once"}}}}}}"#,
                    k05_g4_source_json()
                )),
                "its durable line carries the unchanged RemotePeer shape"
            );
        }
    }
    assert_eq!(typed_clock.reads(), typed_events.len());
    let (typed_bytes, typed_lines) = k05_file_lines(typed_journal.path());
    assert_eq!(typed_lines.len(), typed_events.len());
    for (index, (_, wire)) in k05_kinds().into_iter().enumerate() {
        assert!(
            typed_lines[index]
                .ends_with(&format!(r#","granted_at":100,"principal":"{wire}"}}}}}}"#)),
            "granted row {index} wire form"
        );
        assert!(
            typed_lines[3 + index].ends_with(&format!(r#","principal":"{wire}"}}}}}}"#)),
            "deferred row {index} wire form"
        );
    }
    let typed_loaded = k05_load(typed_dir.path(), "k05-typed").await;
    assert_eq!(typed_loaded.len(), typed_events.len());
    for (index, (kind, _)) in k05_kinds().into_iter().enumerate() {
        assert_eq!(
            k05_granted_fields(&typed_loaded[index]),
            (Some(x.clone()), 100, Some(kind)),
            "granted {kind:?} survives the real file"
        );
        assert_eq!(
            k05_deferred_fields(&typed_loaded[3 + index]).3,
            Some(kind),
            "deferred {kind:?} survives the real file"
        );
    }
    assert_eq!(
        k05_approval_source(&typed_loaded[6]).policy_principal(),
        None
    );
    assert_eq!(
        k05_approval_source(&typed_loaded[7]).policy_principal(),
        None
    );
    assert_eq!(
        k05_line(&typed_loaded[6]).split(r#""source":"#).nth(1),
        k05_line(&typed_loaded[7]).split(r#""source":"#).nth(1),
        "rap and pseudonymous sources are indistinguishable on disk (no provenance is durable)"
    );
    let (typed_old_entries, typed_valid, typed_file) =
        old_reader::parse_entries(&typed_bytes).expect("old reader reads the real typed file");
    assert_eq!(
        (typed_old_entries.len(), typed_valid, typed_file),
        (typed_events.len(), typed_bytes.len(), typed_bytes.len()),
        "no typed row is dropped or tail-tolerated"
    );
    for (index, expected) in typed_old.into_iter().enumerate() {
        assert_eq!(
            typed_old_entries[index].record,
            k05_old_room(expected),
            "row {index}: the old reader sees the pre-change sender/spoke/peer"
        );
    }

    // ── real on-disk leg 4: forward-written future strings ─────────────────
    let forward_dir = tempfile::tempdir().expect("tempdir");
    let (forward_journal, _) = k05_open(forward_dir.path(), "k05-forward", Vec::new()).await;
    let xj = k05_json_str(K05_X);
    let forward_lines = vec![
        k05_raw(1, 1, &k05_payload_granted(&xj, 1, "")),
        k05_raw(
            1,
            2,
            &k05_payload_granted(&xj, 2, r#","principal":"quarantined""#),
        ),
        k05_raw(
            1,
            3,
            &format!(
                r#"{{"event":"admission_deferred","coordinator":"root","spoke":"{}","gate":"{}","principal":"quarantined"}}"#,
                K05_Y, K05_G2_STATUS_GATE
            ),
        ),
        k05_raw(
            1,
            4,
            &k05_payload_granted(&xj, 4, r#","principal":"unknown""#),
        ),
        k05_raw(1, 5, &k05_payload_granted(&xj, 5, r#","principal":null"#)),
    ];
    let forward_bytes = k05_join(&forward_lines);
    std::fs::write(forward_journal.path(), &forward_bytes).expect("forward-write the journal");
    let forward_loaded = k05_load(forward_dir.path(), "k05-forward").await;
    assert_eq!(forward_loaded.len(), 5);
    assert_eq!(k05_granted_fields(&forward_loaded[0]).2, None);
    assert_eq!(
        k05_granted_fields(&forward_loaded[1]).2,
        Some(PrincipalKind::Unknown)
    );
    assert_eq!(
        k05_deferred_fields(&forward_loaded[2]).3,
        Some(PrincipalKind::Unknown)
    );
    assert_eq!(
        k05_granted_fields(&forward_loaded[3]).2,
        Some(PrincipalKind::Unknown)
    );
    assert_eq!(k05_granted_fields(&forward_loaded[4]).2, None);
    let (forward_old, forward_valid, forward_file) =
        old_reader::parse_entries(&forward_bytes).expect("old reader reads forward-written file");
    assert_eq!(
        (forward_old.len(), forward_valid, forward_file),
        (5, forward_bytes.len(), forward_bytes.len())
    );

    // ── AC2(c): old reader, NON-tail, from the NEW writer's in-memory rows ─
    // `None` is the pre-change baseline: every typed row must decode to the
    // very same old event as its principal-less twin.
    let principals: [Option<PrincipalKind>; 4] = [
        None,
        Some(PrincipalKind::Pinned),
        Some(PrincipalKind::Pseudonymous),
        Some(PrincipalKind::Unknown),
    ];
    for principal in principals {
        let label = format!("granted/{principal:?}");
        let bytes = k05_old_reads_nontail(
            &label,
            &[],
            k05_entry(
                1,
                1790974662796,
                k05_granted_event(&x, 1790974662796, principal),
            ),
            k05_old_granted(&x, 1790974662796),
            &x,
        );
        let first = String::from_utf8(bytes).expect("utf8");
        let first = first.lines().next().expect("first line").to_string();
        match principal {
            None => assert_eq!(first, K05_G1_CONSENT_GRANTED, "baseline row is G1"),
            Some(kind) => {
                let wire = k05_kinds()
                    .into_iter()
                    .find(|(candidate, _)| *candidate == kind)
                    .map(|(_, wire)| wire)
                    .expect("kind has a wire string");
                assert_eq!(first, k05_with_principal(K05_G1_CONSENT_GRANTED, wire));
            }
        }
        for gate in [K05_G2_APPROVAL_GATE, K05_G2_STATUS_GATE] {
            let label = format!("deferred/{gate}/{principal:?}");
            let bytes = k05_old_reads_nontail(
                &label,
                &[],
                k05_entry(1, 1790974823204, k05_deferred_event(&y, gate, principal)),
                k05_old_deferred(&y, gate),
                &y,
            );
            let first = String::from_utf8(bytes).expect("utf8");
            let first = first.lines().next().expect("first line").to_string();
            match principal {
                None => assert!(!first.contains("principal"), "{label}: baseline row"),
                Some(kind) => {
                    let wire = k05_kinds()
                        .into_iter()
                        .find(|(candidate, _)| *candidate == kind)
                        .map(|(_, wire)| wire)
                        .expect("kind has a wire string");
                    assert!(
                        first.ends_with(&format!(r#","principal":"{wire}"}}}}}}"#)),
                        "{label}: typed row"
                    );
                }
            }
        }
    }
    // G4: a value whose in-memory principal is Some, serialized by the NEW
    // code, is byte-identical to G4 (so there is no new shape to tolerate) and
    // the old reader reads the same peer_id, non-tail.
    for (label, carried) in [("rap", &rap), ("pseudonymous", &pseudonymous)] {
        let fillers = [
            k05_entry(1, 1790974663100, k05_filler_event()),
            k05_entry(2, 1790974663101, k05_filler_event()),
        ];
        let target = k05_entry(
            3,
            1790974663113,
            k05_approval_event(k05_remote_source(carried)),
        );
        let bytes = k05_old_reads_nontail(
            &format!("approval/{label}"),
            &fillers,
            target,
            k05_old_approval(&x),
            &x,
        );
        let text = String::from_utf8(bytes).expect("utf8");
        assert_eq!(
            text.lines().nth(2),
            Some(K05_G4_APPROVAL_RESOLVED_REMOTE_PEER),
            "approval/{label}: the old reader is handed exactly G4"
        );
    }

    // ── AC2(c): the old-reader copy has no whole-event fallback ────────────
    // It accepts an extra `principal` member ONLY because serde skips unknown
    // struct-variant members; an unknown event tag (no `#[serde(other)]` on
    // the old enum) is a hard CorruptRecord, so the harness can fail.
    let known_good = |seq: u64| k05_raw(1, seq, &k05_payload_revoked(&xj, 9));
    for extra in [
        r#","principal":"quarantined""#,
        r#","principal":"pinned""#,
        r#","principal":{"future":[1,2,3]}"#,
    ] {
        let bytes = k05_join(&[
            k05_raw(1, 1, &k05_payload_granted(&xj, 1, extra)),
            known_good(2),
        ]);
        let (entries, valid, file) = old_reader::parse_entries(&bytes)
            .unwrap_or_else(|error| panic!("extra member {extra} must be skipped: {error:?}"));
        assert_eq!((entries.len(), valid, file), (2, bytes.len(), bytes.len()));
        assert_eq!(entries[0].record, k05_old_room(k05_old_granted(&x, 1)));
    }
    let unknown_event = k05_join(&[
        k05_raw(
            1,
            1,
            &format!(r#"{{"event":"consent_granted_v2","sender":{xj},"granted_at":1}}"#),
        ),
        known_good(2),
    ]);
    assert!(
        matches!(
            old_reader::parse_entries(&unknown_event),
            Err(old_reader::OldJournalError::CorruptRecord { line: 1, .. })
        ),
        "an unknown event tag is not absorbed by any old-reader fallback"
    );

    // ── Negative controls: the harness can fail ────────────────────────────
    // forbidden `sender` object form, non-tail
    let sender_object =
        k05_payload_granted(&format!(r#"{{"peer":"{K05_X}","kind":"pinned"}}"#), 5, "");
    let object_bytes = k05_join(&[k05_raw(1, 1, &sender_object), known_good(2)]);
    assert!(
        matches!(
            old_reader::parse_entries(&object_bytes),
            Err(old_reader::OldJournalError::CorruptRecord { line: 1, .. })
        ),
        "a sender object breaks the old reader when non-tail"
    );
    let object_after_good =
        k05_join(&[known_good(1), k05_raw(1, 2, &sender_object), known_good(3)]);
    assert!(
        matches!(
            old_reader::parse_entries(&object_after_good),
            Err(old_reader::OldJournalError::CorruptRecord { line: 2, .. })
        ),
        "the defect is reported at the 1-based index of the first bad line"
    );
    // typed sender-type change on the real new serializer output
    let pinned_line = k05_line(&k05_entry(
        1,
        1,
        k05_granted_event(&x, 1, Some(PrincipalKind::Pinned)),
    ));
    let object_form = pinned_line.replacen(
        &format!("\"sender\":\"{K05_X}\""),
        &format!("\"sender\":{{\"peer\":\"{K05_X}\",\"kind\":\"pinned\"}}"),
        1,
    );
    assert_ne!(
        object_form, pinned_line,
        "fixture really changed the sender type"
    );
    assert!(matches!(
        old_reader::parse_entries(&k05_join(&[object_form, known_good(2)])),
        Err(old_reader::OldJournalError::CorruptRecord { line: 1, .. })
    ));
    // schema_version 2
    let schema_two = pinned_line.replacen("\"schema_version\":1", "\"schema_version\":2", 1);
    assert_ne!(schema_two, pinned_line, "fixture really bumped the schema");
    assert!(matches!(
        old_reader::parse_entries(&k05_join(&[schema_two.clone(), known_good(2)])),
        Err(old_reader::OldJournalError::UnsupportedSchema {
            found: 2,
            supported: 1
        })
    ));
    // sequence gap
    assert!(matches!(
        old_reader::parse_entries(&k05_join(&[known_good(1), known_good(3)])),
        Err(old_reader::OldJournalError::InvalidSequence {
            expected: 2,
            found: 3
        })
    ));
    // first line not seq 1
    assert!(matches!(
        old_reader::parse_entries(&k05_join(&[known_good(2)])),
        Err(old_reader::OldJournalError::InvalidSequence {
            expected: 1,
            found: 2
        })
    ));
    // non-tail garbage is CorruptRecord
    assert!(matches!(
        old_reader::parse_entries(&k05_join(&["not json".to_string(), known_good(2)])),
        Err(old_reader::OldJournalError::CorruptRecord { line: 1, .. })
    ));
    // tail tolerance: the same defects as the LAST terminated line are dropped
    let good_one = known_good(1);
    for (label, tail) in [
        ("sender object", k05_raw(1, 2, &sender_object)),
        ("garbage", "{\"schema_version\":1,".to_string()),
    ] {
        let bytes = k05_join(&[good_one.clone(), tail]);
        let (entries, valid, file) =
            old_reader::parse_entries(&bytes).unwrap_or_else(|e| panic!("{label} tail: {e:?}"));
        assert_eq!(
            entries.len(),
            1,
            "{label}: malformed last line is tolerated"
        );
        assert_eq!(valid, good_one.len() + 1);
        assert_eq!(file, bytes.len());
        assert!(
            valid < file,
            "{label}: the malformed tail is excluded from valid_len"
        );
    }
    // tail tolerance: a final UNTERMINATED line is ignored even when valid
    let mut unterminated = k05_join(std::slice::from_ref(&good_one));
    unterminated.extend_from_slice(known_good(2).as_bytes());
    let (entries, valid, file) =
        old_reader::parse_entries(&unterminated).expect("unterminated tail is ignored");
    assert_eq!(entries.len(), 1);
    assert_eq!(valid, good_one.len() + 1);
    assert_eq!(file, unterminated.len());
    // a schema_version 2 line parses, so it is NOT tail-tolerated: it fails
    // the whole load even as the last terminated line
    assert!(matches!(
        old_reader::parse_entries(&k05_join(&[schema_two])),
        Err(old_reader::OldJournalError::UnsupportedSchema { .. })
    ));
}
