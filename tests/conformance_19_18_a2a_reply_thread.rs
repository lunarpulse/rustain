//! Story 19.18 — multi-turn reply round trip keystones (FR169, as amended).
//!
//! Every keystone enters through a production front door:
//! - **answering host B**: a real `serve()` with the production
//!   `AttachServer` runtime; B's operator is a real daemon-socket client
//!   (`attach_operator`), writing `ClientFrame`s;
//! - **asker**: `submit_message_for_test` → `transparency_bridge::team_command`
//!   → the rail-3 spawn → the published `AppEvent` → its production handler;
//! - **served verbs**: real HTTP through `dispatch`.
//!
//! Copied, ⛔ not imported (everything there is file-private):
//! `Journal`, `ModePolicy`, `ScriptedProvider`, `build_runtime`, `Host`,
//! `sender`, `app_state` and the settle helper from
//! `tests/conformance_19_17_a2a_fanout.rs:54-473` (the `GateJournal` hold from
//! `:516-531`), and `attach_operator` from `tests/a2a_server_exec.rs:1105-1160`.
//! Two deliberate differences, both production's composition:
//! - the served sink carries `.with_reader(journal.node)` (`daemon/mod.rs:596-599`);
//! - a restarted B keeps ONE listening socket (`try_clone`, `P16`), its identity
//!   keys and its workspace, re-runs `NodeRecovery` and reloads the persisted
//!   conversation (`daemon/mod.rs:384-453`).
//!
//! ⛔ Forbidden bypasses, none used: calling `items_thread` / the projection /
//! the reply arm directly; hand-appending `RecipientItemReplied`; `PeerFixture`;
//! a `runtime: None` host; an `InboundPeerRuntime` double; `tasks/get` as the
//! reply channel. The ONE seeded record is another principal's EARLIER
//! INGRESS (`RecipientItemReceived` under `credential-b`, the 19.16d
//! technique) — never a turn a keystone claims was authored.
//!
//! ⚠ Loopback proves no cross-principal separation (19.16b `A13`): every
//! loopback caller is one principal. `K06` proves the byte-identity at the
//! front door with a seeded foreign principal; `K21` runs on a real
//! non-loopback TLS + API-key listener.

#![cfg(feature = "a2a")]

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
    AttachMode, ClientFrame, ConnectionTier, DaemonFrame, PROTOCOL_VERSION, PeerDraftAction,
    ProtocolError, RecipientAction, RecipientActionOutcome, RecipientActionTarget, read_frame,
    write_frame,
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
    A2aPeerSource, A2aPeerSpec, AppConfig, Authorship, CompletionOptions, Conversation,
    FeedbackLevel, JournalRecord, Message, ResponseMode, RoomEvent, StopReason, StreamChunk,
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
        let node_tree = NodeTree::with_event_tx(
            domain_tx.clone(),
            Arc::new(|| chrono::Utc::now().timestamp_millis()),
        )
        .with_journal(journal.node.clone());
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
            Arc::new(DaemonCore::new(
                workspace.clone(),
                Arc::new(ArcSwap::from_pointee(AppConfig::default())),
                Arc::new(NoOpMemory),
                storage.clone(),
                Arc::new(NoOpSecurity),
                Arc::new(NoOpPersona),
                Arc::new(rustain::adapters::rap::PeerTopicStore::new()),
                Box::new(move || {
                    Ok(build_runtime(
                        Arc::new(ScriptedProvider {
                            text: draft.clone(),
                        }),
                        storage.clone(),
                        &workspace,
                    ))
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
        let server = AttachServer::new_with_node_tree_bus_policy_and_journal(
            core,
            Arc::new(tokio::sync::Mutex::new(conversation)),
            domain_tx,
            node_tree.clone(),
            bus,
            delivery,
            attach_journal,
            journal.node.clone(),
            None,
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

    async fn restart(self) -> Self {
        self.cancel.cancel();
        self.http.await.expect("server task").expect("shutdown");
        let Self {
            journal,
            listener,
            keys,
            options,
            server,
            node_tree,
            _singleton,
            ..
        } = self;
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
    app_state: AppState,
    domain_rx: tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
}

async fn new_sender(peers: Vec<A2aPeerSpec>) -> Sender {
    sender_over(peers, journal().await).await
}

/// An asker over an existing journal — an asker RESTART is a new process
/// (new egress, new `AppState`) over the same workspace.
async fn sender_over(peers: Vec<A2aPeerSpec>, journal: Journal) -> Sender {
    let (egress_tx, _egress_rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    // Story 19.18c `A15`: `Disabled` — this file proves the PRE-19.18c
    // behaviour (legacy on both sides, every legacy→legacy edge counts; `K07`
    // follows up on an `rr_` it never observed). The keyed twin is 19.18c's
    // `K09` in `conformance_19_18c_a2a_item_capability.rs`.
    let egress = A2aEgress::compose(
        peers,
        NodeTree::new(),
        journal.room.clone(),
        journal.node.clone(),
        egress_tx,
        rustain::adapters::a2a::item_capability::ItemCapabilityKeyState::Disabled,
    )
    .expect("compose production sender egress");
    tokio::time::timeout(SETTLE, egress.await_cards_settled())
        .await
        .expect("sender card fetches settle");
    let (mut app_state, domain_rx) = app_state(&journal.path);
    app_state.a2a_send = Some(egress.runtime().clone());
    Sender {
        journal,
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

async fn poll_state(endpoint: &str, task: &str, want: &str) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + SETTLE;
    loop {
        let answer = rpc(
            endpoint,
            serde_json::json!({"jsonrpc":"2.0","id":9,"method":"tasks/get","params":{"id":task}}),
        )
        .await;
        if answer["result"]["status"]["state"] == want || tokio::time::Instant::now() > deadline {
            return answer;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn received(rows: &[(u64, RoomEvent)]) -> Vec<(String, String, String)> {
    rows.iter()
        .filter_map(|(_, row)| match row {
            RoomEvent::RecipientItemReceived {
                address,
                task,
                content,
                ..
            } => Some((
                address.item().as_str().to_owned(),
                task.clone(),
                content.clone(),
            )),
            _ => None,
        })
        .collect()
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

/// Wait for an operator action outcome, ignoring asynchronous transcript,
/// row-control, and approval notifications on the same socket.
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

async fn resolve(
    operator: &mut tokio::net::UnixStream,
    node: &str,
    action: rustain::adapters::daemon::protocol::PeerDraftAction,
) -> DaemonFrame {
    write_frame(
        operator,
        &ClientFrame::ResolvePeerDraft {
            node: node.to_owned(),
            action,
        },
    )
    .await
    .expect("resolution frame");
    next_answer(operator).await
}

fn replies(rows: &[(u64, RoomEvent)]) -> Vec<(u64, ReplyRow)> {
    rows.iter()
        .filter_map(|(seq, row)| match row {
            RoomEvent::RecipientItemReplied {
                address,
                answers,
                content,
                authorship,
                via_node,
                ..
            } => Some((
                *seq,
                ReplyRow {
                    turn: address.turn().as_str().to_owned(),
                    answers: answers.as_str().to_owned(),
                    content: content.clone(),
                    authorship: *authorship,
                    via_node: via_node.as_ref().map(|node| node.as_str().to_owned()),
                },
            )),
            _ => None,
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReplyRow {
    turn: String,
    answers: String,
    content: String,
    authorship: Authorship,
    via_node: Option<String>,
}

fn resolved(rows: &[(u64, RoomEvent)]) -> Vec<(u64, String, bool)> {
    rows.iter()
        .filter_map(|(seq, row)| match row {
            RoomEvent::PeerDraftResolved { node, sent, .. } => {
                Some((*seq, node.as_str().to_owned(), *sent))
            }
            _ => None,
        })
        .collect()
}

fn observed(rows: &[(u64, RoomEvent)]) -> Vec<(String, String, Option<String>)> {
    rows.iter()
        .filter_map(|(_, row)| match row {
            RoomEvent::PeerReplyObserved {
                peer,
                turn,
                content,
                ..
            } => Some((peer.to_string(), turn.clone(), content.clone())),
            _ => None,
        })
        .collect()
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
        panic!("expected an applied item reply result, got {frame:?}");
    };
    assert_eq!(item_id, &receipt.item_id);
    assert_eq!(receipt.kind, rustain::domain::models::ReplyKind::Answer);
    receipt.turn_id.clone()
}

/// Send one question to `b` and return `(item id from the board, task)`.
async fn ask(
    sender: &mut Sender,
    state: &mut TuiState,
    host: &Host,
    text: &str,
) -> (String, String) {
    let before = received(&room_events(&host.journal).await).len();
    sender
        .type_command(state, &format!("/team send b {text}"))
        .await;
    settle_send(sender, state).await;
    let items = received(&room_events(&host.journal).await);
    assert_eq!(items.len(), before + 1, "exactly one item minted");
    let (item, task, _) = items.last().cloned().expect("item");
    (item, task)
}

// ── Keystones ───────────────────────────────────────────────────────────────

/// `K02`, `K05`, `K07`, `K09`, `K10`, `K18` — the headline round trip, every
/// hop through its front door: the asker's typed `/team send` → B's real
/// ingress → the operator's discovery through `/team board` (`P8`) → B's
/// operator on the real daemon socket → the served thread → the asker's
/// `/team thread` → `/team followup` → B's `/team reply` → the four-turn
/// thread, journaled once.
#[tokio::test]
async fn the_reply_round_trip_through_every_front_door() {
    let mut b = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let (mut operator, attach_shutdown) = attach_operator(&mut b).await;
    let mut sender = new_sender(vec![b.peer("b")]).await;
    let mut state = TuiState::new(120, 40);

    sender
        .type_command(&mut state, "/team send b question")
        .await;
    settle_send(&mut sender, &mut state).await;
    assert_eq!(
        state.feedback_blocks["team-send-1"].message,
        "  ● b  delivered"
    );
    let items = received(&room_events(&b.journal).await);
    let (_, task_q, _) = items[0].clone();
    // `P8`: the operator learns the id from the board, not from B's journal.
    let board = read_board(&mut sender, &mut state, "b").await;
    let ids = board_item_ids(&board);
    assert_eq!(ids.len(), 1, "{board}");
    let ri_q = ids[0].clone();
    assert!(!board.contains("replies"), "no reply yet: {board}");
    let node_q = waiting_node(&b, &task_q).await;
    pending_card(&b, &node_q).await;
    let sender_rows_before = room_events(&sender.journal).await;

    // ── K02: B's operator answers through the real socket (d′ applies). ──
    let body = "use the outbox\n  pattern";
    let frame = operator_reply(&mut operator, &ri_q, body).await;
    let rr_1 = frame_turn(&frame);
    let b_rows = room_events(&b.journal).await;
    let turns = replies(&b_rows);
    assert_eq!(turns.len(), 1, "exactly one reply turn");
    assert_eq!(
        turns[0].1,
        ReplyRow {
            turn: rr_1.clone(),
            answers: ri_q.clone(),
            content: body.to_owned(),
            authorship: Authorship::HumanWritten,
            via_node: Some(node_q.clone()),
        }
    );
    let settled = resolved(&b_rows);
    assert_eq!(settled.len(), 1);
    assert!(settled[0].2, "the card was sent");
    assert!(
        turns[0].0 < settled[0].0,
        "the turn is durable BEFORE the card resolves"
    );
    let completed = poll_state(&b.endpoint, &task_q, "completed").await;
    assert_eq!(completed["result"]["status"]["state"], "completed");
    assert_eq!(
        persisted_card(&b, &node_q).await.as_deref(),
        Some(body),
        "the card cleared: the row holds the sent answer, no longer pending"
    );
    let listed = rpc(
        &b.endpoint,
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"x-rustain-items/list","params":{}}),
    )
    .await;
    assert_eq!(
        listed["result"]["items"][0]["state"], "received",
        "the answered item's state is untouched (`M03`)"
    );
    assert_eq!(listed["result"]["items"][0]["replyCount"], 1);
    assert_eq!(
        room_events(&sender.journal).await,
        sender_rows_before,
        "nothing reached the asker: no push (`A2`)"
    );

    // ── K05: the served read, through the real HTTP dispatch. ──
    let length = room_events(&b.journal).await.len();
    let mut served = serde_json::Value::Null;
    for _ in 0..3 {
        served = thread_rpc(&b, &ri_q).await;
    }
    assert_eq!(
        room_events(&b.journal).await.len(),
        length,
        "three reads write nothing (`M08`)"
    );
    let turns_json = served["result"]["turns"].as_array().expect("turns");
    assert_eq!(served["result"]["root"], ri_q.as_str());
    assert_eq!(served["result"]["omittedBefore"], 0);
    assert_eq!(served["result"]["principalCollapsed"], true);
    assert_eq!(
        turns_json,
        &vec![
            serde_json::json!({
                "turnId": ri_q, "kind": "item", "task": task_q, "state": "received",
                "content": "question", "ordinal": 0,
            }),
            serde_json::json!({
                "turnId": rr_1, "kind": "reply", "inReplyTo": ri_q,
                "authorship": "human-written", "content": body, "ordinal": 1,
            }),
        ],
        "a dense ordinal and ⛔ no journal order on the wire (`M10`)"
    );
    let empty = rpc(
        &b.endpoint,
        serde_json::json!({"jsonrpc":"2.0","id":4,"method":"x-rustain-items/thread","params":{"itemId":""}}),
    )
    .await;
    assert_eq!(empty["error"]["code"], -32602, "`M33`");

    // ── K07: the follow-up is a 19.17 delivery carrying the id it answers. ──
    sender
        .type_command(
            &mut state,
            &format!("/team followup b {rr_1}  and for retries?"),
        )
        .await;
    settle_send(&mut sender, &mut state).await;
    assert_eq!(
        state.feedback_blocks["team-send-2"].message,
        "  ● b  delivered"
    );
    let b_rows = room_events(&b.journal).await;
    let follow = b_rows
        .iter()
        .filter_map(|(_, row)| match row {
            RoomEvent::RecipientItemReceived {
                address,
                task,
                content,
                in_reply_to,
                ..
            } if in_reply_to.is_some() => Some((
                address.item().as_str().to_owned(),
                task.clone(),
                content.clone(),
                in_reply_to.clone(),
            )),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(follow.len(), 1, "one follow-up item (`M12`, `M13`)");
    let (ri_f, task_f, content_f, link) = follow[0].clone();
    assert_eq!(
        content_f, "and for retries?",
        "the body after two positional tokens"
    );
    assert_eq!(link.as_deref(), Some(rr_1.as_str()));
    let node_f = waiting_node(&b, &task_f).await;
    pending_card(&b, &node_f).await;
    let rr_2 = frame_turn(&operator_reply(&mut operator, &ri_f, "retry with backoff").await);

    // ── K09 + K10: the asker reads the four-turn thread. ──
    let block = read_thread_block(&mut sender, &mut state, "b", &ri_q).await;
    assert_eq!(
        block,
        format!(
            "thread · b · {ri_q}\n  you       {ri_q}\n      question\n  b         {rr_1}\n      use the outbox\n        pattern\n  you       {ri_f}\n      and for retries?\n  b         {rr_2}\n      retry with backoff\n{}",
            rustain::adapters::a2a::board::COLLAPSED_PRINCIPAL_DISCLOSURE
        )
    );
    let seen = observed(&room_events(&sender.journal).await);
    let b_identity = b.peer("b").resolved_identity().to_string();
    assert_eq!(
        seen,
        vec![
            (b_identity.clone(), rr_1.clone(), Some(body.to_owned())),
            (
                b_identity.clone(),
                rr_2.clone(),
                Some("retry with backoff".to_owned())
            ),
        ],
        "one row per PEER turn, raw content, the peer's identity (`M19`)"
    );
    let again = read_thread_block(&mut sender, &mut state, "b", &ri_f).await;
    assert_eq!(again, block, "any member reads the same thread");
    assert_eq!(
        observed(&room_events(&sender.journal).await).len(),
        2,
        "a second read journals nothing (`M18`)"
    );
    assert_eq!(
        state
            .feedback_blocks
            .keys()
            .filter(|id| id.starts_with("team-thread"))
            .count(),
        1,
        "one block, replaced (`M17`)"
    );
    let log = rustain::domain::services::transparency::fold_transparency(
        &RoomJournalReader::load_entries(sender.journal.node.as_ref())
            .await
            .expect("sender journal"),
    );
    assert_eq!(
        log.iter()
            .filter(|row| row.kind
                == rustain::domain::services::transparency::TransparencyKind::PeerReplyObserved)
            .count(),
        2,
        "the observed turns appear in `/team log`"
    );

    // Positive control: a new send is a new ROOT, not in this thread.
    let (ri_new, _) = ask(&mut sender, &mut state, &b, "new topic").await;
    assert!(!block.contains(&ri_new));
    assert_eq!(
        served_ids(&thread_rpc(&b, &ri_new).await),
        vec![ri_new.clone()]
    );

    // ── K18: the board carries the reply signal. The first board armed the
    // stated refresh floor (a design bound, 19.16b `AC5(d)`); outlast it
    // exactly once rather than race it. ──
    tokio::time::sleep(rustain::adapters::a2a::board::board_refresh_floor()).await;
    let board = read_board(&mut sender, &mut state, "b").await;
    let row = board
        .lines()
        .find(|line| line.contains(&format!("item {ri_q} ")))
        .unwrap_or_else(|| panic!("the answered item's row: {board}"));
    assert!(row.ends_with(" · 1 replies"), "{row}");
    let new_row = board
        .lines()
        .find(|line| line.contains(&format!("item {ri_new} ")))
        .expect("the reply-less row");
    assert!(!new_row.contains("replies"), "{new_row}");

    attach_shutdown.cancel();
}

/// `K02` failure case + positive control. A failed durable reply append returns
/// a correlated refusal and no turn exists. An acknowledged item with no
/// pending card takes one direct turn, `via_node: None`.
#[tokio::test]
async fn a_failed_reply_is_never_confirmed_and_a_cardless_item_takes_a_direct_turn() {
    let mut options = HostOptions::new(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndAuto);
    options.attach_wrap = Some(Arc::new(|inner| {
        Arc::new(FailReplyJournal {
            inner,
            failing: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            kind: FailKind::Reply,
        }) as Arc<dyn RoomJournal>
    }));
    let mut b = Host::serve_with(options).await;
    let (mut operator, attach_shutdown) = attach_operator(&mut b).await;
    let mut sender = new_sender(vec![b.peer("b")]).await;
    let mut state = TuiState::new(120, 40);
    let (ri_q, _) = ask(&mut sender, &mut state, &b, "question").await;
    let frame = operator_reply(&mut operator, &ri_q, "answer").await;
    assert!(
        matches!(
            &frame,
            DaemonFrame::RecipientActionResult {
                action: RecipientAction::Reply,
                target: RecipientActionTarget::Item { item_id },
                outcome: RecipientActionOutcome::Refused {
                    error: ProtocolError::Internal(_),
                },
            } if item_id == &ri_q
        ),
        "{frame:?}"
    );
    assert!(replies(&room_events(&b.journal).await).is_empty());
    attach_shutdown.cancel();

    // Positive control on a clean host: ack, then reply with no card.
    let mut c = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndAuto).await;
    let (mut operator, attach_shutdown) = attach_operator(&mut c).await;
    let mut sender = new_sender(vec![c.peer("b")]).await;
    let (ri_a, _) = ask(&mut sender, &mut state, &c, "question").await;
    write_frame(
        &mut operator,
        &ClientFrame::AcknowledgeRecipientItem {
            item_id: ri_a.clone(),
        },
    )
    .await
    .expect("ack");
    assert!(matches!(
        next_answer(&mut operator).await,
        DaemonFrame::RecipientActionResult {
            action: RecipientAction::Acknowledge,
            target: RecipientActionTarget::Item { item_id },
            outcome: RecipientActionOutcome::Applied { reply: None },
        } if item_id == ri_a
    ));
    let rr = frame_turn(&operator_reply(&mut operator, &ri_a, "answer").await);
    let turns = replies(&room_events(&c.journal).await);
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].1.turn, rr);
    assert_eq!(turns[0].1.via_node, None);
    assert!(resolved(&room_events(&c.journal).await).is_empty());
    attach_shutdown.cancel();
}

/// `K03` — a tombstone, an unknown id and a reply-turn id are refused with
/// the shipped sentences; a reply naming a host path is refused at authoring
/// (`P13`); an oversized reply is refused. Every refusal writes zero rows.
///
/// `M04` → RED: accept a reply to a `Removed` item. `M39` → RED: skip the
/// author-time scrub.
#[tokio::test]
async fn refused_replies_write_nothing() {
    // Wait mode: no auto-response appends rows behind the test's back, so the
    // journal length is a clean zero-rows witness.
    let mut b = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let (mut operator, attach_shutdown) = attach_operator(&mut b).await;
    let mut sender = new_sender(vec![b.peer("b")]).await;
    let mut state = TuiState::new(120, 40);
    let (ri_r, _) = ask(&mut sender, &mut state, &b, "to be removed").await;
    let (ri_q, task_q) = ask(&mut sender, &mut state, &b, "question").await;
    let node_q = waiting_node(&b, &task_q).await;
    pending_card(&b, &node_q).await;
    let rr = frame_turn(&operator_reply(&mut operator, &ri_q, "answer").await);
    write_frame(
        &mut operator,
        &ClientFrame::RemoveRecipientItem {
            item_id: ri_r.clone(),
        },
    )
    .await
    .expect("remove");
    let removed = |rows: &[(u64, RoomEvent)]| {
        rows.iter()
            .any(|(_, row)| matches!(row, RoomEvent::RecipientItemRemoved { .. }))
    };
    let deadline = tokio::time::Instant::now() + SETTLE;
    while !removed(&room_events(&b.journal).await) {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let length = room_events(&b.journal).await.len();
    for (item, text, expected) in [
        (
            ri_r.as_str(),
            "late",
            "recipient item already removed".to_owned(),
        ),
        ("ri_zz", "hello", "recipient item not found".to_owned()),
        (
            rr.as_str(),
            "to myself",
            "recipient item not found".to_owned(),
        ),
        (
            ri_q.as_str(),
            "see /etc/x for it",
            "this reply names a host path, which peers are never shown — rephrase it".to_owned(),
        ),
        (
            ri_q.as_str(),
            &"x".repeat(rustain::domain::models::MAX_REPLY_BYTES + 1),
            format!(
                "reply is longer than {} bytes — shorten it",
                rustain::domain::models::MAX_REPLY_BYTES
            ),
        ),
    ] {
        let frame = operator_reply(&mut operator, item, text).await;
        let DaemonFrame::RecipientActionResult {
            action,
            target,
            outcome: RecipientActionOutcome::Refused { error },
        } = frame
        else {
            panic!("{item}: expected a correlated refusal, got {frame:?}");
        };
        assert_eq!(action, RecipientAction::Reply);
        assert_eq!(
            target,
            RecipientActionTarget::Item {
                item_id: item.to_owned()
            }
        );
        assert!(error.to_string().contains(&expected), "{item}: {error}");
    }
    assert_eq!(room_events(&b.journal).await.len(), length, "zero rows");
    attach_shutdown.cancel();
}

/// K07 — composition and send authorization remain independent on raw reply
/// events; the public thread preserves only composition and its existing marks.
#[tokio::test]
async fn draft_resolutions_record_exactly_the_sending_turns() {
    use rustain::domain::models::SendAuthorization;

    let assert_facts = |rows: &[(u64, RoomEvent)], item: &str, expected, act: &str| {
        let facts: Vec<_> = rows
            .iter()
            .filter_map(|(_, event)| match event {
                RoomEvent::RecipientItemReplied {
                    answers,
                    authorship,
                    send_authorization,
                    ..
                } if answers.as_str() == item => Some((*authorship, *send_authorization)),
                _ => None,
            })
            .collect();
        assert_eq!(
            facts,
            vec![(expected, SendAuthorization::HumanDecided)],
            "K07 {act}: exactly one raw reply records composition independently of the human send decision"
        );
    };
    let assert_thread = |thread: &serde_json::Value, expected: &str, act: &str| {
        let turns = thread["result"]["turns"]
            .as_array()
            .expect("K07 served thread contains turns");
        let replies: Vec<_> = turns
            .iter()
            .filter(|turn| turn["kind"] == "reply")
            .collect();
        assert!(
            !replies.is_empty(),
            "K07 {act}: privacy assertion requires a served reply"
        );
        for reply in replies {
            assert_eq!(
                reply["authorship"], expected,
                "K07 {act}: authorship keeps its existing wire token"
            );
        }
        let json = serde_json::to_string(thread).expect("thread JSON");
        assert!(
            !json.contains("\"sendAuthorization\"") && !json.contains("\"send_authorization\""),
            "K07 {act}: public thread must not expose either spelling of send authorization: {json}"
        );
    };
    // (1) WriteOwn on a wait-mode card.
    let mut b = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let (mut operator, attach_shutdown) = attach_operator(&mut b).await;
    let mut sender = new_sender(vec![b.peer("b")]).await;
    let mut state = TuiState::new(120, 40);
    let (ri_w, task_w) = ask(&mut sender, &mut state, &b, "question").await;
    let node_w = waiting_node(&b, &task_w).await;
    pending_card(&b, &node_w).await;
    let resolution = resolve(
        &mut operator,
        &node_w,
        PeerDraftAction::WriteOwn {
            content: "human answer".to_owned(),
        },
    )
    .await;
    assert!(matches!(
        &resolution,
        DaemonFrame::RecipientActionResult {
            action: RecipientAction::Response,
            target: RecipientActionTarget::Node { node_id },
            outcome: RecipientActionOutcome::Applied {
                reply: Some(receipt),
            },
        } if node_id == &node_w && receipt.item_id.as_str() == ri_w.as_str()
    ));
    let completed = poll_state(&b.endpoint, &task_w, "completed").await;
    let turns = replies(&room_events(&b.journal).await);
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].1.answers, ri_w);
    assert_eq!(
        turns[0].1.authorship,
        Authorship::HumanWritten,
        "K07 WriteOwn: the human-written reply turn keeps HumanWritten authorship"
    );
    assert_facts(
        &room_events(&b.journal).await,
        &ri_w,
        Authorship::HumanWritten,
        "WriteOwn",
    );
    assert_thread(&thread_rpc(&b, &ri_w).await, "human-written", "WriteOwn");
    assert_eq!(
        completed["result"]["status"]["message"]["parts"][0]["text"],
        turns[0].1.content.as_str()
    );
    // Direct operator reply and defer use the same frames as their /team verbs.
    for (defer, act) in [(false, "/team reply"), (true, "/team defer")] {
        let item = if defer {
            let (item, task) = ask(&mut sender, &mut state, &b, act).await;
            let node = waiting_node(&b, &task).await;
            pending_card(&b, &node).await;
            item
        } else {
            // The WriteOwn card is already resolved: this must exercise the
            // direct reply writer, not resolve another live card as WriteOwn.
            ri_w.clone()
        };
        let frame = if defer {
            write_frame(
                &mut operator,
                &ClientFrame::DeferRecipientItem {
                    item_id: item.clone(),
                },
            )
            .await
            .expect("K07 send direct defer");
            next_answer(&mut operator).await
        } else {
            operator_reply(&mut operator, &item, "direct human answer").await
        };
        assert!(
            matches!(
                frame,
                DaemonFrame::RecipientActionResult {
                    outcome: RecipientActionOutcome::Applied { reply: Some(_) },
                    ..
                }
            ),
            "K07 {act}: direct operator act must produce a durable turn"
        );
        let rows = room_events(&b.journal).await;
        if defer {
            assert_facts(&rows, &item, Authorship::HumanWritten, act);
        } else {
            let facts: Vec<_> = rows
                .iter()
                .filter_map(|(_, event)| match event {
                    RoomEvent::RecipientItemReplied {
                        answers,
                        authorship,
                        send_authorization,
                        ..
                    } if answers.as_str() == item => Some((*authorship, *send_authorization)),
                    _ => None,
                })
                .collect();
            assert_eq!(
                facts,
                vec![(Authorship::HumanWritten, SendAuthorization::HumanDecided); 2],
                "K07 /team reply: the resolved-card direct writer adds one human-written, human-decided turn without changing WriteOwn"
            );
        }
        assert_thread(&thread_rpc(&b, &item).await, "human-written", act);
    }
    attach_shutdown.cancel();

    // (2) Approve and (3) Reject on draft-mode cards.
    let mut d = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndDraft).await;
    let (mut operator, attach_shutdown) = attach_operator(&mut d).await;
    let mut sender = new_sender(vec![d.peer("b")]).await;
    let (ri_a, task_a) = ask(&mut sender, &mut state, &d, "approve me").await;
    let node_a = waiting_node(&d, &task_a).await;
    pending_card(&d, &node_a).await;
    let approval = resolve(&mut operator, &node_a, PeerDraftAction::Approve).await;
    assert!(matches!(
        &approval,
        DaemonFrame::RecipientActionResult {
            action: RecipientAction::Response,
            target: RecipientActionTarget::Node { node_id },
            outcome: RecipientActionOutcome::Applied {
                reply: Some(receipt),
            },
        } if node_id == &node_a && receipt.item_id.as_str() == ri_a.as_str()
    ));
    poll_state(&d.endpoint, &task_a, "completed").await;
    assert_facts(
        &room_events(&d.journal).await,
        &ri_a,
        Authorship::AgentComposed,
        "Approve",
    );
    assert_thread(&thread_rpc(&d, &ri_a).await, "agent-composed", "Approve");
    let rendered = read_thread_block(&mut sender, &mut state, "b", &ri_a).await;
    assert!(
        rendered.contains("[agent-composed]"),
        "K07 approved composition remains marked in /team thread: {rendered}"
    );

    let (ri_e, task_e) = ask(&mut sender, &mut state, &d, "edit me").await;
    let node_e = waiting_node(&d, &task_e).await;
    pending_card(&d, &node_e).await;
    let edited = resolve(
        &mut operator,
        &node_e,
        PeerDraftAction::Edit {
            content: "edited answer".to_owned(),
        },
    )
    .await;
    assert!(
        matches!(
            edited,
            DaemonFrame::RecipientActionResult {
                outcome: RecipientActionOutcome::Applied { reply: Some(_) },
                ..
            }
        ),
        "K07 Edit must send a durable reply"
    );
    poll_state(&d.endpoint, &task_e, "completed").await;
    assert_facts(
        &room_events(&d.journal).await,
        &ri_e,
        Authorship::AgentComposed,
        "Edit",
    );
    assert_thread(&thread_rpc(&d, &ri_e).await, "agent-composed", "Edit");
    let (_, task_r) = ask(&mut sender, &mut state, &d, "reject me").await;
    let node_r = waiting_node(&d, &task_r).await;
    pending_card(&d, &node_r).await;
    let rejection = resolve(&mut operator, &node_r, PeerDraftAction::Reject).await;
    assert!(matches!(
        rejection,
        DaemonFrame::RecipientActionResult {
            action: RecipientAction::Response,
            target: RecipientActionTarget::Node { node_id },
            outcome: RecipientActionOutcome::Applied { reply: None },
        } if node_id == node_r
    ));
    let deadline = tokio::time::Instant::now() + SETTLE;
    while resolved(&room_events(&d.journal).await).len() < 3 {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let turns = replies(&room_events(&d.journal).await);
    assert_eq!(
        turns.len(),
        2,
        "K07 Reject records no turn after Approve and Edit"
    );
    assert_eq!(turns[0].1.answers, ri_a);
    assert_eq!(turns[0].1.authorship, Authorship::AgentComposed);
    assert_eq!(turns[0].1.content, "automatic reply");
    attach_shutdown.cancel();

    // (4) An auto template is not a reply.
    let a = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndAuto).await;
    let mut sender = new_sender(vec![a.peer("b")]).await;
    let (_, task_t) = ask(&mut sender, &mut state, &a, "auto me").await;
    poll_state(&a.endpoint, &task_t, "completed").await;
    assert!(
        replies(&room_events(&a.journal).await).is_empty(),
        "K07 NotifyAndAuto template must not fabricate a reply turn"
    );

    // Unknown future composition is a render-only compatibility control, not
    // a fabricated journal act. The approved positive control above used HTTP.
    use rustain::adapters::a2a::send::{ServedThread, ServedTurn, ThreadRead};
    let unknown = ThreadRead::Read {
        thread: ServedThread {
            root: "ri_unknown".to_owned(),
            turns: vec![ServedTurn {
                turn_id: "rr_unknown".to_owned(),
                reply: true,
                task: None,
                in_reply_to: Some("ri_unknown".to_owned()),
                removed: false,
                content: Some("future composition answer".to_owned()),
                content_hash: None,
                kind: rustain::domain::models::ReplyKind::Answer,
                status: None,
                authorship: Authorship::Unknown,
                retracted_at_ms: None,
            }],
            omitted_before: 0,
            principal_collapsed: true,
            principal_bound: false,
        },
        mine: vec![false],
    };
    let rendered = rustain::adapters::tui::handlers::team_command::render_team_thread(
        "b",
        "ri_unknown",
        &unknown,
        false,
    );
    assert!(
        rendered.contains("future composition answer"),
        "K07 Unknown control must actually render its turn: {rendered}"
    );
    assert!(
        !rendered.contains("[agent-composed]") && !rendered.contains("[auto-sent]"),
        "K07 Unknown composition must render no provenance mark: {rendered}"
    );
}

/// `K04` ordering control (`M07`): with the AttachServer's journal refusing
/// `RecipientItemReplied`, a sending resolution writes NEITHER row and the
/// persisted card stays pending — the turn comes before everything else.
#[tokio::test]
async fn a_refused_turn_leaves_the_card_pending_and_writes_nothing() {
    let mut options = HostOptions::new(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait);
    options.attach_wrap = Some(Arc::new(|inner| {
        Arc::new(FailReplyJournal {
            inner,
            failing: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            kind: FailKind::Reply,
        }) as Arc<dyn RoomJournal>
    }));
    let mut b = Host::serve_with(options).await;
    let (mut operator, attach_shutdown) = attach_operator(&mut b).await;
    let mut sender = new_sender(vec![b.peer("b")]).await;
    let mut state = TuiState::new(120, 40);
    let (_, task) = ask(&mut sender, &mut state, &b, "question").await;
    let node = waiting_node(&b, &task).await;
    pending_card(&b, &node).await;
    let result = resolve(
        &mut operator,
        &node,
        PeerDraftAction::WriteOwn {
            content: "answer".to_owned(),
        },
    )
    .await;
    assert!(matches!(
        result,
        DaemonFrame::RecipientActionResult {
            action: RecipientAction::Response,
            target: RecipientActionTarget::Node { node_id: result_node },
            outcome: RecipientActionOutcome::Refused {
                error: ProtocolError::Internal(_),
            },
        } if result_node == node
    ));
    let rows = room_events(&b.journal).await;
    assert!(replies(&rows).is_empty());
    assert!(resolved(&rows).is_empty());
    assert_eq!(
        persisted_card(&b, &node).await.as_deref(),
        Some("[awaiting your response]")
    );
    attach_shutdown.cancel();
}

/// `K06` + `M09` (front-door half) — another principal's item, seeded as
/// that peer's EARLIER INGRESS under `credential-b` (the 19.16d technique;
/// ⛔ not a hand-appended turn), is byte-identical to a never-minted id on
/// the thread read AND as a follow-up reference; it never joins the caller's
/// thread even though it names the caller's root; a removed root is a
/// tombstone turn, distinct from not-found.
///
/// `M11` → RED: a distinct message for not-yours.
#[tokio::test]
async fn not_yours_is_byte_identical_to_never_existed() {
    let mut b = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndAuto).await;
    let (mut operator, attach_shutdown) = attach_operator(&mut b).await;
    let mut sender = new_sender(vec![b.peer("b")]).await;
    let mut state = TuiState::new(120, 40);
    let (ri_q, _) = ask(&mut sender, &mut state, &b, "question").await;
    let foreign = rustain::domain::models::ItemAddress::from_a2a_ingress(
        rustain::adapters::a2a::exec::SubmitterKey::from_api_key("credential-b")
            .pseudonymous_peer_id(),
        rustain::domain::models::ItemId::from_replay("ri_foreign"),
    );
    b.journal
        .room
        .record_event(RoomEvent::RecipientItemReceived {
            stories: None,
            message_type: rustain::domain::models::SemanticMessageType::Unknown,
            address: foreign,
            task: "their-task".to_owned(),
            alias: None,
            content: "someone else's words".to_owned(),
            in_reply_to: Some(ri_q.clone()),
            capability_hash: None,
            reply_capability_matched: false,
            authorship: rustain::domain::models::Authorship::Unknown,
            send_authorization: rustain::domain::models::SendAuthorization::Unknown,
        })
        .await
        .expect("seed another principal's earlier ingress");
    assert_eq!(
        served_ids(&thread_rpc(&b, &ri_q).await),
        vec![ri_q.clone()],
        "the foreign item names the root but never joins the caller's thread"
    );
    let not_yours = thread_rpc(&b, "ri_foreign").await;
    let never = thread_rpc(&b, "ri_never_minted").await;
    assert_eq!(not_yours, never);
    assert_eq!(not_yours["error"]["code"], -32001);
    assert_eq!(not_yours["error"]["message"], "Item not found");
    let carrier = |reference: &str, id: &str| {
        serde_json::json!({"jsonrpc":"2.0","id":9,"method":"message/send","params":{"message":{
            "kind":"message","messageId":id,"role":"user",
            "parts":[{"kind":"text","text":"follow"}],
            "metadata":{"x-rustain-in-reply-to":reference}}}})
    };
    let minted = received(&room_events(&b.journal).await).len();
    let followup_not_yours = rpc(&b.endpoint, carrier("ri_foreign", "m-1")).await;
    let followup_never = rpc(&b.endpoint, carrier("ri_never_minted", "m-1")).await;
    assert_eq!(followup_not_yours, followup_never);
    assert_eq!(followup_not_yours["error"]["code"], -32001);
    assert_eq!(
        received(&room_events(&b.journal).await).len(),
        minted,
        "nothing minted"
    );
    assert!(
        !room_events(&b.journal)
            .await
            .iter()
            .any(|(_, row)| serde_json::to_string(row).expect("row").contains("m-1")),
        "no row names the refused message"
    );

    // A removed root is a tombstone turn, not not-found.
    write_frame(
        &mut operator,
        &ClientFrame::RemoveRecipientItem {
            item_id: ri_q.clone(),
        },
    )
    .await
    .expect("remove");
    let deadline = tokio::time::Instant::now() + SETTLE;
    loop {
        let served = thread_rpc(&b, &ri_q).await;
        if served["result"]["turns"][0]["state"] == "removed" {
            assert!(served["result"]["turns"][0].get("content").is_none());
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "{served}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    attach_shutdown.cancel();
}

/// `K07` Ask path (E6) — under `ask` with an attached operator who approves,
/// the follow-up's link survives `start_task` → `setup_task` → the pending-
/// approval watcher → `launch`. Positive control: with nobody attached the
/// follow-up auto-declines and mints nothing.
#[tokio::test]
async fn an_approved_follow_up_keeps_its_link() {
    let mut b = Host::serve(A2aAdmissionPolicy::Ask, ResponseMode::NotifyAndAuto).await;
    let (mut operator, attach_shutdown) = attach_operator(&mut b).await;
    let send = |id: &str, reference: Option<&str>| {
        let mut metadata = serde_json::Map::new();
        if let Some(reference) = reference {
            metadata.insert("x-rustain-in-reply-to".to_owned(), reference.into());
        }
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"message/send","params":{"message":{
            "kind":"message","messageId":id,"role":"user",
            "parts":[{"kind":"text","text":"text"}],"metadata":metadata}}})
    };
    let first = rpc(&b.endpoint, send("ask-root", None)).await;
    assert_eq!(first["result"]["status"]["state"], "auth-required");
    approve_next(&mut operator).await;
    let deadline = tokio::time::Instant::now() + SETTLE;
    while received(&room_events(&b.journal).await).is_empty() {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let root = received(&room_events(&b.journal).await)[0].0.clone();
    rpc(&b.endpoint, send("ask-follow", Some(&root))).await;
    approve_next(&mut operator).await;
    let deadline = tokio::time::Instant::now() + SETTLE;
    loop {
        let links: Vec<Option<String>> = room_events(&b.journal)
            .await
            .into_iter()
            .filter_map(|(_, row)| match row {
                RoomEvent::RecipientItemReceived { in_reply_to, .. } => Some(in_reply_to),
                _ => None,
            })
            .collect();
        if links.len() == 2 {
            assert_eq!(links[1].as_deref(), Some(root.as_str()));
            break;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Story 19.20c: "nobody attached" is a detached writer, not a stopped
    // attach server — stopping the server now cancels every pending approval
    // and refuses new admissions (RuntimeShutdown). Detach and wait for the
    // daemon to close the connection, which follows its registry removal.
    write_frame(&mut operator, &ClientFrame::Detach)
        .await
        .expect("detach");
    while read_frame::<_, DaemonFrame>(&mut operator)
        .await
        .expect("frame")
        .is_some()
    {}
    drop(operator);

    // Positive control: nobody attached → the unattended approval declines
    // after its deferred first answer, and nothing is minted.
    rpc(&b.endpoint, send("ask-unattended", Some(&root))).await;
    let declined = poll_state(&b.endpoint, "ask-unattended", "rejected").await;
    assert_eq!(
        declined["result"]["status"]["state"], "rejected",
        "{declined}"
    );
    assert_eq!(received(&room_events(&b.journal).await).len(), 2);
    attach_shutdown.cancel();
}

/// Grant the next forwarded admission approval through the real frame.
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

/// `K08` — a bad reference mints nothing on B, and the asker's row is 19.17's
/// `no usable answer` (a JSON-RPC error), with only the dispatch row.
///
/// `M14` → RED: mint the item and ignore the bad reference.
#[tokio::test]
async fn a_bad_reference_mints_nothing() {
    let b = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut sender = new_sender(vec![b.peer("b")]).await;
    let mut state = TuiState::new(120, 40);
    sender
        .type_command(&mut state, "/team followup b ri_nope hi")
        .await;
    settle_send(&mut sender, &mut state).await;
    assert_eq!(
        state.feedback_blocks["team-send-1"].message,
        "    b  no usable answer — '/team board' shows whether it arrived"
    );
    assert!(room_events(&b.journal).await.is_empty(), "B wrote nothing");
    let rows = room_events(&sender.journal).await;
    assert_eq!(rows.len(), 1);
    assert!(matches!(
        rows[0].1,
        RoomEvent::RemoteEnvelopeDispatched { .. }
    ));
}

/// `K12` — the follow-up's `dispatched` row is durable BEFORE the POST
/// (19.17's `GateJournal` hold on B's append), then one `accepted` row.
///
/// `M21` → RED: a second send path that writes the dispatch after the POST.
#[tokio::test]
async fn a_follow_up_is_dispatched_durably_before_its_post() {
    let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mut options = HostOptions::new(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndAuto);
    options.serve_wrap = Some({
        let (armed, started, release) = (armed.clone(), started.clone(), release.clone());
        Arc::new(move |inner| {
            Arc::new(GateJournal {
                inner,
                armed: armed.clone(),
                started: started.clone(),
                release: release.clone(),
            }) as Arc<dyn RoomJournal>
        })
    });
    let b = Host::serve_with(options).await;
    let mut sender = new_sender(vec![b.peer("b")]).await;
    let mut state = TuiState::new(120, 40);
    let (ri_q, _) = ask(&mut sender, &mut state, &b, "question").await;
    let before = room_events(&sender.journal).await.len();
    armed.store(true, std::sync::atomic::Ordering::SeqCst);
    sender
        .type_command(&mut state, &format!("/team followup b {ri_q} more"))
        .await;
    tokio::time::timeout(SETTLE, started.notified())
        .await
        .expect("B began appending the follow-up");
    let held = room_events(&sender.journal).await;
    assert_eq!(
        held.len(),
        before + 1,
        "the dispatch row is durable while B is held"
    );
    assert!(matches!(
        held.last().map(|(_, row)| row),
        Some(RoomEvent::RemoteEnvelopeDispatched {
            content_hash: Some(_),
            ..
        })
    ));
    release.notify_one();
    settle_send(&mut sender, &mut state).await;
    let rows = room_events(&sender.journal).await;
    assert_eq!(rows.len(), before + 2);
    assert!(matches!(
        rows.last().map(|(_, row)| row),
        Some(RoomEvent::RemoteEnvelopeAccepted { .. })
    ));
}

/// `K13` — restart survival (`AD-1828`). (1) B restarts before answering;
/// the operator re-attaches and `/team reply` on the pre-restart item is
/// accepted; the asker reads it. (2) B restarts again; the asker reads the
/// same turns. (3) The ASKER restarts; the re-read serves the same turns and
/// re-journals nothing (the dedup is seeded from the journal).
///
/// `M22` → RED: reply turns kept in memory only — (2) loses the turn.
/// `M23` → RED: a process-local dedup with no durable seed — (3) journals twice.
#[tokio::test]
async fn threads_survive_restarts_on_both_hosts() {
    let b = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut sender = new_sender(vec![b.peer("b")]).await;
    let mut state = TuiState::new(120, 40);
    let (ri_q, task_q) = ask(&mut sender, &mut state, &b, "question").await;
    let node = waiting_node(&b, &task_q).await;
    pending_card(&b, &node).await;
    let mut b = b.restart().await;
    let (mut operator, attach_shutdown) = attach_operator(&mut b).await;
    let rr = frame_turn(&operator_reply(&mut operator, &ri_q, "after the restart").await);
    let first = read_thread_block(&mut sender, &mut state, "b", &ri_q).await;
    assert!(
        first.contains(&rr) && first.contains("after the restart"),
        "{first}"
    );
    attach_shutdown.cancel();
    drop(operator);

    let b = b.restart().await;
    let second = read_thread_block(&mut sender, &mut state, "b", &ri_q).await;
    assert_eq!(second, first, "B's restart changes nothing a reader sees");

    let rows_before = observed(&room_events(&sender.journal).await).len();
    assert_eq!(rows_before, 1);
    let workspace = sender.journal.workspace.clone();
    drop(sender);
    let mut restarted = sender_over(vec![b.peer("b")], open_journal(workspace).await).await;
    let third = read_thread_block(&mut restarted, &mut TuiState::new(120, 40), "b", &ri_q).await;
    assert_eq!(third, first);
    assert_eq!(
        observed(&room_events(&restarted.journal).await).len(),
        rows_before,
        "the asker's restart re-journals nothing"
    );
}

/// `K15` — an agent-composed draft naming the workspace root: `tasks/get`
/// withholds it (shipped), and the thread serves that turn `contentWithheld`
/// with no content; the asker renders the withheld wording. Positive
/// control: a clean body is served verbatim.
///
/// `M26` → RED: serve reply content raw.
#[tokio::test]
async fn a_reply_turn_passes_the_disclosure_scrub() {
    let mut options = HostOptions::new(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndDraft);
    options.draft_mentions_workspace = true;
    let mut b = Host::serve_with(options).await;
    let (mut operator, attach_shutdown) = attach_operator(&mut b).await;
    let mut sender = new_sender(vec![b.peer("b")]).await;
    let mut state = TuiState::new(120, 40);
    let (ri_q, task) = ask(&mut sender, &mut state, &b, "where is it?").await;
    let node = waiting_node(&b, &task).await;
    pending_card(&b, &node).await;
    let resolution = resolve(&mut operator, &node, PeerDraftAction::Approve).await;
    assert!(matches!(
        &resolution,
        DaemonFrame::RecipientActionResult {
            action: RecipientAction::Response,
            target: RecipientActionTarget::Node { node_id },
            outcome: RecipientActionOutcome::Applied {
                reply: Some(receipt),
            },
        } if node_id == &node && receipt.item_id.as_str() == ri_q.as_str()
    ));
    let completed = poll_state(&b.endpoint, &task, "completed").await;
    assert!(
        completed["result"]["status"]["message"]["parts"][0]["text"]
            .as_str()
            .is_some_and(|text| text.starts_with("host-bound-unavailable")),
        "{completed}"
    );
    let served = thread_rpc(&b, &ri_q).await;
    let turn = &served["result"]["turns"][1];
    assert_eq!(turn["kind"], "reply");
    assert_eq!(turn["contentWithheld"], true);
    assert!(turn.get("content").is_none(), "{turn}");
    let block = read_thread_block(&mut sender, &mut state, "b", &ri_q).await;
    assert!(block.contains(rustain::adapters::a2a::projection::HOST_BOUND_UNAVAILABLE_TEXT));
    assert!(!block.contains(&b.journal.path.display().to_string()));
    // Positive control: the item's own clean body is served verbatim.
    assert_eq!(served["result"]["turns"][0]["content"], "where is it?");
    attach_shutdown.cancel();
}

/// `K16` — bounded by bytes and by count, newest wins (`P9`, `Q8`).
/// Control-character bodies at `MAX_REPLY_BYTES` serialize ~6×, so with
/// production limits the thread exceeds the byte budget: the read serves the
/// root plus the newest turns that fit, states `omittedBefore`, and the
/// client reads it without `BodyTooLarge`. `MAX_THREAD_TURNS + 1` turns
/// serve the root plus the newest `MAX_THREAD_TURNS - 1`.
///
/// `M27` → RED: count-only cap (the read fails `BodyTooLarge`).
/// `M35` → RED: oldest-first (the newest turn is absent).
#[tokio::test]
async fn the_thread_read_is_bounded_and_the_newest_turns_win() {
    use rustain::domain::models::MAX_REPLY_BYTES;
    use rustain::domain::services::recipient_thread_bounds::MAX_THREAD_TURNS;

    let mut b = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndAuto).await;
    let (mut operator, attach_shutdown) = attach_operator(&mut b).await;
    let mut sender = new_sender(vec![b.peer("b")]).await;
    let mut state = TuiState::new(120, 40);
    let (ri_q, _) = ask(&mut sender, &mut state, &b, "question").await;
    let control = "\u{1}".repeat(MAX_REPLY_BYTES);
    let mut turns = Vec::new();
    for _ in 0..3 {
        turns.push(frame_turn(
            &operator_reply(&mut operator, &ri_q, &control).await,
        ));
    }
    let served = thread_rpc(&b, &ri_q).await;
    assert_eq!(
        served_ids(&served),
        vec![ri_q.clone(), turns[1].clone(), turns[2].clone()]
    );
    assert_eq!(served["result"]["omittedBefore"], 1);
    let block = read_thread_block(&mut sender, &mut state, "b", &ri_q).await;
    assert!(
        block.contains("  1 earlier turns not shown — the full thread is on b's host"),
        "{block}"
    );
    assert!(block.contains(&turns[2]));
    let recorded = replies(&room_events(&b.journal).await).len();
    let refused = operator_reply(&mut operator, &ri_q, &"x".repeat(MAX_REPLY_BYTES + 1)).await;
    assert!(matches!(
        &refused,
        DaemonFrame::RecipientActionResult {
            action: RecipientAction::Reply,
            target: RecipientActionTarget::Item { item_id },
            outcome: RecipientActionOutcome::Refused {
                error: ProtocolError::Malformed(message),
            },
        } if item_id == &ri_q && message.contains("reply is longer than")
    ));
    assert_eq!(replies(&room_events(&b.journal).await).len(), recorded);

    let (ri_c, _) = ask(&mut sender, &mut state, &b, "count me").await;
    let mut counted = Vec::new();
    for index in 0..MAX_THREAD_TURNS {
        counted.push(frame_turn(
            &operator_reply(&mut operator, &ri_c, &format!("r{index}")).await,
        ));
    }
    let served = thread_rpc(&b, &ri_c).await;
    let ids = served_ids(&served);
    assert_eq!(ids.len(), MAX_THREAD_TURNS);
    assert_eq!(ids[0], ri_c);
    assert_eq!(ids[1..], counted[1..]);
    assert_eq!(served["result"]["omittedBefore"], 1);
    attach_shutdown.cancel();
}

/// `K17` — one act, one turn. (1) `/team reply` on a pending card resolves
/// it: one turn, the card clears (the headline covers it; asserted here on
/// the retry host too). (2) After an injected `PeerDraftResolved` failure
/// the turn stands; a different text and a reject are refused; the same
/// answer replays the remaining steps with no second turn. (3) After a
/// restart, `/team reply` finds the card from the PERSISTED row. (4) After a
/// restart, `/team reply` then a card resolution: still one turn.
///
/// `M37` → RED: allow a second turn for a node. `M41` → RED: look up
/// `pending_drafts` only — (3) misses the card.
#[tokio::test]
async fn one_act_records_one_turn_across_retries_and_restarts() {
    let failing = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let mut options = HostOptions::new(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait);
    options.attach_wrap = Some({
        let failing = failing.clone();
        Arc::new(move |inner| {
            Arc::new(FailReplyJournal {
                inner,
                failing: failing.clone(),
                kind: FailKind::Resolved,
            }) as Arc<dyn RoomJournal>
        })
    });
    let mut b = Host::serve_with(options).await;
    let (mut operator, attach_shutdown) = attach_operator(&mut b).await;
    let mut sender = new_sender(vec![b.peer("b")]).await;
    let mut state = TuiState::new(120, 40);
    let (ri_q, task) = ask(&mut sender, &mut state, &b, "question").await;
    let node = waiting_node(&b, &task).await;
    pending_card(&b, &node).await;

    // (2) The turn lands, `PeerDraftResolved` fails.
    let frame = operator_reply(&mut operator, &ri_q, "the answer").await;
    assert!(
        matches!(
            &frame,
            DaemonFrame::RecipientActionResult {
                action: RecipientAction::Reply,
                target: RecipientActionTarget::Item { item_id },
                outcome: RecipientActionOutcome::Refused {
                    error: ProtocolError::Internal(_),
                },
            } if item_id == &ri_q
        ),
        "{frame:?}"
    );
    assert_eq!(replies(&room_events(&b.journal).await).len(), 1);
    failing.store(false, std::sync::atomic::Ordering::SeqCst);
    for action in [
        PeerDraftAction::WriteOwn {
            content: "a different answer".to_owned(),
        },
        PeerDraftAction::Edit {
            content: "an edit".to_owned(),
        },
        PeerDraftAction::Reject,
    ] {
        let result = resolve(&mut operator, &node, action).await;
        assert!(matches!(
            result,
            DaemonFrame::RecipientActionResult {
                action: RecipientAction::Response,
                target: RecipientActionTarget::Node { node_id },
                outcome: RecipientActionOutcome::Refused {
                    error: ProtocolError::Internal(_),
                },
            } if node_id == node
        ));
    }
    let retry = resolve(
        &mut operator,
        &node,
        PeerDraftAction::WriteOwn {
            content: "the answer".to_owned(),
        },
    )
    .await;
    assert!(matches!(
        &retry,
        DaemonFrame::RecipientActionResult {
            action: RecipientAction::Response,
            target: RecipientActionTarget::Node { node_id },
            outcome: RecipientActionOutcome::Applied {
                reply: Some(receipt),
            },
        } if node_id == &node && receipt.item_id.as_str() == ri_q.as_str()
    ));
    poll_state(&b.endpoint, &task, "completed").await;
    let rows = room_events(&b.journal).await;
    assert_eq!(replies(&rows).len(), 1, "no second turn");
    assert_eq!(resolved(&rows).len(), 1);
    attach_shutdown.cancel();
    drop(operator);

    // Restart keeps the received item, not a live controller for its old task.
    let (ri_p, task_p) = ask(&mut sender, &mut state, &b, "after restart").await;
    let node_p = waiting_node(&b, &task_p).await;
    pending_card(&b, &node_p).await;
    let mut b = b.restart().await;
    let (mut operator, attach_shutdown) = attach_operator(&mut b).await;
    let rr = frame_turn(&operator_reply(&mut operator, &ri_p, "found the card").await);
    let rows = room_events(&b.journal).await;
    let for_p: Vec<_> = replies(&rows)
        .into_iter()
        .filter(|(_, row)| row.answers == ri_p)
        .collect();
    assert_eq!(for_p.len(), 1);
    assert_eq!(for_p[0].1.turn, rr);
    let retry = resolve(
        &mut operator,
        &node_p,
        PeerDraftAction::WriteOwn {
            content: "resolved again".to_owned(),
        },
    )
    .await;
    assert!(matches!(
        retry,
        DaemonFrame::RecipientActionResult {
            action: RecipientAction::Response,
            target: RecipientActionTarget::Node { node_id },
            outcome: RecipientActionOutcome::Idempotent,
        } if node_id == node_p
    ));
    let rows = room_events(&b.journal).await;
    assert_eq!(
        replies(&rows)
            .into_iter()
            .filter(|(_, row)| row.answers == ri_p)
            .count(),
        1,
        "still one turn"
    );
    assert_eq!(
        resolved(&rows)
            .into_iter()
            .filter(|(_, node, _)| node == &node_p)
            .count(),
        0,
        "a direct human reply must not resurrect or resolve the pre-restart task"
    );
    attach_shutdown.cancel();
}

/// `K21` — thread access is off by default for API-key callers (`OQ1` = a),
/// on a REAL non-loopback listener (`0.0.0.0`, TLS + one API key), through the
/// HTTP front door. (1) Off: the thread read gets `-32040` with the fixed
/// message and B writes nothing. (2) Off: a follow-up with the carrier gets
/// `-32040`, mints nothing, writes zero rows. (3) `credential-scoped`: both
/// are served. Positive control: a loopback caller is served with the setting
/// off (every other keystone here).
///
/// `M44` → RED: gate after the lookup (an unknown id then answers `-32001`
/// ahead of the policy refusal). `M45` → RED: default to credential-scoped.
#[tokio::test]
async fn thread_access_is_off_by_default_for_api_key_callers() {
    use rustain::adapters::a2a::admission::ThreadReads;
    use rustain::adapters::a2a::auth::A2aServerAuth;

    // The setting reaches the host exactly as `run_inner` derives it: parsed
    // from `.rustain/a2a.json`, absent ⇒ the type's default (`M45`).
    for opt_in in [false, true] {
        let dir = tempfile::tempdir().expect("tls dir");
        let config_path = dir.path().join("a2a.json");
        std::fs::write(
            &config_path,
            if opt_in {
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
        let mut options = HostOptions::new(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndAuto);
        options.bind = "0.0.0.0:0";
        options.security = A2aServerSecurity {
            tls: Some(material),
            auth: Some(A2aServerAuth::ApiKey(
                rustain::adapters::a2a::auth::ApiKeySet::unbound(vec!["k21-key".into()]),
            )),
            thread_reads: setting,
        };
        let b = Host::serve_with(options).await;
        let port = b.listener.local_addr().expect("address").port();
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .expect("client");
        let call = |body: serde_json::Value| {
            let client = client.clone();
            async move {
                client
                    .post(format!("https://127.0.0.1:{port}/"))
                    .header(rustain::adapters::a2a::auth::API_KEY_HEADER, "k21-key")
                    .json(&body)
                    .send()
                    .await
                    .expect("post")
                    .json::<serde_json::Value>()
                    .await
                    .expect("json")
            }
        };
        let message = |id: &str, reference: Option<&str>| {
            let mut metadata = serde_json::Map::new();
            if let Some(reference) = reference {
                metadata.insert("x-rustain-in-reply-to".to_owned(), reference.into());
            }
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"message/send","params":{"message":{
                "kind":"message","messageId":id,"role":"user",
                "parts":[{"kind":"text","text":"text"}],"metadata":metadata}}})
        };
        let root = call(message("k21-root", None)).await;
        let item = root["result"]["metadata"]["x-rustain-item-id"]
            .as_str()
            .expect("an API-key caller's root item is minted as before")
            .to_owned();
        let minted = received(&room_events(&b.journal).await).len();
        let thread = call(serde_json::json!({"jsonrpc":"2.0","id":2,
            "method":"x-rustain-items/thread","params":{"itemId":item}}))
        .await;
        let unknown = call(serde_json::json!({"jsonrpc":"2.0","id":2,
            "method":"x-rustain-items/thread","params":{"itemId":"ri_never"}}))
        .await;
        let follow = call(message("k21-follow", Some(&item))).await;
        match opt_in {
            true => {
                assert_eq!(served_ids(&thread), vec![item.clone()], "{thread}");
                assert_eq!(thread["result"]["principalCollapsed"], false);
                assert!(
                    follow["result"]["metadata"]["x-rustain-item-id"].is_string(),
                    "{follow}"
                );
            }
            false => {
                for answer in [&thread, &unknown, &follow] {
                    assert_eq!(answer["error"]["code"], -32040, "{answer}");
                    assert_eq!(
                        answer["error"]["message"],
                        rustain::adapters::a2a::admission::THREAD_ACCESS_OFF
                    );
                }
                let rows = room_events(&b.journal).await;
                assert_eq!(received(&rows).len(), minted, "no item minted");
                assert!(
                    !rows.iter().any(|(_, row)| serde_json::to_string(row)
                        .expect("row")
                        .contains("k21-follow")),
                    "zero rows for the refused follow-up"
                );

                // The asker's half, through its front door over the anchored
                // roster entry: the ratified thread line, and a refused
                // follow-up is `✗ declined` (`AC12(d)`), minting nothing.
                // SAFETY: a test-unique variable name, set once, read by the
                // client at send time; no other test reads it.
                unsafe { std::env::set_var("RUSTAIN_19_18_K21_KEY", "k21-key") };
                let mut spec = A2aPeerSpec::new(
                    "b",
                    rustain::domain::models::RedactedUrl::from(format!(
                        "https://localhost:{port}/"
                    )),
                    A2aPeerSource::Workspace,
                );
                spec.auth = Some("RUSTAIN_19_18_K21_KEY".to_owned());
                spec.ca_cert = Some(cert_path.clone());
                let mut asker = new_sender(vec![spec]).await;
                let mut state = TuiState::new(120, 40);
                let block = read_thread_block(&mut asker, &mut state, "b", &item).await;
                assert_eq!(
                    block,
                    "thread access is off on b's host — its operator can enable it"
                );
                asker
                    .type_command(&mut state, &format!("/team followup b {item} more"))
                    .await;
                settle_send(&mut asker, &mut state).await;
                assert_eq!(
                    state.feedback_blocks["team-send-1"].message,
                    "  ✗ b  declined"
                );
                assert_eq!(received(&room_events(&b.journal).await).len(), minted);
            }
        }
    }
}

// ── Journal wrappers ────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum FailKind {
    Reply,
    Resolved,
}

/// Fails exactly one fact class while `failing` holds.
struct FailReplyJournal {
    inner: Arc<dyn RoomJournal>,
    failing: Arc<std::sync::atomic::AtomicBool>,
    kind: FailKind,
}

#[async_trait::async_trait]
impl RoomJournal for FailReplyJournal {
    async fn record_event(&self, event: RoomEvent) -> Result<(), RoomJournalError> {
        let hit = match self.kind {
            FailKind::Reply => matches!(event, RoomEvent::RecipientItemReplied { .. }),
            FailKind::Resolved => matches!(event, RoomEvent::PeerDraftResolved { .. }),
        };
        if hit && self.failing.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(RoomJournalError::Append("injected failure".to_owned()));
        }
        self.inner.record_event(event).await
    }
}

/// 19.17's hold (`conformance_19_17_a2a_fanout.rs:516-531`), armed on demand
/// so the first send passes through.
struct GateJournal {
    inner: Arc<dyn RoomJournal>,
    armed: Arc<std::sync::atomic::AtomicBool>,
    started: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait::async_trait]
impl RoomJournal for GateJournal {
    async fn record_event(&self, event: RoomEvent) -> Result<(), RoomJournalError> {
        if matches!(event, RoomEvent::RecipientItemReceived { .. })
            && self.armed.swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.started.notify_one();
            self.release.notified().await;
        }
        self.inner.record_event(event).await
    }
}
