//! Story 19.18c — per-item capability keystones.
//!
//! Every keystone enters through a production front door:
//! - **answering host B**: a real `serve()` with the production
//!   `AttachServer` runtime (19.18's `Host`); non-loopback keystones bind
//!   `0.0.0.0:0` with TLS + API keys, `threadReads` parsed from a written
//!   `a2a.json` — ⛔ never a constructed `ThreadReads`;
//! - **senders**: 19.18's `Sender` plus its own key dir, composed through the
//!   SAME loader helper both roots call (`load_item_capability_key`), driving
//!   `submit_message_for_test` → `transparency_bridge::team_command` → the
//!   rail-3 spawn → the published `AppEvent` → its production handler;
//! - **the co-holder**: a raw `reqwest` client presenting the same API key
//!   over the same TLS endpoint — exactly what a peer holding the key has;
//! - **the model rail**: `A2aEgress::compose` → the composed `A2aProvider` →
//!   `CapabilityProvider::invoke`.
//!
//! Copied, ⛔ not imported (everything there is file-private), from
//! `tests/conformance_19_18_a2a_reply_thread.rs:34-1005`: `Journal`,
//! `open_journal`/`journal`, `ModePolicy`, `ScriptedProvider`,
//! `build_runtime`, `HostOptions`, `Host` (+ `restart`), `attach_operator`,
//! `Sender`, `new_sender`, `sender_over`, `app_state`, `settle_send`,
//! `room_events`, `rpc`, `waiting_node`, `next_event`, `read_thread_block`,
//! `read_board`, `board_item_ids`, `next_answer`, `operator_reply`,
//! `persisted_card`, `pending_card`, `thread_rpc`, `served_ids`,
//! `frame_turn`; `approve_next` from `:1631-1653`; 19.17's fan-out hosts are
//! the same `Host`. Deliberate differences: `HostOptions.consent` (production composes
//! the AttachServer with a consent projection, `P16`), `HostOptions.provider_gate`
//! (`K13`: the host provider held on a `Notify`, never a sleep),
//! `Host::restart_on_new_port` (`K06d`), and `Sender.keys` (`P15`).
//!
//! ⛔ Forbidden bypasses, none used: calling `pre_lookup`/`post_lookup`,
//! `items_thread`, `scoped_task` or the projection directly for an end-to-end
//! claim; `runtime.delegate` with a hand-built `TaskClient`; `PeerFixture`; a
//! `runtime: None` host; a constructed `ThreadReads`. The only hand-appended
//! `RecipientItemReceived` rows are labelled as another build's ingress
//! (`K08`'s fail-closed and positive-control rows).

#![cfg(feature = "a2a")]

use std::sync::{Once, OnceLock};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rustain::adapters::a2a::item_capability::{
    ItemCapabilityKey, ItemCapabilityKeyStore, derive_item_capability, load_item_capability_key,
};
use rustain::adapters::tui::app::handle_input;
use rustain::adapters::tui::handlers::team_command::{
    open_team_retract_card, resolve_team_retract_card,
};
use rustain::domain::events::DomainInputEvent;
use rustain::infrastructure::runtime::transparency_bridge::team_retract_dispatch;
use sha2::Digest as _;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use futures::stream::BoxStream;
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::a2a::auth::A2aServerSecurity;
use rustain::adapters::a2a::card_cache::SignedCardCache;
use rustain::adapters::a2a::egress::A2aEgress;
use rustain::adapters::a2a::server::{ServeConfig, serve};
use rustain::adapters::a2a::transparency::TransparencySink;
use rustain::adapters::daemon::protocol::{
    AttachMode, ClientFrame, ConnectionTier, DaemonFrame, PROTOCOL_VERSION, RecipientAction,
    RecipientActionOutcome, RecipientActionTarget, read_frame, write_frame,
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
    A2aPeerSource, A2aPeerSpec, AppConfig, CompletionOptions, Conversation, FeedbackLevel,
    JournalRecord, Message, ResponseMode, RoomEvent, StopReason, StreamChunk,
};
use rustain::domain::ports::{
    DeliveryPolicy, RoomJournal, RoomJournalError, RoomJournalReader, SecurityPort, StoragePort,
    StreamingProvider, ToolSetPort,
};
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::infrastructure::runtime::app_state::AppState;
use rustain::infrastructure::runtime::transparency_bridge::{
    team_command, team_send_settled, team_thread_ready,
};
use rustain::infrastructure::subagent::{NodeJournal, NodeRoomJournal, NodeTree};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const SETTLE: Duration = Duration::from_secs(45);

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

async fn journal() -> Journal {
    open_journal(Arc::new(tempfile::tempdir().expect("workspace"))).await
}

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
        _message_type: rustain::domain::models::SemanticMessageType,
    ) -> rustain::domain::ports::PeerResponsePolicy {
        rustain::domain::ports::PeerResponsePolicy {
            mode: self.0,
            auto_response: None,
            ..Default::default()
        }
    }
}

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

/// Story 19.18c `K13` — `ScriptedProvider` held on a `Notify`.
struct GatedProvider {
    text: String,
    gate: Arc<Notify>,
}

#[async_trait::async_trait]
impl StreamingProvider for GatedProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<Message>,
        _options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        use futures::StreamExt;
        let gate = self.gate.clone();
        let text = self.text.clone();
        Ok(futures::stream::once(async move {
            gate.notified().await;
            futures::stream::iter(vec![
                StreamChunk::Text {
                    content: text,
                    parent_tool_use_id: None,
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                },
            ])
        })
        .flatten()
        .boxed())
    }
    async fn abort(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    fn provider_id(&self) -> String {
        "reply-gated".to_owned()
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
        recipient_context: Arc::new(Default::default()),
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

const CONVERSATION_ID: &str = "reply-recipient";

/// How one answering host is composed. `serve_wrap` wraps the served sink;
/// `attach_wrap` wraps the AttachServer's journal (`P16`: both are wrappable,
/// or `M07`/`M25` cannot inject a failure).
#[derive(Clone)]
struct HostOptions {
    policy: A2aAdmissionPolicy,
    mode: ResponseMode,
    draft_mentions_workspace: bool,
    serve_wrap: Option<Wrap>,
    attach_wrap: Option<Wrap>,
    security: A2aServerSecurity,
    bind: &'static str,
    /// Story 19.18c `P16` — compose the AttachServer with a consent
    /// projection, as production does (`daemon/mod.rs:560-572`).
    consent: bool,
    /// Story 19.18c `K13` — hold the host's provider until notified, so the
    /// first answer is `working` and the model rail polls (⛔ never a sleep).
    provider_gate: Option<Arc<Notify>>,
}

type Wrap = Arc<dyn Fn(Arc<dyn RoomJournal>) -> Arc<dyn RoomJournal> + Send + Sync>;

impl HostOptions {
    fn new(policy: A2aAdmissionPolicy, mode: ResponseMode) -> Self {
        Self {
            policy,
            mode,
            draft_mentions_workspace: false,
            serve_wrap: None,
            attach_wrap: None,
            security: A2aServerSecurity::default(),
            bind: "127.0.0.1:0",
            consent: false,
            provider_gate: None,
        }
    }
}

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
    async fn serve(policy: A2aAdmissionPolicy, mode: ResponseMode) -> Self {
        Self::serve_with(HostOptions::new(policy, mode)).await
    }

    async fn serve_with(options: HostOptions) -> Self {
        let listener = std::net::TcpListener::bind(options.bind).expect("bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let keys = Arc::new(tempfile::tempdir().expect("keys"));
        Self::compose(journal().await, listener, keys, options, false).await
    }

    async fn compose(
        journal: Journal,
        listener: std::net::TcpListener,
        keys: Arc<tempfile::TempDir>,
        options: HostOptions,
        restarted: bool,
    ) -> Self {
        let (domain_tx, domain_rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
        // Story 19.18c: bound to this host as the daemon composes it
        // (`daemon/mod.rs:381`) — recovery restores only this host's nodes,
        // so without the binding a restart would see no orphan (`K10`).
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
        let draft = if options.draft_mentions_workspace {
            format!("the answer is in {}/notes.txt", journal.path.display())
        } else {
            "automatic reply".to_owned()
        };
        let core = {
            let workspace = journal.path.clone();
            let storage = storage.clone();
            let gate = options.provider_gate.clone();
            Arc::new(DaemonCore::new(
                workspace.clone(),
                Arc::new(ArcSwap::from_pointee(AppConfig::default())),
                Arc::new(NoOpMemory),
                storage.clone(),
                Arc::new(NoOpSecurity),
                Arc::new(NoOpPersona),
                Arc::new(rustain::adapters::rap::PeerTopicStore::new()),
                Box::new(move || {
                    let provider: Arc<dyn StreamingProvider> = match &gate {
                        Some(gate) => Arc::new(GatedProvider {
                            text: draft.clone(),
                            gate: gate.clone(),
                        }),
                        None => Arc::new(ScriptedProvider {
                            text: draft.clone(),
                        }),
                    };
                    Ok(build_runtime(provider, storage.clone(), &workspace))
                }),
            ))
        };
        let delivery: Arc<dyn DeliveryPolicy> = Arc::new(ModePolicy(options.mode));
        let attach_journal = match &options.attach_wrap {
            Some(wrap) => wrap(journal.room.clone()),
            None => journal.room.clone(),
        };
        let serve_sink = match &options.serve_wrap {
            Some(wrap) => wrap(journal.room.clone()),
            None => journal.room.clone(),
        };
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
            attach_journal,
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
        let cancel = CancellationToken::new();
        let http = tokio::spawn(serve(
            tokio_listener,
            ServeConfig {
                registry: Arc::new(CapabilityRegistry::new(None)),
                signer,
                security: options.security.clone(),
                runtime: Some(server.clone() as Arc<dyn rustain::domain::ports::InboundPeerRuntime>),
                // ⛔ The one deliberate difference from 19.17's `Host`: the
                // sink carries the READER, as production composes it
                // (`daemon/mod.rs:596-599`). Without it `latest_seq` answers
                // 0, the served fold never refreshes, and a reply turn written
                // on the daemon rail would be invisible to the thread read.
                transparency: Arc::new(
                    TransparencySink::new(serve_sink).with_reader(journal.node.clone()),
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

    fn peer(&self, alias: &str) -> A2aPeerSpec {
        A2aPeerSpec::new(
            alias,
            rustain::domain::models::RedactedUrl::from(self.endpoint.clone()),
            A2aPeerSource::Workspace,
        )
    }

    /// Story 19.18c `K06d` — the same host (journal, keys, workspace) back up
    /// on a NEW port: a moved peer.
    async fn restart_on_new_port(self) -> Self {
        let listener = std::net::TcpListener::bind(self.options.bind).expect("bind");
        listener.set_nonblocking(true).expect("nonblocking");
        self.restart_with(listener).await
    }

    async fn restart(self) -> Self {
        let listener = self.listener.try_clone().expect("keep the one socket");
        self.restart_with(listener).await
    }

    async fn restart_with(self, listener: std::net::TcpListener) -> Self {
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
        // The restarted process takes the daemon singleton; this one's lock
        // must be released first, as a process exit would release it.
        drop(_singleton);
        let workspace = journal.workspace.clone();
        drop(journal);
        let _ = std::fs::remove_file(workspace.path().join("daemon.sock"));
        Self::compose(open_journal(workspace).await, listener, keys, options, true).await
    }
}

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

struct Sender {
    journal: Journal,
    /// Story 19.18c — this sender's data dir holding `item-capability.key`,
    /// reused across an asker restart (mirrors `Host.keys`).
    keys: Arc<tempfile::TempDir>,
    app_state: AppState,
    domain_rx: tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
}

async fn new_sender(peers: Vec<A2aPeerSpec>) -> Sender {
    sender_over(
        peers,
        journal().await,
        Arc::new(tempfile::tempdir().expect("sender keys")),
    )
    .await
}

/// An asker over an existing journal — an asker RESTART is a new process
/// (new egress, new `AppState`) over the same workspace.
/// Story 19.18c — composed through the SAME loader helper both roots call
/// (`P5`), over this sender's own key dir.
async fn sender_over(
    peers: Vec<A2aPeerSpec>,
    journal: Journal,
    keys: Arc<tempfile::TempDir>,
) -> Sender {
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
    Sender {
        journal,
        keys,
        app_state,
        domain_rx,
    }
}

impl Sender {
    async fn type_command(&mut self, state: &mut TuiState, line: &str) {
        state.input_buffer = line.to_owned();
        let InputAction::ExecuteCommand { name, args } = submit_message_for_test(state) else {
            panic!("{line:?} must reach the command dispatcher");
        };
        assert_eq!(name, "team");
        team_command(state, "conv", args.as_deref(), &self.app_state).await;
    }
}

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

async fn settle_send(sender: &mut Sender, state: &mut TuiState) {
    let mut tabs = rustain::domain::models::tab::TabManager::new(CancellationToken::new());
    let event = tokio::time::timeout(SETTLE, sender.domain_rx.recv())
        .await
        .expect("settled")
        .expect("open");
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

async fn rpc(endpoint: &str, body: serde_json::Value) -> serde_json::Value {
    reqwest::Client::new()
        .post(endpoint)
        .json(&body)
        .send()
        .await
        .expect("http")
        .json()
        .await
        .expect("json")
}

async fn waiting_node(host: &Host, task: &str) -> String {
    use base64::Engine;
    let suffix = format!(
        "/t-{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(task)
    );
    let deadline = tokio::time::Instant::now() + SETTLE;
    loop {
        if let Some(entry) = host.node_tree.list().await.into_iter().find(|entry| {
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

// ── Story 19.18 helpers over the front doors ────────────────────────────────

/// The next domain event the asker's spawn published.
async fn next_event(sender: &mut Sender) -> AppEvent {
    tokio::time::timeout(SETTLE, sender.domain_rx.recv())
        .await
        .expect("an asker event before the deadline")
        .expect("domain event channel stays open")
}

/// `/team thread <peer> <item>` through the operator's rail: the typed
/// command → the rail-3 spawn → `TeamThreadReady` → the production handler.
async fn read_thread_block(
    sender: &mut Sender,
    state: &mut TuiState,
    peer: &str,
    item: &str,
) -> String {
    sender
        .type_command(state, &format!("/team thread {peer} {item}"))
        .await;
    assert_eq!(
        state.feedback_blocks[TEAM_THREAD_BLOCK_ID].message, "  reading…",
        "the stable block shows its pending line before the read lands"
    );
    let event = next_event(sender).await;
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

/// `/team board <peer>` through the operator's rail, rendered.
async fn read_board(sender: &mut Sender, state: &mut TuiState, peer: &str) -> String {
    sender
        .type_command(state, &format!("/team board {peer}"))
        .await;
    let event = next_event(sender).await;
    let AppEvent::TeamBoardReady { message, .. } = event else {
        panic!("a board read publishes TeamBoardReady, got {event:?}");
    };
    message
}

/// `P8` — the operator's discovery hop: the item id is parsed from the
/// rendered board line (`    item <id> · …`), ⛔ never read from B's journal.
fn board_item_ids(board: &str) -> Vec<String> {
    board
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("item "))
        .filter_map(|rest| rest.split(' ').next())
        .map(str::to_owned)
        .collect()
}

/// The next operator frame that answers an act — asynchronous events and
/// peer-row operations in between are skipped.
async fn next_answer(operator: &mut tokio::net::UnixStream) -> DaemonFrame {
    loop {
        match tokio::time::timeout(SETTLE, read_frame::<_, DaemonFrame>(operator))
            .await
            .expect("an operator frame before the deadline")
            .expect("readable frame")
            .expect("open socket")
        {
            DaemonFrame::Event(_)
            | DaemonFrame::PeerRowOperation(_)
            | DaemonFrame::ApprovalRequest { .. } => {}
            frame => return frame,
        }
    }
}

async fn operator_reply(
    operator: &mut tokio::net::UnixStream,
    item_id: &str,
    text: &str,
) -> DaemonFrame {
    write_frame(
        operator,
        &ClientFrame::ReplyToRecipientItem {
            item_id: item_id.to_owned(),
            text: text.to_owned(),
        },
    )
    .await
    .expect("operator frame");
    next_answer(operator).await
}

/// The persisted conversation's peer-response row for `node`, if any.
async fn persisted_card(host: &Host, node: &str) -> Option<String> {
    let storage = FileSystemStorage::with_workspace_root(
        rustain::infrastructure::paths::sessions_dir(&host.journal.path),
        host.journal.path.clone(),
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

/// Wait (bounded) until `node`'s card is persisted in a pending shape.
async fn pending_card(host: &Host, node: &str) {
    let deadline = tokio::time::Instant::now() + SETTLE;
    loop {
        if let Some(content) = persisted_card(host, node).await
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

async fn thread_rpc(host: &Host, item_id: &str) -> serde_json::Value {
    rpc(
        &host.endpoint,
        serde_json::json!({
            "jsonrpc": "2.0", "id": 77, "method": "x-rustain-items/thread",
            "params": { "itemId": item_id, "someFutureField": true },
        }),
    )
    .await
}

fn served_ids(thread: &serde_json::Value) -> Vec<String> {
    thread["result"]["turns"]
        .as_array()
        .expect("served turns")
        .iter()
        .map(|turn| turn["turnId"].as_str().expect("turn id").to_owned())
        .collect()
}

fn frame_turn(frame: &DaemonFrame) -> String {
    let DaemonFrame::RecipientActionResult {
        action: RecipientAction::Reply,
        target: RecipientActionTarget::Item { item_id },
        outcome: RecipientActionOutcome::Applied {
            reply: Some(receipt),
        },
    } = frame
    else {
        panic!("expected applied recipient reply result, got {frame:?}");
    };
    assert_eq!(&receipt.item_id, item_id);
    receipt.turn_id.clone()
}

// ── Story 19.18c harness ────────────────────────────────────────────────────

/// `k` — the one API key the headline host serves; every co-holder presents
/// it. `credential-b` is a SECOND configured key: a distinct principal (`K07`).
const KEY: &str = "rustain-19-18c-k";
const CREDENTIAL_B: &str = "rustain-19-18c-credential-b";
/// One environment-variable name per sender role (`P15`): sender A and
/// sender C both present `k`; A presents `credential-b` in `K07`.
const A_ENV: &str = "RUSTAIN_19_18C_A_KEY";
const C_ENV: &str = "RUSTAIN_19_18C_C_KEY";
const A_CREDENTIAL_B_ENV: &str = "RUSTAIN_19_18C_A_CREDENTIAL_B";

static SETUP: Once = Once::new();
static LOG: OnceLock<Arc<std::sync::Mutex<Vec<u8>>>> = OnceLock::new();

/// The one global `tracing` buffer (`K12`, `P10`): TRACE, unfiltered, so a
/// spawned `serve` handler's line lands here too.
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

/// Called FIRST in every test (`P15`): one `Once` sets every variable this
/// file reads (a parallel `set_var` races `getenv`) and installs the one
/// global subscriber.
fn setup() {
    SETUP.call_once(|| {
        // SAFETY: set exactly once, before any test in this binary spawns a
        // client that reads them; the names are unique to this file.
        unsafe {
            std::env::set_var(A_ENV, KEY);
            std::env::set_var(C_ENV, KEY);
            std::env::set_var(A_CREDENTIAL_B_ENV, CREDENTIAL_B);
        }
        let buffer = LOG.get_or_init(Default::default).clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(LogBuffer(buffer))
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("one global subscriber");
    });
}

fn captured_log() -> String {
    String::from_utf8_lossy(
        &LOG.get()
            .expect("setup installed the buffer")
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
    .into_owned()
}

/// A K21-style answering host: `0.0.0.0:0`, TLS, API keys, `threadReads`
/// parsed from its own written `a2a.json` (⛔ never constructed).
struct TlsPeer {
    host: Host,
    dir: tempfile::TempDir,
    cert_path: PathBuf,
}

impl TlsPeer {
    fn port(&self) -> u16 {
        self.host.listener.local_addr().expect("address").port()
    }
}

async fn tls_host(
    credential_scoped: bool,
    policy: A2aAdmissionPolicy,
    mode: ResponseMode,
    keys: &[&str],
    consent: bool,
) -> TlsPeer {
    use rustain::adapters::a2a::admission::ThreadReads;
    use rustain::adapters::a2a::auth::A2aServerAuth;

    let dir = tempfile::tempdir().expect("tls dir");
    let config_path = dir.path().join("a2a.json");
    std::fs::write(
        &config_path,
        if credential_scoped {
            r#"{"server":{"admission":"allow","threadReads":"credential-scoped"}}"#
        } else {
            r#"{"server":{"admission":"allow"}}"#
        },
    )
    .expect("a2a.json");
    let setting: ThreadReads =
        rustain::adapters::a2a::config::parse_workspace_a2a_server_config(&config_path)
            .expect("parse")
            .expect("server block")
            .thread_reads
            .unwrap_or_default();
    let cert =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("certificate");
    let (cert_path, key_path) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
    std::fs::write(&cert_path, cert.cert.pem()).expect("cert");
    std::fs::write(&key_path, cert.signing_key.serialize_pem()).expect("key");
    let material =
        rustain::adapters::a2a::tls::load_tls_material(&cert_path, &key_path).expect("tls");
    let mut options = HostOptions::new(policy, mode);
    options.bind = "0.0.0.0:0";
    options.consent = consent;
    options.security = A2aServerSecurity {
        tls: Some(material),
        auth: Some(A2aServerAuth::ApiKey(
            rustain::adapters::a2a::auth::ApiKeySet::unbound(
                keys.iter().map(|key| (*key).into()).collect(),
            ),
        )),
        thread_reads: setting,
    };
    TlsPeer {
        host: Host::serve_with(options).await,
        dir,
        cert_path,
    }
}

/// A roster entry for a TLS peer, credentialed through `env` and anchored to
/// its certificate — the operator's shape (19.18 `K21`).
fn credentialed(peer: &TlsPeer, alias: &str, env: &str) -> A2aPeerSpec {
    let mut spec = A2aPeerSpec::new(
        alias,
        rustain::domain::models::RedactedUrl::from(format!("https://localhost:{}/", peer.port())),
        A2aPeerSource::Workspace,
    );
    spec.auth = Some(env.to_owned());
    spec.ca_cert = Some(peer.cert_path.clone());
    let _ = peer.dir.path();
    spec
}

/// The co-holder: a raw client presenting `key` over the peer's TLS socket.
struct Raw {
    url: String,
    key: &'static str,
    client: reqwest::Client,
}

fn raw(peer: &TlsPeer, key: &'static str) -> Raw {
    Raw {
        url: format!("https://127.0.0.1:{}/", peer.port()),
        key,
        client: reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .expect("client"),
    }
}

impl Raw {
    /// The WHOLE response body, for byte-identity (never a code alone).
    async fn text(&self, body: serde_json::Value) -> String {
        self.client
            .post(&self.url)
            .header(rustain::adapters::a2a::auth::API_KEY_HEADER, self.key)
            .json(&body)
            .send()
            .await
            .expect("post")
            .text()
            .await
            .expect("body")
    }

    async fn call(&self, body: serde_json::Value) -> serde_json::Value {
        serde_json::from_str(&self.text(body).await).expect("json")
    }
}

fn rpc_body(id: u64, method: &str, params: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

fn send_body(id: u64, message_id: &str, metadata: serde_json::Value) -> serde_json::Value {
    rpc_body(
        id,
        "message/send",
        serde_json::json!({ "message": {
            "kind": "message", "messageId": message_id, "role": "user",
            "parts": [{ "kind": "text", "text": "text" }], "metadata": metadata,
        }}),
    )
}

fn thread_body(id: u64, item: &str, capability: Option<&str>) -> serde_json::Value {
    let mut params = serde_json::json!({ "itemId": item });
    if let Some(capability) = capability {
        params["capability"] = capability.into();
    }
    rpc_body(id, rustain::adapters::a2a::ITEMS_THREAD_METHOD, params)
}

fn retract_body(id: u64, item: &str, capability: Option<&str>) -> serde_json::Value {
    let mut params = serde_json::json!({ "itemId": item });
    if let Some(capability) = capability {
        params["capability"] = capability.into();
    }
    rpc_body(id, rustain::adapters::a2a::ITEMS_RETRACT_METHOD, params)
}

fn task_body(id: u64, method: &str, task: &str, capability: Option<&str>) -> serde_json::Value {
    let mut params = serde_json::json!({ "id": task });
    if let Some(capability) = capability {
        params["metadata"] = serde_json::json!({ "x-rustain-item-capability": capability });
    }
    rpc_body(id, method, params)
}

/// This sender's key, loaded the way its root loaded it.
fn sender_key(sender: &Sender) -> ItemCapabilityKey {
    ItemCapabilityKeyStore::new(sender.keys.path())
        .load()
        .expect("the sender's key")
}

fn wire(key: &ItemCapabilityKey, task: &str) -> String {
    derive_item_capability(key, task).as_wire_str().to_owned()
}

/// `hex(sha256(decode(capability)))` — what the recipient stores.
fn stored_hash(capability: &str) -> String {
    hex::encode(sha2::Sha256::digest(
        URL_SAFE_NO_PAD
            .decode(capability)
            .expect("a wire capability decodes"),
    ))
}

/// A capability under a DIFFERENT key: what a co-holder can mint.
fn stranger(task: &str) -> String {
    wire(&ItemCapabilityKey::from_file_bytes(&[0xEE; 32]), task)
}

/// The forgeable UNKEYED guess (`M07`): a hash of the served task.
fn unkeyed(task: &str) -> String {
    URL_SAFE_NO_PAD.encode(blake3::hash(task.as_bytes()).as_bytes())
}

/// One recipient item as B journaled it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Item {
    id: String,
    task: String,
    content: String,
    in_reply_to: Option<String>,
    capability_hash: Option<String>,
    matched: bool,
}

fn items(rows: &[(u64, RoomEvent)]) -> Vec<Item> {
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

async fn item_named(journal: &Journal, content: &str) -> Item {
    items(&room_events(journal).await)
        .into_iter()
        .rev()
        .find(|item| item.content == content)
        .unwrap_or_else(|| panic!("an item {content:?}"))
}

/// Send `text` to `peer` through the operator's rail and return B's item.
async fn send_item(
    sender: &mut Sender,
    state: &mut TuiState,
    peer: &str,
    host: &Host,
    text: &str,
) -> Item {
    let before = items(&room_events(&host.journal).await).len();
    sender
        .type_command(state, &format!("/team send {peer} {text}"))
        .await;
    settle_send(sender, state).await;
    let after = items(&room_events(&host.journal).await);
    assert_eq!(
        after.len(),
        before + 1,
        "exactly one item minted for {text:?}"
    );
    after.last().cloned().expect("item")
}

/// The next rendered send row (`team-send-N`) the settle handler wrote.
fn send_row(state: &TuiState, n: usize) -> String {
    state.feedback_blocks[&format!("team-send-{n}")]
        .message
        .clone()
}

/// Typed `/team retract` → real preview → the handler raises the card.
async fn raise_retract_card(sender: &mut Sender, state: &mut TuiState, line: &str) {
    sender.type_command(state, line).await;
    let preview = tokio::time::timeout(SETTLE, async {
        loop {
            match sender.domain_rx.recv().await {
                Some(AppEvent::TeamRetractPreviewReady { preview, .. }) => break preview,
                Some(_) => {}
                None => panic!("the domain channel closed before the preview arrived"),
            }
        }
    })
    .await
    .expect("the preview spawn publishes TeamRetractPreviewReady");
    open_team_retract_card(state, "conv", preview);
}

/// Press `y` exactly as the event loop acts on it; the dispatch's sentence.
async fn press_y(sender: &Sender, state: &mut TuiState) -> Option<String> {
    use rustain::adapters::tui::app::InputAction;
    if handle_input(state, &DomainInputEvent::KeyPress('y')) != InputAction::TeamRetractConfirm {
        return None;
    }
    let pending = resolve_team_retract_card(state, true)?;
    let runtime = sender.app_state.a2a_send.as_ref().expect("egress runtime");
    Some(
        team_retract_dispatch(runtime, &pending.peer, &pending.item_id, pending.task)
            .await
            .message,
    )
}

fn retracted(rows: &[(u64, RoomEvent)], item: &str) -> bool {
    rows.iter().any(|(_, row)| {
        matches!(row, RoomEvent::RecipientItemRetracted { address, .. }
            if address.item().as_str() == item)
    })
}

fn status_queries(rows: &[(u64, RoomEvent)]) -> usize {
    rows.iter()
        .filter(|(_, row)| {
            matches!(row, RoomEvent::AdmissionDeferred { gate, .. }
                if gate.starts_with(rustain::adapters::a2a::transparency::STATUS_QUERY_GATE_PREFIX))
        })
        .count()
}

/// Wait (bounded) for B's operator's removal to be durable.
async fn remove_on_host(operator: &mut tokio::net::UnixStream, host: &Host, item: &str) {
    write_frame(
        operator,
        &ClientFrame::RemoveRecipientItem {
            item_id: item.to_owned(),
        },
    )
    .await
    .expect("remove");
    let deadline = tokio::time::Instant::now() + SETTLE;
    while !room_events(&host.journal).await.iter().any(|(_, row)| {
        matches!(row, RoomEvent::RecipientItemRemoved { address } if address.item().as_str() == item)
    }) {
        assert!(tokio::time::Instant::now() < deadline, "removal never durable");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ── Keystones ───────────────────────────────────────────────────────────────

/// `K04` (`A13`, `P7`) — the ingress through the real `dispatch`: a valid
/// carrier is journaled as its digest only; every malformed carrier (length,
/// alphabet, padding, non-canonical trailing bits, wrong type) is `-32602`
/// with zero rows; an absent or `null` carrier is legacy and adds no field;
/// a malformed reply capability WITHOUT a reference is still `-32602`.
/// `M05` → RED: store the raw value. `M06` → RED: treat a malformed carrier
/// as absent.
#[tokio::test]
async fn k04_the_ingress_stores_only_the_digest_of_a_well_formed_carrier() {
    setup();
    let b = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndAuto).await;
    let capability = stranger("k04-valid");
    let valid = rpc(
        &b.endpoint,
        send_body(
            1,
            "k04-valid",
            serde_json::json!({ "x-rustain-item-capability": capability }),
        ),
    )
    .await;
    assert!(
        valid["result"]["metadata"]["x-rustain-item-id"].is_string(),
        "{valid}"
    );
    let rows = room_events(&b.journal).await;
    let item = items(&rows).pop().expect("minted");
    assert_eq!(item.capability_hash, Some(stored_hash(&capability)));
    let line = serde_json::to_string(&rows.last().expect("row").1).expect("row");
    assert!(!line.contains(&capability), "{line}");

    let mut noncanonical = capability.clone();
    noncanonical.pop();
    noncanonical.push('B');
    let length = rows.len();
    for (index, bad) in [
        serde_json::Value::from(&capability[..42]),
        serde_json::Value::from(format!("{capability}A")),
        serde_json::Value::from(format!("{}+", &capability[..42])),
        serde_json::Value::from(format!("{}/", &capability[..42])),
        serde_json::Value::from(format!("{}=", &capability[..42])),
        serde_json::Value::from(noncanonical),
        serde_json::json!(7),
        serde_json::json!({ "value": capability }),
    ]
    .into_iter()
    .enumerate()
    {
        let answer = rpc(
            &b.endpoint,
            send_body(
                2,
                &format!("k04-bad-{index}"),
                serde_json::json!({ "x-rustain-item-capability": bad }),
            ),
        )
        .await;
        assert_eq!(answer["error"]["code"], -32602, "{bad}: {answer}");
    }
    let answer = rpc(
        &b.endpoint,
        send_body(
            3,
            "k04-bad-reply",
            serde_json::json!({ "x-rustain-reply-capability": "not-a-capability" }),
        ),
    )
    .await;
    assert_eq!(answer["error"]["code"], -32602, "{answer}");
    assert_eq!(room_events(&b.journal).await.len(), length, "zero rows");

    for (id, metadata) in [
        ("k04-absent", serde_json::json!({})),
        (
            "k04-null",
            serde_json::json!({ "x-rustain-item-capability": null }),
        ),
    ] {
        rpc(&b.endpoint, send_body(4, id, metadata)).await;
        let rows = room_events(&b.journal).await;
        let row = rows
            .iter()
            .rev()
            .find(|(_, row)| matches!(row, RoomEvent::RecipientItemReceived { task, .. } if task == id))
            .expect("minted");
        let line = serde_json::to_string(&row.1).expect("row");
        assert!(!line.contains("capability"), "{id} is legacy: {line}");
    }
}

/// `K05` — the headline. On a K21-style TLS host B with ONE API key `k`
/// (`threadReads` off, `Allow` + `NotifyAndWait`, operator attached), sender
/// A reads its own thread with thread access off; a co-holder presenting `k`
/// cannot read it, follow up on it, poll or cancel its task, or retract it —
/// and every refusal is byte-identical to an id that never existed.
///
/// `M07` (unkeyed) → step 3's third probe served. `M08` (distinct miss
/// answer) → the byte equalities fail. `M09` (pre-lookup gate kept for a
/// presented capability) → step 2 renders the access-off line. `M10`
/// (`tasks/*` ungated) → step 9 serves "secret answer". `M11` (check after
/// `retract_outcome`) → step 6's tombstone probe answers `-32041`. `M23`
/// (skip the reply-capability check) → step 5 mints. `M24` (capability in
/// the content hash) → step 2's `you` turns `another sender`.
#[tokio::test]
async fn k05_only_the_sender_can_read_answer_poll_cancel_or_retract_its_item() {
    setup();
    let mut b = tls_host(
        false,
        A2aAdmissionPolicy::Allow,
        ResponseMode::NotifyAndWait,
        &[KEY],
        false,
    )
    .await;
    let scoped = tls_host(
        true,
        A2aAdmissionPolicy::Allow,
        ResponseMode::NotifyAndWait,
        &[KEY],
        false,
    )
    .await;
    let (mut operator, attach_shutdown) = attach_operator(&mut b.host).await;
    let mut a = new_sender(vec![
        credentialed(&b, "b", A_ENV),
        credentialed(&scoped, "b2", A_ENV),
    ])
    .await;
    let key_a = sender_key(&a);
    let attacker = raw(&b, KEY);
    let mut state = TuiState::new(120, 40);

    // 1. A sends; its operator discovers the id from the board (19.18 `P8`).
    let sent = send_item(&mut a, &mut state, "b", &b.host, "question").await;
    assert_eq!(send_row(&state, 1), "  ● b  delivered");
    assert_eq!(
        sent.capability_hash.as_deref(),
        Some(stored_hash(&wire(&key_a, &sent.task)).as_str()),
        "B stored the digest of A's derivation for this task"
    );
    let board = read_board(&mut a, &mut state, "b").await;
    let ids = board_item_ids(&board);
    assert_eq!(ids, vec![sent.id.clone()], "{board}");
    let ri_a = ids[0].clone();
    let node_a = waiting_node(&b.host, &sent.task).await;
    pending_card(&b.host, &node_a).await;
    let rr_1 = frame_turn(&operator_reply(&mut operator, &ri_a, "the answer").await);

    // 2. The flip: A reads its own thread with thread access OFF.
    let block = read_thread_block(&mut a, &mut state, "b", &ri_a).await;
    assert!(
        block.starts_with(&format!(
            "thread · b · {ri_a}\n  you       {ri_a}\n      question\n  b         {rr_1}\n      the answer"
        )),
        "{block}"
    );

    // 3. The co-holder, on the same principal.
    let listed = attacker
        .call(rpc_body(
            2,
            rustain::adapters::a2a::ITEMS_LIST_METHOD,
            serde_json::json!({}),
        ))
        .await;
    assert_eq!(listed["result"]["items"][0]["task"], sent.task.as_str());
    let bare = attacker.call(thread_body(7, &ri_a, None)).await;
    assert_eq!(bare["error"]["code"], -32040, "{bare}");
    let never = |capability: &str| thread_body(7, "ri_never", Some(capability));
    for guess in [stranger(&sent.task), unkeyed(&sent.task)] {
        let probe = attacker.text(thread_body(7, &ri_a, Some(&guess))).await;
        assert_eq!(probe, attacker.text(never(&guess)).await);
        assert!(probe.contains("-32001"), "{probe}");
    }

    // 4. A `credential-scoped` host still refuses a capability-less read of a
    // capability-bearing item — with the never-existed bytes.
    let on_scoped = send_item(&mut a, &mut state, "b2", &scoped.host, "scoped").await;
    let scoped_attacker = raw(&scoped, KEY);
    let probe = scoped_attacker
        .text(thread_body(7, &on_scoped.id, None))
        .await;
    assert_eq!(
        probe,
        scoped_attacker.text(thread_body(7, "ri_never", None)).await
    );
    assert!(probe.contains("-32001"), "{probe}");

    // 5. A follow-up with a wrong reply capability mints nothing.
    let length = room_events(&b.host.journal).await.len();
    let follow = |reference: &str| {
        send_body(
            8,
            "attacker-follow",
            serde_json::json!({
                "x-rustain-item-capability": stranger("attacker-follow"),
                "x-rustain-in-reply-to": reference,
                "x-rustain-reply-capability": stranger(&sent.task),
            }),
        )
    };
    let probe = attacker.text(follow(&ri_a)).await;
    assert_eq!(probe, attacker.text(follow("ri_never")).await);
    assert!(probe.contains("-32001"), "{probe}");
    assert_eq!(
        room_events(&b.host.journal).await.len(),
        length,
        "zero rows"
    );

    // 6. A wrong-capability retract: never-existed bytes; nothing marked.
    let wrong = stranger(&sent.task);
    let probe = attacker.text(retract_body(9, &ri_a, Some(&wrong))).await;
    assert_eq!(
        probe,
        attacker
            .text(retract_body(9, "ri_never", Some(&wrong)))
            .await
    );
    assert!(!retracted(&room_events(&b.host.journal).await, &ri_a));
    // … even on a tombstone: the check runs before the tombstone class.
    let third = send_item(&mut a, &mut state, "b", &b.host, "third").await;
    raise_retract_card(&mut a, &mut state, &format!("/team retract b {}", third.id)).await;
    assert!(state.pending_team_retract.as_ref().expect("card").armed);
    remove_on_host(&mut operator, &b.host, &third.id).await;
    let wrong_third = stranger(&third.task);
    let probe = attacker
        .text(retract_body(9, &third.id, Some(&wrong_third)))
        .await;
    assert_eq!(
        probe,
        attacker
            .text(retract_body(9, "ri_never", Some(&wrong_third)))
            .await,
        "never `-32041` for a caller without the capability"
    );
    // Positive control: A's matched capability reaches the tombstone class.
    assert_eq!(
        press_y(&a, &mut state).await.expect("dispatched"),
        "b's host already removed that item — its content is no longer shown there.\n\
         Nothing was marked."
    );

    // 7. A retracts its own item: landed, the mark is on B's item.
    raise_retract_card(&mut a, &mut state, &format!("/team retract b {ri_a}")).await;
    let landed = press_y(&a, &mut state).await.expect("dispatched");
    assert!(landed.starts_with("Retracted on b's host."), "{landed}");
    assert!(retracted(&room_events(&b.host.journal).await, &ri_a));
    // 7b. … and a wrong capability still reads as never-existed, never
    // `alreadyRetracted` (which would leak A's retract timestamp).
    let probe = attacker.text(retract_body(9, &ri_a, Some(&wrong))).await;
    assert_eq!(
        probe,
        attacker
            .text(retract_body(9, "ri_never", Some(&wrong)))
            .await
    );

    // 8. Positive control: loopback is served without any capability.
    let loopback = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndAuto).await;
    let mut local = new_sender(vec![loopback.peer("l")]).await;
    let local_item = send_item(&mut local, &mut state, "l", &loopback, "local").await;
    assert_eq!(
        served_ids(&thread_rpc(&loopback, &local_item.id).await),
        vec![local_item.id.clone()]
    );

    // 9. `tasks/get`: the answer channel is closed to the co-holder.
    let second = send_item(&mut a, &mut state, "b", &b.host, "second").await;
    let node_2 = waiting_node(&b.host, &second.task).await;
    pending_card(&b.host, &node_2).await;
    operator_reply(&mut operator, &second.id, "secret answer").await;
    let queries = status_queries(&room_events(&b.host.journal).await);
    let never_get = attacker
        .text(task_body(11, "tasks/get", "rustain-never", None))
        .await;
    assert!(never_get.contains("-32001"), "{never_get}");
    for capability in [None, Some(stranger(&second.task))] {
        let probe = attacker
            .text(task_body(
                11,
                "tasks/get",
                &second.task,
                capability.as_deref(),
            ))
            .await;
        assert_eq!(probe, never_get);
        assert!(!probe.contains("secret answer"));
    }
    assert_eq!(
        status_queries(&room_events(&b.host.journal).await),
        queries,
        "no StatusQueried row on a miss"
    );
    // Positive control: the capability is served (A's own poll).
    let own = attacker
        .call(task_body(
            11,
            "tasks/get",
            &second.task,
            Some(&wire(&key_a, &second.task)),
        ))
        .await;
    assert_eq!(
        own["result"]["status"]["message"]["parts"][0]["text"],
        "secret answer"
    );

    // 10. `tasks/cancel`: the write channel is closed too.
    let parked = send_item(&mut a, &mut state, "b", &b.host, "parked").await;
    waiting_node(&b.host, &parked.task).await;
    let never_cancel = attacker
        .text(task_body(12, "tasks/cancel", "rustain-never", None))
        .await;
    for capability in [None, Some(stranger(&parked.task))] {
        assert_eq!(
            attacker
                .text(task_body(
                    12,
                    "tasks/cancel",
                    &parked.task,
                    capability.as_deref()
                ))
                .await,
            never_cancel
        );
    }
    assert_eq!(
        poll_state_with(&attacker, &parked.task, &key_a).await["result"]["status"]["state"],
        "auth-required",
        "B's task stays auth-required"
    );

    // 11. A's own follow-up: delivered, matched, and in A's thread.
    a.type_command(&mut state, &format!("/team followup b {ri_a} and retries?"))
        .await;
    settle_send(&mut a, &mut state).await;
    let follow_up = item_named(&b.host.journal, "and retries?").await;
    assert_eq!(follow_up.in_reply_to.as_deref(), Some(ri_a.as_str()));
    assert!(follow_up.matched, "{follow_up:?}");
    let block = read_thread_block(&mut a, &mut state, "b", &ri_a).await;
    assert!(
        block.contains(&format!(
            "\n  you       {}\n      and retries?",
            follow_up.id
        )),
        "{block}"
    );
    attach_shutdown.cancel();
}

/// A's own `tasks/get` with its capability (the model rail's poll shape).
async fn poll_state_with(raw: &Raw, task: &str, key: &ItemCapabilityKey) -> serde_json::Value {
    raw.call(task_body(13, "tasks/get", task, Some(&wire(key, task))))
        .await
}

impl Host {
    /// Stop serving (the process died); the journal stays.
    async fn stop(self) -> Journal {
        self.cancel.cancel();
        self.http.await.expect("server task").expect("shutdown");
        self.journal
    }
}

impl TlsPeer {
    async fn moved(self) -> Self {
        let Self {
            host,
            dir,
            cert_path,
        } = self;
        Self {
            host: host.restart_on_new_port().await,
            dir,
            cert_path,
        }
    }
}

/// `K06` (`AC7`) — sender C holds the SAME API key but its OWN key dir: it
/// cannot read, follow up on or retract A's item, and each refusal is a
/// ratified 19.17/19.18/19.16f row; B writes nothing. Plus A's follow-up to a
/// stopped host: one preflight row, zero dispatch rows (`AC7(d)`, `P17`).
#[tokio::test]
async fn k06_a_co_holding_sender_cannot_act_on_another_senders_item() {
    setup();
    for credential_scoped in [false, true] {
        let b = tls_host(
            credential_scoped,
            A2aAdmissionPolicy::Allow,
            ResponseMode::NotifyAndWait,
            &[KEY],
            false,
        )
        .await;
        let mut a = new_sender(vec![credentialed(&b, "b", A_ENV)]).await;
        let mut c = new_sender(vec![credentialed(&b, "b", C_ENV)]).await;
        let (mut state_a, mut state_c) = (TuiState::new(120, 40), TuiState::new(120, 40));
        let sent = send_item(&mut a, &mut state_a, "b", &b.host, "question").await;
        let length = room_events(&b.host.journal).await.len();

        let block = read_thread_block(&mut c, &mut state_c, "b", &sent.id).await;
        assert_eq!(
            block,
            if credential_scoped {
                "thread not found on b's host"
            } else {
                "thread access is off on b's host — its operator can enable it"
            }
        );
        c.type_command(&mut state_c, &format!("/team followup b {} hi", sent.id))
            .await;
        settle_send(&mut c, &mut state_c).await;
        assert_eq!(
            send_row(&state_c, 1),
            if credential_scoped {
                "    b  no usable answer — '/team board' shows whether it arrived"
            } else {
                "  ✗ b  declined"
            }
        );
        raise_retract_card(
            &mut c,
            &mut state_c,
            &format!("/team retract b {}", sent.id),
        )
        .await;
        assert_eq!(
            press_y(&c, &mut state_c).await.expect("dispatched"),
            format!(
                "b's host has no item {} this host can address. Nothing was marked.",
                sent.id
            )
        );
        assert_eq!(
            room_events(&b.host.journal).await.len(),
            length,
            "B wrote nothing for C (threadReads scoped: {credential_scoped})"
        );

        if !credential_scoped {
            // `AC7(d)` — B is gone: the follow-up's list read fails, so the
            // follow-up is a preflight refusal and nothing is dispatched.
            b.host.stop().await;
            let rows = room_events(&a.journal).await;
            let dispatches = |rows: &[(u64, RoomEvent)]| {
                rows.iter()
                    .filter(|(_, row)| matches!(row, RoomEvent::RemoteEnvelopeDispatched { .. }))
                    .count()
            };
            a.type_command(&mut state_a, &format!("/team followup b {} x", sent.id))
                .await;
            settle_send(&mut a, &mut state_a).await;
            assert!(
                send_row(&state_a, 2)
                    .starts_with("  ⚠ b  unreachable\n    peer `b` has no usable A2A endpoint: "),
                "{}",
                send_row(&state_a, 2)
            );
            let after = room_events(&a.journal).await;
            assert_eq!(dispatches(&after), dispatches(&rows), "zero dispatch rows");
            assert_eq!(
                after[rows.len()..]
                    .iter()
                    .filter(|(_, row)| matches!(
                        row,
                        RoomEvent::RemoteEnvelopeRejected { task: None, .. }
                    ))
                    .count(),
                1,
                "one preflight-refusal row"
            );
        }
    }
}

/// `K06c` (`P11`, `A9`) — a hostile host replays another peer's task: a
/// co-holder on B mints `ri_evil` for C's task with A's capability for it
/// (as C's host received it). A's `/team thread b ri_evil` presents nothing:
/// `(b, m_c)` is not A's dispatch to B. Positive control: C's own item on C
/// is served. `M12` → RED: derive for any task the peer serves.
#[tokio::test]
async fn k06c_a_replayed_foreign_task_earns_no_capability() {
    setup();
    let b = tls_host(
        false,
        A2aAdmissionPolicy::Allow,
        ResponseMode::NotifyAndAuto,
        &[KEY],
        false,
    )
    .await;
    let c = tls_host(
        false,
        A2aAdmissionPolicy::Allow,
        ResponseMode::NotifyAndAuto,
        &[KEY],
        false,
    )
    .await;
    let mut a = new_sender(vec![
        credentialed(&b, "b", A_ENV),
        credentialed(&c, "c", A_ENV),
    ])
    .await;
    let mut state = TuiState::new(120, 40);
    let on_c = send_item(&mut a, &mut state, "c", &c.host, "for c").await;
    let evil = raw(&b, KEY)
        .call(send_body(
            1,
            &on_c.task,
            serde_json::json!({
                "x-rustain-item-capability": wire(&sender_key(&a), &on_c.task),
            }),
        ))
        .await;
    let ri_evil = evil["result"]["metadata"]["x-rustain-item-id"]
        .as_str()
        .unwrap_or_else(|| panic!("the replay mints on B: {evil}"))
        .to_owned();
    assert_eq!(
        read_thread_block(&mut a, &mut state, "b", &ri_evil).await,
        "thread access is off on b's host — its operator can enable it"
    );
    let own = read_thread_block(&mut a, &mut state, "c", &on_c.id).await;
    assert!(
        own.starts_with(&format!(
            "thread · c · {}\n  you       {}\n      for c",
            on_c.id, on_c.id
        )),
        "{own}"
    );
}

/// `K06d` (`PQ2` = B widened) — an UNPINNED alias survives a rename at the
/// same URL (the endpoint arm) and a move under the same alias (the
/// pseudonym arm), but not both: then no capability is sent and B's
/// pre-lookup gate answers the access-off line. `M33` → RED on (i), `M34` →
/// RED on (ii).
#[tokio::test]
async fn k06d_an_unpinned_peer_survives_a_rename_or_a_move_but_not_both() {
    setup();
    let b = tls_host(
        false,
        A2aAdmissionPolicy::Allow,
        ResponseMode::NotifyAndAuto,
        &[KEY],
        false,
    )
    .await;
    let mut state = TuiState::new(120, 40);
    let mut a = new_sender(vec![credentialed(&b, "b", A_ENV)]).await;
    let first = send_item(&mut a, &mut state, "b", &b.host, "first").await;
    // Story 19.19 K08: capability access and the rendered own-content label
    // must agree after a rename or same-alias move, retaining task/hash checks.
    let served = |alias: &str, block: &str| {
        assert!(
            block.starts_with(&format!("thread · {alias} · {}\n  ", first.id))
                && block.contains(&format!("  {}\n      first", first.id)),
            "{block}"
        );
        let label = block.lines().find_map(|line| {
            line.strip_prefix("  ")
                .and_then(|row| row.strip_suffix(first.id.as_str()))
                .map(str::trim)
        });
        assert_eq!(
            label,
            Some("you"),
            "own dispatched bytes lost their label: {block}"
        );
    };
    served(
        "b",
        &read_thread_block(&mut a, &mut state, "b", &first.id).await,
    );

    // (i) rename `b` → `b2`, same URL.
    let Sender { journal, keys, .. } = a;
    let mut a = sender_over(vec![credentialed(&b, "b2", A_ENV)], journal, keys).await;
    served(
        "b2",
        &read_thread_block(&mut a, &mut state, "b2", &first.id).await,
    );

    // Story 19.19 K08: adding a pin keeps the historical endpoint-bearing
    // row correlated; it does not retroactively replace its alias pseudonym.
    let pin = rustain::domain::models::PinnedKey::new(
        rustain::domain::models::PinnedKeyAlgorithm::EdDsa,
        URL_SAFE_NO_PAD.encode(
            IdentityKeyStore::new(b.host.keys.path())
                .load_or_generate()
                .unwrap()
                .identity()
                .public_key
                .as_slice(),
        ),
        None,
    );
    let Sender { journal, keys, .. } = a;
    let mut a = sender_over(
        vec![credentialed(&b, "b", A_ENV).with_pinned_key(Some(pin.clone()))],
        journal,
        keys,
    )
    .await;
    served(
        "b",
        &read_thread_block(&mut a, &mut state, "b", &first.id).await,
    );

    // (ii) B moves to a new port; alias `b` at the new URL.
    let b = b.moved().await;
    let Sender { journal, keys, .. } = a;
    let mut a = sender_over(vec![credentialed(&b, "b", A_ENV)], journal, keys).await;
    served(
        "b",
        &read_thread_block(&mut a, &mut state, "b", &first.id).await,
    );

    // The approved PQ2 alias arm also survives a move AND pin-add together.
    // This labels this host's sent bytes, not continuity of host/personhood.
    let Sender { journal, keys, .. } = a;
    let mut a = sender_over(
        vec![credentialed(&b, "b", A_ENV).with_pinned_key(Some(pin))],
        journal,
        keys,
    )
    .await;
    served(
        "b",
        &read_thread_block(&mut a, &mut state, "b", &first.id).await,
    );

    // (iii) renamed AND moved: no capability is sent — B's pre-lookup gate,
    // which only a capability-less request meets, answers.
    let Sender { journal, keys, .. } = a;
    let mut a = sender_over(vec![credentialed(&b, "b2", A_ENV)], journal, keys).await;
    assert_eq!(
        read_thread_block(&mut a, &mut state, "b2", &first.id).await,
        "thread access is off on b2's host — its operator can enable it"
    );
}
/// A reply-turn id requires the observed answer as well as an own dispatch.
/// A renamed peer keeps the binding; a sibling sender whose ledger was seeded
/// earlier reloads the journal before resolving the reply.
#[tokio::test]
async fn reply_turn_capability_survives_alias_rename_and_sibling_observation() {
    setup();
    let mut b = tls_host(
        false,
        A2aAdmissionPolicy::Allow,
        ResponseMode::NotifyAndWait,
        &[KEY],
        false,
    )
    .await;
    let (mut operator, attach_shutdown) = attach_operator(&mut b.host).await;
    let mut a = new_sender(vec![credentialed(&b, "b", A_ENV)]).await;
    let mut state = TuiState::new(120, 40);
    let sent = send_item(&mut a, &mut state, "b", &b.host, "question").await;
    pending_card(&b.host, &waiting_node(&b.host, &sent.task).await).await;
    let mut sibling = sender_over(
        vec![credentialed(&b, "b", A_ENV)],
        a.journal.clone(),
        a.keys.clone(),
    )
    .await;
    // Seed the sibling's ledger before the other sender observes the answer.
    read_thread_block(&mut sibling, &mut state, "b", &sent.id).await;
    let rr = frame_turn(&operator_reply(&mut operator, &sent.id, "answer").await);
    read_thread_block(&mut a, &mut state, "b", &sent.id).await;

    sibling
        .type_command(&mut state, &format!("/team followup b {rr} sibling turn"))
        .await;
    settle_send(&mut sibling, &mut state).await;
    assert!(item_named(&b.host.journal, "sibling turn").await.matched);

    let Sender { journal, keys, .. } = a;
    let mut renamed = sender_over(vec![credentialed(&b, "b2", A_ENV)], journal, keys).await;
    renamed
        .type_command(&mut state, &format!("/team followup b2 {rr} renamed turn"))
        .await;
    settle_send(&mut renamed, &mut state).await;
    assert!(item_named(&b.host.journal, "renamed turn").await.matched);
    attach_shutdown.cancel();
}

/// `K07` (`P13`) — a capability never substitutes for the principal. B has
/// two keys, `k` and `credential-b` (distinct principals). A mints for real
/// under `credential-b` with carrier `c`: `credential-b` + `c` is served;
/// `k` + `c` answers the never-existed bytes. `M13` → RED: look the id up
/// across principals and serve on a capability match.
#[tokio::test]
async fn k07_a_capability_never_substitutes_for_the_principal() {
    setup();
    let b = tls_host(
        false,
        A2aAdmissionPolicy::Allow,
        ResponseMode::NotifyAndAuto,
        &[KEY, CREDENTIAL_B],
        false,
    )
    .await;
    let mut a = new_sender(vec![credentialed(&b, "b", A_CREDENTIAL_B_ENV)]).await;
    let mut state = TuiState::new(120, 40);
    let sent = send_item(&mut a, &mut state, "b", &b.host, "question").await;
    let capability = wire(&sender_key(&a), &sent.task);
    let owner = raw(&b, CREDENTIAL_B)
        .call(thread_body(3, &sent.id, Some(&capability)))
        .await;
    assert_eq!(served_ids(&owner), vec![sent.id.clone()], "{owner}");
    let other = raw(&b, KEY);
    let probe = other
        .text(thread_body(3, &sent.id, Some(&capability)))
        .await;
    assert_eq!(
        probe,
        other
            .text(thread_body(3, "ri_never", Some(&capability)))
            .await
    );
    assert!(probe.contains("-32001"), "{probe}");
}

/// `K08` — legacy items are never grandfathered open, and an undecodable
/// stored hash fails closed. `M14` → RED: serve a legacy item under `off`
/// when any capability is presented. `M15` → RED: read an undecodable hash
/// as legacy.
#[tokio::test]
async fn k08_legacy_items_keep_their_gate_and_a_broken_hash_fails_closed() {
    setup();
    for credential_scoped in [false, true] {
        let b = tls_host(
            credential_scoped,
            A2aAdmissionPolicy::Allow,
            ResponseMode::NotifyAndAuto,
            &[KEY],
            false,
        )
        .await;
        let raw = raw(&b, KEY);
        let minted = raw
            .call(send_body(1, "legacy-root", serde_json::json!({})))
            .await;
        let ri_l = minted["result"]["metadata"]["x-rustain-item-id"]
            .as_str()
            .unwrap_or_else(|| panic!("a legacy item mints: {minted}"))
            .to_owned();
        assert_eq!(
            item_named(&b.host.journal, "text").await.capability_hash,
            None,
            "no carrier ⇒ legacy"
        );
        let any = stranger("legacy-root");
        let answers = [
            raw.call(thread_body(2, &ri_l, None)).await,
            raw.call(thread_body(2, &ri_l, Some(&any))).await,
            raw.call(send_body(
                3,
                "legacy-follow",
                serde_json::json!({ "x-rustain-in-reply-to": ri_l }),
            ))
            .await,
        ];
        for answer in &answers {
            if credential_scoped {
                assert!(answer.get("result").is_some(), "served: {answer}");
            } else {
                assert_eq!(answer["error"]["code"], -32040, "{answer}");
            }
        }
        let retract = raw.call(retract_body(4, &ri_l, None)).await;
        assert_eq!(
            retract["result"]["alreadyRetracted"], false,
            "a legacy retract stays credential-scoped (disclosed): {retract}"
        );
    }

    // Fail closed, and its positive control. These two rows are labelled
    // SEEDED: another build's ingress under this API-key principal (the 19.18
    // `A6` rule) — ⛔ never a row a keystone claims was sent.
    let b = tls_host(
        true,
        A2aAdmissionPolicy::Allow,
        ResponseMode::NotifyAndAuto,
        &[KEY],
        false,
    )
    .await;
    let principal =
        rustain::adapters::a2a::exec::SubmitterKey::from_api_key(KEY).pseudonymous_peer_id();
    let capability = stranger("seeded");
    for (id, hash) in [
        ("ri_zz", "zz".to_owned()),
        ("ri_valid", stored_hash(&capability)),
    ] {
        b.host
            .journal
            .room
            .record_event(RoomEvent::RecipientItemReceived {
                message_type: rustain::domain::models::SemanticMessageType::Unknown,
                address: rustain::domain::models::ItemAddress::from_a2a_ingress(
                    principal.clone(),
                    rustain::domain::models::ItemId::from_replay(id),
                ),
                task: format!("seeded-{id}"),
                alias: None,
                content: "seeded".to_owned(),
                in_reply_to: None,
                capability_hash: Some(hash),
                reply_capability_matched: false,
            })
            .await
            .expect("seed another build's ingress");
    }
    let raw = raw(&b, KEY);
    for presented in [None, Some(capability.as_str())] {
        let probe = raw.text(thread_body(5, "ri_zz", presented)).await;
        assert_eq!(probe, raw.text(thread_body(5, "ri_never", presented)).await);
        assert!(probe.contains("-32001"), "{probe}");
    }
    let served = raw
        .call(thread_body(5, "ri_valid", Some(&capability)))
        .await;
    assert_eq!(served_ids(&served), vec!["ri_valid".to_owned()], "{served}");
}

/// `K09` (`A15`) — on a loopback host, a raw follow-up that carries its own
/// item capability but NO reply capability is accepted (loopback unchanged)
/// and is its own root; A's matched follow-up of the reply turn it observed
/// joins, and the thread shows four turns — the keyed twin of 19.18 `K07`.
/// `M16` → RED: count every `in_reply_to` edge.
#[tokio::test]
async fn k09_an_unmatched_link_does_not_join_a_capability_bearing_thread() {
    setup();
    let mut b = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let (mut operator, attach_shutdown) = attach_operator(&mut b).await;
    let mut a = new_sender(vec![b.peer("b")]).await;
    let mut state = TuiState::new(120, 40);
    let q = send_item(&mut a, &mut state, "b", &b, "question").await;
    pending_card(&b, &waiting_node(&b, &q.task).await).await;
    let rr_1 = frame_turn(&operator_reply(&mut operator, &q.id, "answer").await);
    read_thread_block(&mut a, &mut state, "b", &q.id).await;

    let rogue = rpc(
        &b.endpoint,
        send_body(
            1,
            "rogue",
            serde_json::json!({
                "x-rustain-item-capability": stranger("rogue"),
                "x-rustain-in-reply-to": q.id,
            }),
        ),
    )
    .await;
    let rogue_id = rogue["result"]["metadata"]["x-rustain-item-id"]
        .as_str()
        .unwrap_or_else(|| panic!("loopback accepts it: {rogue}"))
        .to_owned();
    let rogue_item = items(&room_events(&b.journal).await)
        .into_iter()
        .find(|item| item.id == rogue_id)
        .expect("rogue item");
    assert!(rogue_item.capability_hash.is_some() && !rogue_item.matched);

    a.type_command(&mut state, &format!("/team followup b {rr_1} and retries?"))
        .await;
    settle_send(&mut a, &mut state).await;
    let follow = item_named(&b.journal, "and retries?").await;
    assert!(follow.matched, "{follow:?}");
    pending_card(&b, &waiting_node(&b, &follow.task).await).await;
    let rr_2 = frame_turn(&operator_reply(&mut operator, &follow.id, "backoff").await);

    let block = read_thread_block(&mut a, &mut state, "b", &q.id).await;
    assert_eq!(
        block,
        format!(
            "thread · b · {q}\n  you       {q}\n      question\n  b         {rr_1}\n      answer\n  you       {f}\n      and retries?\n  b         {rr_2}\n      backoff\n{}",
            rustain::adapters::a2a::board::COLLAPSED_PRINCIPAL_DISCLOSURE,
            q = q.id,
            f = follow.id,
        )
    );
    assert_eq!(
        served_ids(&thread_rpc(&b, &rogue_id).await),
        vec![rogue_id.clone()],
        "the unmatched item is its own root"
    );
    attach_shutdown.cancel();
}

/// `K09b` (`A15`) — a legacy root never serves a capability-bearing
/// descendant: A's follow-up to the co-holder's legacy item (accepted under
/// `credential-scoped`) and B's reply to it stay out of the legacy thread
/// every co-holder can read. `M25` → RED: keep that edge.
#[tokio::test]
async fn k09b_a_legacy_root_never_serves_a_capability_bearing_descendant() {
    setup();
    let mut b = tls_host(
        true,
        A2aAdmissionPolicy::Allow,
        ResponseMode::NotifyAndWait,
        &[KEY],
        false,
    )
    .await;
    let (mut operator, attach_shutdown) = attach_operator(&mut b.host).await;
    let attacker = raw(&b, KEY);
    let legacy = attacker
        .call(send_body(1, "legacy-root", serde_json::json!({})))
        .await;
    let ri_l = legacy["result"]["metadata"]["x-rustain-item-id"]
        .as_str()
        .unwrap_or_else(|| panic!("{legacy}"))
        .to_owned();
    let mut a = new_sender(vec![credentialed(&b, "b", A_ENV)]).await;
    let mut state = TuiState::new(120, 40);
    a.type_command(&mut state, &format!("/team followup b {ri_l} secret"))
        .await;
    settle_send(&mut a, &mut state).await;
    let follow = item_named(&b.host.journal, "secret").await;
    assert_eq!(follow.in_reply_to.as_deref(), Some(ri_l.as_str()));
    assert!(follow.capability_hash.is_some() && !follow.matched);
    pending_card(&b.host, &waiting_node(&b.host, &follow.task).await).await;
    operator_reply(&mut operator, &follow.id, "secret reply").await;

    let served = attacker.text(thread_body(2, &ri_l, None)).await;
    assert!(
        served.contains(&ri_l),
        "the legacy thread is served: {served}"
    );
    assert!(!served.contains("secret"), "{served}");
    let probe = attacker.text(thread_body(2, &follow.id, None)).await;
    assert_eq!(
        probe,
        attacker.text(thread_body(2, "ri_never", None)).await,
        "A's follow-up is its own capability-bearing root"
    );
    attach_shutdown.cancel();
}

/// Grant the next forwarded approval (admission or sender consent) through
/// the real frame (19.18's `approve_next`).
async fn approve_next(operator: &mut tokio::net::UnixStream) {
    let request_id = loop {
        if let DaemonFrame::ApprovalRequest { request_id, .. } =
            tokio::time::timeout(SETTLE, read_frame::<_, DaemonFrame>(operator))
                .await
                .expect("approval request")
                .expect("frame")
                .expect("open")
        {
            break request_id;
        }
    };
    write_frame(
        operator,
        &ClientFrame::ApprovalResponse {
            request_id,
            outcome: rustain::domain::models::ApprovalOutcome::Once,
        },
    )
    .await
    .expect("grant");
}

/// Wait (bounded) until B's journal holds an item whose content is `text`.
async fn minted(host: &Host, text: &str) -> Item {
    let deadline = tokio::time::Instant::now() + SETTLE;
    loop {
        if let Some(item) = items(&room_events(&host.journal).await)
            .into_iter()
            .find(|item| item.content == text)
        {
            return item;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{text:?} never minted"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The task of A's most recent dispatch (its own journal).
async fn last_dispatched_task(sender: &Sender) -> String {
    room_events(&sender.journal)
        .await
        .into_iter()
        .rev()
        .find_map(|(_, row)| match row {
            RoomEvent::RemoteEnvelopeDispatched {
                task: Some(task),
                act: rustain::domain::models::DispatchAct::Task,
                ..
            } => Some(task),
            _ => None,
        })
        .expect("a dispatch row")
}

/// `K10` (`A6`, `P16`) — restart. A and B each restart and A's thread is
/// still served, the co-holder still gets the never-existed bytes, and every
/// RESEEDED task keeps its hash: B (Ask + NotifyAndWait, composed with a
/// consent projection as production is) restarts with one of A's tasks
/// `auth-required` (an orphan) and one consent-parked pending approval (an
/// orphan whose hash only the pending file holds); a second, consent-less
/// Ask host restarts with a pending approval (the pending-file reseed); a
/// third host stops mid-draft (an orphan only the item's hash covers).
/// `M17` → RED: a new key per process. `M26` → RED: reseed with no hash at
/// either site.
#[tokio::test]
async fn k10_restarts_keep_every_capability() {
    setup();
    let mut b = tls_host(
        false,
        A2aAdmissionPolicy::Ask,
        ResponseMode::NotifyAndWait,
        &[KEY],
        true,
    )
    .await;
    let mut plain = tls_host(
        false,
        A2aAdmissionPolicy::Ask,
        ResponseMode::NotifyAndWait,
        &[KEY],
        false,
    )
    .await;
    let (mut operator, attach_shutdown) = attach_operator(&mut b.host).await;
    let (mut plain_operator, plain_shutdown) = attach_operator(&mut plain.host).await;
    let mut a = new_sender(vec![
        credentialed(&b, "b", A_ENV),
        credentialed(&plain, "p", A_ENV),
    ])
    .await;
    let key_a = sender_key(&a);
    let mut state = TuiState::new(120, 40);

    // An approved item, parked `auth-required` on its card.
    a.type_command(&mut state, "/team send b answered").await;
    approve_next(&mut operator).await;
    settle_send(&mut a, &mut state).await;
    let first = minted(&b.host, "answered").await;
    pending_card(&b.host, &waiting_node(&b.host, &first.task).await).await;
    // A pending approval, never answered: a pending record + (consent) a
    // durable node before approval.
    a.type_command(&mut state, "/team send b pending").await;
    settle_send(&mut a, &mut state).await;
    let pending_task = last_dispatched_task(&a).await;
    // The consent-less host's pending approval.
    a.type_command(&mut state, "/team send p plain-pending")
        .await;
    settle_send(&mut a, &mut state).await;
    let plain_task = last_dispatched_task(&a).await;
    let _ = (&mut plain_operator, &mut operator);

    // A restarts over the same journal and key dir.
    let Sender { journal, keys, .. } = a;
    let mut a = sender_over(
        vec![
            credentialed(&b, "b", A_ENV),
            credentialed(&plain, "p", A_ENV),
        ],
        journal,
        keys,
    )
    .await;
    assert!(
        read_thread_block(&mut a, &mut state, "b", &first.id)
            .await
            .starts_with(&format!(
                "thread · b · {}\n  you       {}",
                first.id, first.id
            )),
        "A's restarted process derives the same capability"
    );

    // B restarts (19.18's restart: same socket, keys, workspace).
    attach_shutdown.cancel();
    plain_shutdown.cancel();
    drop(operator);
    drop(plain_operator);
    let TlsPeer {
        host,
        dir,
        cert_path,
    } = b;
    let b = TlsPeer {
        host: host.restart().await,
        dir,
        cert_path,
    };
    let TlsPeer {
        host,
        dir,
        cert_path,
    } = plain;
    let plain = TlsPeer {
        host: host.restart().await,
        dir,
        cert_path,
    };
    assert!(
        read_thread_block(&mut a, &mut state, "b", &first.id)
            .await
            .starts_with(&format!(
                "thread · b · {}\n  you       {}",
                first.id, first.id
            ))
    );
    let attacker = raw(&b, KEY);
    let wrong = stranger(&first.task);
    assert_eq!(
        attacker.text(thread_body(4, &first.id, Some(&wrong))).await,
        attacker
            .text(thread_body(4, "ri_never", Some(&wrong)))
            .await
    );

    // Every reseeded task kept its hash.
    for (peer, task) in [
        (&b, first.task.as_str()),
        (&b, pending_task.as_str()),
        (&plain, plain_task.as_str()),
    ] {
        let attacker = raw(peer, KEY);
        let never = attacker
            .text(task_body(5, "tasks/get", "rustain-never", None))
            .await;
        assert_eq!(
            attacker.text(task_body(5, "tasks/get", task, None)).await,
            never,
            "{task}: a reseeded task is still capability-bearing"
        );
        let own = attacker
            .call(task_body(5, "tasks/get", task, Some(&wire(&key_a, task))))
            .await;
        assert_eq!(own["result"]["status"]["state"], "failed", "{task}: {own}");
    }

    // A true orphan: B stops while the task's node is still running (its
    // draft held on the provider). No pending record holds it, so only the
    // orphan pass can reseed it, with the hash from the item (`M26-orphan`).
    let gate = Arc::new(Notify::new());
    let drafting = {
        let mut peer = tls_host(
            false,
            A2aAdmissionPolicy::Allow,
            ResponseMode::NotifyAndDraft,
            &[KEY],
            false,
        )
        .await;
        peer.host.options.provider_gate = Some(gate.clone());
        let TlsPeer {
            host,
            dir,
            cert_path,
        } = peer;
        TlsPeer {
            host: host.restart().await,
            dir,
            cert_path,
        }
    };
    let (egress, _rail_journal, rail_keys) =
        model_rail(vec![credentialed(&drafting, "d", A_ENV)]).await;
    let running = invoke(&egress, "d");
    let orphan = minted(&drafting.host, "delegated").await;
    running.abort();
    let TlsPeer {
        host,
        dir,
        cert_path,
    } = drafting;
    let drafting = TlsPeer {
        host: host.restart().await,
        dir,
        cert_path,
    };
    let rail_key = ItemCapabilityKeyStore::new(rail_keys.path())
        .load()
        .expect("key");
    let attacker = raw(&drafting, KEY);
    let never = attacker
        .text(task_body(6, "tasks/get", "rustain-never", None))
        .await;
    assert_eq!(
        attacker
            .text(task_body(6, "tasks/get", &orphan.task, None))
            .await,
        never,
        "a reseeded orphan is still capability-bearing"
    );
    let own = attacker
        .call(task_body(
            6,
            "tasks/get",
            &orphan.task,
            Some(&wire(&rail_key, &orphan.task)),
        ))
        .await;
    assert_eq!(own["result"]["status"]["state"], "failed", "orphan: {own}");
}

/// A lost pending file cannot prove an orphan was legacy. A co-holder must
/// not gain task access merely because the restart lost the hash source.
#[tokio::test]
async fn missing_pending_file_does_not_restore_an_orphan_as_legacy() {
    setup();
    let mut b = tls_host(
        false,
        A2aAdmissionPolicy::Ask,
        ResponseMode::NotifyAndWait,
        &[KEY],
        true,
    )
    .await;
    let (operator, shutdown) = attach_operator(&mut b.host).await;
    let mut a = new_sender(vec![credentialed(&b, "b", A_ENV)]).await;
    let mut state = TuiState::new(120, 40);
    a.type_command(&mut state, "/team send b pending").await;
    settle_send(&mut a, &mut state).await;
    let task = last_dispatched_task(&a).await;
    let pending_path = b.host.journal.path.join(".rustain/a2a-pending.json");
    assert!(
        std::fs::read_to_string(&pending_path)
            .expect("pending record persisted")
            .contains(&task)
    );
    shutdown.cancel();
    drop(operator);
    std::fs::remove_file(&pending_path).expect("simulate missing pending file");
    let TlsPeer {
        host,
        dir,
        cert_path,
    } = b;
    let b = TlsPeer {
        host: host.restart().await,
        dir,
        cert_path,
    };
    let caller = raw(&b, KEY);
    let missing = caller
        .text(task_body(1, "tasks/get", "rustain-never", None))
        .await;
    assert_eq!(
        caller.text(task_body(1, "tasks/get", &task, None)).await,
        missing,
        "unknown capability class must not be restored as legacy"
    );
}

/// `K11` — fan-out mints one capability per recipient: `/team send b,c,d q`
/// stores three pairwise-distinct hashes, each the digest of A's derivation
/// over THAT recipient's task. `M18` → RED: derive from a per-send constant.
#[tokio::test]
async fn k11_a_fan_out_mints_one_capability_per_recipient() {
    setup();
    let hosts = [
        Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndAuto).await,
        Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndAuto).await,
        Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndAuto).await,
    ];
    let mut a = new_sender(vec![
        hosts[0].peer("b"),
        hosts[1].peer("c"),
        hosts[2].peer("d"),
    ])
    .await;
    let key_a = sender_key(&a);
    let mut state = TuiState::new(120, 40);
    a.type_command(&mut state, "/team send b,c,d q").await;
    for _ in 0..3 {
        settle_send(&mut a, &mut state).await;
    }
    let mut hashes = Vec::new();
    for host in &hosts {
        let item = items(&room_events(&host.journal).await)
            .pop()
            .expect("one item per recipient");
        let hash = item.capability_hash.expect("capability-bearing");
        assert_eq!(hash, stored_hash(&wire(&key_a, &item.task)));
        hashes.push(hash);
    }
    hashes.sort();
    hashes.dedup();
    assert_eq!(hashes.len(), 3, "pairwise distinct");
}

/// Every regular file under `root`, recursively.
fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read dir").flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                files.push(path);
            }
        }
    }
    files
}

/// `K12` (`A13`, `P10`) — a raw capability is never stored, exported or
/// logged: after sends, a thread read, a follow-up, a retract, a capability
/// `tasks/get`, a pending approval and a restart, every file in B's and A's
/// workspaces, both transparency exports and the whole-run `tracing` capture
/// hold ZERO occurrences of any capability A sent (or its decoded hex).
/// Positive controls: the capture holds a library line from a spawned task;
/// the pending file holds the pending task WITH its hash before the scan;
/// each needle's digest IS a stored hash. `M05` → RED: store the raw value.
/// `M30` → RED: `tracing::debug!` the carrier.
#[tokio::test]
async fn k12_no_raw_capability_is_stored_exported_or_logged() {
    setup();
    let mut b = tls_host(
        false,
        A2aAdmissionPolicy::Ask,
        ResponseMode::NotifyAndWait,
        &[KEY],
        false,
    )
    .await;
    let (mut operator, attach_shutdown) = attach_operator(&mut b.host).await;
    let mut a = new_sender(vec![credentialed(&b, "b", A_ENV)]).await;
    let key_a = sender_key(&a);
    let mut state = TuiState::new(120, 40);

    a.type_command(&mut state, "/team send b scanned").await;
    approve_next(&mut operator).await;
    settle_send(&mut a, &mut state).await;
    let first = minted(&b.host, "scanned").await;
    pending_card(&b.host, &waiting_node(&b.host, &first.task).await).await;
    operator_reply(&mut operator, &first.id, "answer").await;
    read_thread_block(&mut a, &mut state, "b", &first.id).await;
    a.type_command(&mut state, &format!("/team followup b {} more", first.id))
        .await;
    approve_next(&mut operator).await;
    settle_send(&mut a, &mut state).await;
    minted(&b.host, "more").await;
    raise_retract_card(&mut a, &mut state, &format!("/team retract b {}", first.id)).await;
    press_y(&a, &mut state).await.expect("retract dispatched");
    raw(&b, KEY)
        .call(task_body(
            6,
            "tasks/get",
            &first.task,
            Some(&wire(&key_a, &first.task)),
        ))
        .await;
    a.type_command(&mut state, "/team send b left-pending")
        .await;
    settle_send(&mut a, &mut state).await;
    let pending_task = last_dispatched_task(&a).await;

    // The pending file holds the pending task and its HASH (asserted before
    // the scan: the file is truncated at reconcile and on terminal moves).
    let pending_path = b
        .host
        .journal
        .path
        .join(".rustain")
        .join("a2a-pending.json");
    let pending: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&pending_path).expect("pending file"))
            .expect("pending json");
    assert!(
        pending.as_array().expect("records").iter().any(|record| {
            record["task_id"] == pending_task.as_str()
                && record["capability_hash"] == stored_hash(&wire(&key_a, &pending_task)).as_str()
        }),
        "{pending}"
    );

    let tasks: Vec<String> = room_events(&a.journal)
        .await
        .into_iter()
        .filter_map(|(_, row)| match row {
            RoomEvent::RemoteEnvelopeDispatched {
                task: Some(task),
                act: rustain::domain::models::DispatchAct::Task,
                ..
            } => Some(task),
            _ => None,
        })
        .collect();
    assert!(tasks.len() >= 3, "{tasks:?}");
    let needles: Vec<(String, String)> = tasks
        .iter()
        .map(|task| {
            let capability = wire(&key_a, task);
            let decoded = hex::encode(URL_SAFE_NO_PAD.decode(&capability).expect("decode"));
            (capability, decoded)
        })
        .collect();
    let stored: Vec<String> = items(&room_events(&b.host.journal).await)
        .into_iter()
        .filter_map(|item| item.capability_hash)
        .collect();
    for (capability, _) in &needles[..2] {
        assert!(
            stored.contains(&stored_hash(capability)),
            "positive control: the needle's digest is what B stored"
        );
    }
    let scan = |haystack: &str, what: &str| {
        for (capability, decoded) in &needles {
            assert!(
                !haystack.contains(capability.as_str()),
                "{what} holds a capability"
            );
            assert!(
                !haystack.contains(decoded.as_str()),
                "{what} holds its bytes"
            );
        }
    };
    scan(
        &String::from_utf8_lossy(&std::fs::read(&pending_path).expect("pending")),
        "a2a-pending.json",
    );

    // Restart B with the approval still pending: the reconcile's library
    // line — logged by the spawned `serve` task — is the capture's positive
    // control.
    attach_shutdown.cancel();
    drop(operator);
    let TlsPeer {
        host,
        dir,
        cert_path,
    } = b;
    let b = TlsPeer {
        host: host.restart().await,
        dir,
        cert_path,
    };
    // `serve` reconciles BEFORE it accepts: one answered request orders the
    // scan after the restart's reconcile.
    raw(&b, KEY)
        .call(rpc_body(
            8,
            rustain::adapters::a2a::ITEMS_LIST_METHOD,
            serde_json::json!({}),
        ))
        .await;
    for root in [&b.host.journal.path, &a.journal.path] {
        for file in files_under(root) {
            scan(
                &String::from_utf8_lossy(&std::fs::read(&file).expect("file")),
                &file.display().to_string(),
            );
        }
    }
    for journal in [&b.host.journal, &a.journal] {
        let rows = rustain::domain::services::transparency::fold_transparency(
            &RoomJournalReader::load_entries(journal.node.as_ref())
                .await
                .expect("journal"),
        );
        scan(
            &rustain::domain::services::transparency::render_export(&rows),
            "the transparency export",
        );
    }
    let log = captured_log();
    assert!(
        log.contains(&format!(
            "A2A approval-pending task failed by host restart task={pending_task}"
        )),
        "positive control: the capture holds this run's reconcile line, logged by the \
         spawned `serve` task"
    );
    scan(&log, "the tracing capture");
}

/// A serve sink that wakes the test on the first `tasks/get` it journals.
struct StatusQueryNotify {
    inner: Arc<dyn RoomJournal>,
    seen: Arc<Notify>,
}

#[async_trait::async_trait]
impl RoomJournal for StatusQueryNotify {
    async fn record_event(&self, event: RoomEvent) -> Result<(), RoomJournalError> {
        let status = matches!(&event, RoomEvent::AdmissionDeferred { gate, .. }
            if gate.starts_with(rustain::adapters::a2a::transparency::STATUS_QUERY_GATE_PREFIX));
        let recorded = self.inner.record_event(event).await;
        if status {
            self.seen.notify_one();
        }
        recorded
    }
}

/// The model rail, composed as production composes it.
async fn model_rail(peers: Vec<A2aPeerSpec>) -> (A2aEgress, Journal, Arc<tempfile::TempDir>) {
    let journal = journal().await;
    let keys = Arc::new(tempfile::tempdir().expect("keys"));
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    let egress = A2aEgress::compose(
        peers,
        NodeTree::new(),
        journal.room.clone(),
        journal.node.clone(),
        event_tx,
        load_item_capability_key(Ok(keys.path().to_path_buf())),
    )
    .expect("compose");
    tokio::time::timeout(SETTLE, egress.await_cards_settled())
        .await
        .expect("cards settle");
    (egress, journal, keys)
}

fn invoke(egress: &A2aEgress, peer: &str) -> tokio::task::JoinHandle<Result<String, String>> {
    use rustain::domain::ports::CapabilityProvider;
    let provider = egress.provider().clone();
    let id = rustain::domain::models::CapabilityId {
        protocol: "a2a".to_owned(),
        server: peer.to_owned(),
        tool: "delegate".to_owned(),
    };
    tokio::spawn(async move {
        provider
            .invoke(
                &id,
                serde_json::json!({ "message": "delegated" }),
                CancellationToken::new(),
            )
            .await
            .map(|result| result.content)
            .map_err(|error| error.to_string())
    })
}

/// `K13` (`A9`, `A18`, `P12`) — the model rail against a 19.18c API-key host.
/// (i) Poll: B (`NotifyAndDraft`) holds its provider until the test has
/// observed the first `tasks/get` journaled — which only a SERVED poll is —
/// and the delegation then runs to the peer's decision point; B's item is
/// capability-bearing. (ii) Cancel, mandatory: an `Ask` host with an attached
/// approver answers `auth-required`, `step` cancels at once, the
/// capability-bearing `tasks/cancel` is served and B's task ends `canceled`.
/// `M20` → RED: no binding (the poll answers not-found). `M20b` → RED: bind
/// `tasks_get` only (the cancel answers not-found; B stays `auth-required`).
#[tokio::test]
async fn k13_the_model_rail_polls_and_cancels_with_its_own_capability() {
    setup();
    // (i) the poll run.
    let gate = Arc::new(Notify::new());
    let seen = Arc::new(Notify::new());
    let mut b = {
        let mut peer = tls_host(
            false,
            A2aAdmissionPolicy::Allow,
            ResponseMode::NotifyAndDraft,
            &[KEY],
            false,
        )
        .await;
        // Re-compose B with the gated provider and the notifying sink: the
        // same socket, keys and workspace (19.18's restart shape).
        peer.host.options.provider_gate = Some(gate.clone());
        let seen = seen.clone();
        peer.host.options.serve_wrap = Some(Arc::new(move |inner| {
            Arc::new(StatusQueryNotify {
                inner,
                seen: seen.clone(),
            }) as Arc<dyn RoomJournal>
        }));
        let TlsPeer {
            host,
            dir,
            cert_path,
        } = peer;
        TlsPeer {
            host: host.restart().await,
            dir,
            cert_path,
        }
    };
    let (_draft_operator, draft_shutdown) = attach_operator(&mut b.host).await;
    let (egress, journal, keys) = model_rail(vec![credentialed(&b, "b", A_ENV)]).await;
    let running = invoke(&egress, "b");
    tokio::time::timeout(SETTLE, seen.notified())
        .await
        .expect("a served tasks/get was journaled while the provider was held");
    gate.notify_one();
    let outcome = tokio::time::timeout(SETTLE, running)
        .await
        .expect("delegation settles")
        .expect("join");
    let error =
        outcome.expect_err("the draft parks the task: the rail stops at its decision point");
    assert!(
        error.contains("remote agent requested input"),
        "the polls and the closing cancel were served: {error}"
    );
    let key = ItemCapabilityKeyStore::new(keys.path())
        .load()
        .expect("key");
    let item = items(&room_events(&b.host.journal).await)
        .pop()
        .expect("B minted the delegated item");
    assert_eq!(
        item.capability_hash,
        Some(stored_hash(&wire(&key, &item.task))),
        "the model rail's item is capability-bearing"
    );
    // Story 19.19 K08: the real model-tool dispatch grants item capability,
    // but its absent human-text hash must never label the delegated body you.
    let mut sender = sender_over(vec![credentialed(&b, "b", A_ENV)], journal, keys).await;
    let mut state = TuiState::new(130, 30);
    let block = read_thread_block(&mut sender, &mut state, "b", &item.id).await;
    assert!(
        block
            .lines()
            .any(|line| line.trim() == format!("another sender  {}", item.id)),
        "model-rail content has no own human-text hash: {block}"
    );
    draft_shutdown.cancel();

    // (ii) the cancel run.
    let mut asked = tls_host(
        false,
        A2aAdmissionPolicy::Ask,
        ResponseMode::NotifyAndWait,
        &[KEY],
        false,
    )
    .await;
    let (_operator, attach_shutdown) = attach_operator(&mut asked.host).await;
    let (egress, journal, keys) = model_rail(vec![credentialed(&asked, "b", A_ENV)]).await;
    let outcome = tokio::time::timeout(SETTLE, invoke(&egress, "b"))
        .await
        .expect("delegation settles")
        .expect("join");
    let error = outcome.expect_err("auth-required stops the rail");
    assert!(error.contains("remote agent requested input"), "{error}");
    let task = room_events(&journal)
        .await
        .into_iter()
        .find_map(|(_, row)| match row {
            RoomEvent::RemoteEnvelopeDispatched { task, .. } => task,
            _ => None,
        })
        .expect("the model rail's dispatch row");
    let key = ItemCapabilityKeyStore::new(keys.path())
        .load()
        .expect("key");
    let state = raw(&asked, KEY)
        .call(task_body(7, "tasks/get", &task, Some(&wire(&key, &task))))
        .await;
    assert_eq!(state["result"]["status"]["state"], "canceled", "{state}");
    attach_shutdown.cancel();
}

/// `K17` (`P1`, `A22`) — no co-holder task-id squat. While A's `m` is held in
/// memory, a duplicate send of `m` answers `already in flight` (the
/// reference bytes). After B restarts until A's own `tasks/get` for `m`
/// answers not-found, co-holder E's resend of `m` — with E's own carrier,
/// then with none — answers the SAME bytes and B writes nothing. Positive
/// controls: A's resend with its own carrier is accepted; B's operator's
/// reply to `ri_A` reaches A's thread. `M28` → RED: drop the `A22` check.
#[tokio::test]
async fn k17_a_co_holder_cannot_squat_another_senders_task_id() {
    setup();
    let mut b = tls_host(
        false,
        A2aAdmissionPolicy::Allow,
        ResponseMode::NotifyAndWait,
        &[KEY],
        false,
    )
    .await;
    let (mut operator, attach_shutdown) = attach_operator(&mut b.host).await;
    let mut a = new_sender(vec![credentialed(&b, "b", A_ENV)]).await;
    let key_a = sender_key(&a);
    let mut state = TuiState::new(120, 40);
    let sent = send_item(&mut a, &mut state, "b", &b.host, "question").await;
    let m = sent.task.clone();
    let own_carrier = serde_json::json!({ "x-rustain-item-capability": wire(&key_a, &m) });
    let reference = raw(&b, KEY)
        .text(send_body(20, &m, own_carrier.clone()))
        .await;
    assert!(reference.contains("is already in flight"), "{reference}");

    // Terminate the card (a direct turn later needs none) and restart until
    // A's own poll for `m` is not-found — a squat test whose task is still
    // held proves nothing.
    pending_card(&b.host, &waiting_node(&b.host, &m).await).await;
    operator_reply(&mut operator, &sent.id, "first answer").await;
    attach_shutdown.cancel();
    drop(operator);
    let mut b = b;
    let mut restarts = 0;
    loop {
        let TlsPeer {
            host,
            dir,
            cert_path,
        } = b;
        b = TlsPeer {
            host: host.restart().await,
            dir,
            cert_path,
        };
        restarts += 1;
        let own = raw(&b, KEY)
            .call(task_body(21, "tasks/get", &m, Some(&wire(&key_a, &m))))
            .await;
        if own["error"]["code"] == -32001 {
            break;
        }
        assert!(restarts < 3, "A's task is never released: {own}");
    }
    let length = room_events(&b.host.journal).await.len();
    let e = raw(&b, KEY);
    for carrier in [
        serde_json::json!({ "x-rustain-item-capability": stranger(&m) }),
        serde_json::json!({}),
    ] {
        assert_eq!(e.text(send_body(20, &m, carrier)).await, reference);
    }
    assert_eq!(
        room_events(&b.host.journal).await.len(),
        length,
        "zero rows"
    );

    // Positive controls.
    let resent = raw(&b, KEY).call(send_body(22, &m, own_carrier)).await;
    assert!(
        resent["result"]["metadata"]["x-rustain-item-id"].is_string(),
        "A's own resend is accepted: {resent}"
    );
    let (mut operator, attach_shutdown) = attach_operator(&mut b.host).await;
    let rr = frame_turn(&operator_reply(&mut operator, &sent.id, "the answer").await);
    let block = read_thread_block(&mut a, &mut state, "b", &sent.id).await;
    assert!(
        block.contains(&format!("{rr}\n      the answer")),
        "{block}"
    );
    attach_shutdown.cancel();
}

/// `K18` (`PQ1` = A) — an unusable key refuses credentialed peers and sends
/// legacy to uncredentialed ones; the key file is never rewritten; a
/// repaired key sends with a carrier. `M31` → RED: send legacy when
/// unusable. `M32` → RED: refuse uncredentialed peers too.
#[tokio::test]
async fn k18_an_unusable_key_refuses_credentialed_peers_and_is_never_rewritten() {
    use std::os::unix::fs::PermissionsExt;
    setup();
    let b = tls_host(
        false,
        A2aAdmissionPolicy::Allow,
        ResponseMode::NotifyAndAuto,
        &[KEY],
        false,
    )
    .await;
    let l = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndAuto).await;
    let keys = Arc::new(tempfile::tempdir().expect("keys"));
    let key_path = keys
        .path()
        .join(rustain::adapters::a2a::item_capability::ITEM_CAPABILITY_KEY_FILE);
    std::fs::write(&key_path, [9u8; 32]).expect("plant");
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o644)).expect("0644");
    let planted = (
        std::fs::read(&key_path).expect("bytes"),
        std::fs::metadata(&key_path)
            .expect("meta")
            .modified()
            .expect("mtime"),
    );
    let peers = || vec![credentialed(&b, "b", A_ENV), l.peer("l")];
    let mut a = sender_over(peers(), journal().await, keys.clone()).await;
    let mut state = TuiState::new(120, 40);
    let b_length = room_events(&b.host.journal).await.len();
    let a_rows = room_events(&a.journal).await.len();

    a.type_command(&mut state, "/team send b hello").await;
    settle_send(&mut a, &mut state).await;
    assert_eq!(
        send_row(&state, 1),
        format!(
            "  ⚠ b  unreachable\n    this host's item key can't be read ({}: item capability \
             key file permissions must be 0600, got 644). Fix its permissions (chmod 600) or \
             restore it; don't delete it.",
            key_path.display()
        )
    );
    let rows = room_events(&a.journal).await;
    let new_rows = &rows[a_rows..];
    // Story 19.21: the one preflight-refusal row now sits between the send
    // action's record and the recipient's typed settlement — and nothing else:
    // still no dispatch row, so still no POST (`PQ1`).
    assert_eq!(new_rows.len(), 3, "{new_rows:?}");
    assert!(matches!(
        new_rows[0].1,
        RoomEvent::TeamSendActionRecorded { .. }
    ));
    assert!(matches!(
        new_rows[1].1,
        RoomEvent::RemoteEnvelopeRejected { task: None, .. }
    ));
    assert!(matches!(
        new_rows[2].1,
        RoomEvent::TeamSendRecipientSettled {
            settlement: rustain::domain::models::SendSettlement::Unreachable { .. },
            ..
        }
    ));
    assert!(
        !new_rows
            .iter()
            .any(|row| matches!(row.1, RoomEvent::RemoteEnvelopeDispatched { .. })),
        "a refused send writes no dispatch row: {new_rows:?}"
    );
    assert_eq!(room_events(&b.host.journal).await.len(), b_length);

    // The model rail refuses before `delegate`: no dispatch row, so no POST.
    let (egress, rail_journal, _) = {
        let journal = journal().await;
        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
        let egress = A2aEgress::compose(
            vec![credentialed(&b, "b", A_ENV)],
            NodeTree::new(),
            journal.room.clone(),
            journal.node.clone(),
            event_tx,
            load_item_capability_key(Ok(keys.path().to_path_buf())),
        )
        .expect("an unusable key composes");
        tokio::time::timeout(SETTLE, egress.await_cards_settled())
            .await
            .expect("cards settle");
        (egress, journal, ())
    };
    let refused = tokio::time::timeout(SETTLE, invoke(&egress, "b"))
        .await
        .expect("settles")
        .expect("join")
        .expect_err("refused");
    assert!(
        refused.contains("this host's item key can't be read"),
        "{refused}"
    );
    assert!(
        room_events(&rail_journal).await.is_empty(),
        "no dispatch row"
    );
    assert_eq!(room_events(&b.host.journal).await.len(), b_length);

    // Positive control: an uncredentialed peer still sends — legacy.
    let local = send_item(&mut a, &mut state, "l", &l, "local").await;
    assert_eq!(
        local.capability_hash, None,
        "no carrier on the loopback row"
    );

    // Repair: chmod 600 and re-compose; `b` now delivers WITH a carrier.
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).expect("0600");
    let Sender { journal, keys, .. } = a;
    let mut a = sender_over(peers(), journal, keys).await;
    let repaired = send_item(&mut a, &mut state, "b", &b.host, "repaired").await;
    assert_eq!(
        repaired.capability_hash,
        Some(stored_hash(&wire(&sender_key(&a), &repaired.task)))
    );
    assert_eq!(
        (
            std::fs::read(&key_path).expect("bytes"),
            std::fs::metadata(&key_path)
                .expect("meta")
                .modified()
                .expect("mtime"),
        ),
        planted,
        "the key file's bytes and mtime never change"
    );
}
