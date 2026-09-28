//! Story 19.17 A2A fan-out keystones.
//!
//! These tests enter through the operator's rail: `submit_message_for_test` →
//! `team_command` → the spawned delivery → `AppEvent::TeamSendSettled` → the
//! production `team_send_settled` handler. Recipients are real loopback
//! `serve()` hosts with the production `AttachServer` runtime. The host
//! composition below is deliberately copied from `a2a_server_exec.rs:436-547`;
//! no host uses `runtime: None` and no operator is attached.

#![cfg(feature = "a2a")]

use std::path::Path;
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
use rustain::adapters::daemon::runtime::{DaemonCore, DaemonTurnRuntime};
use rustain::adapters::daemon::server::AttachServer;
use rustain::adapters::filesystem::FileSystemStorage;
use rustain::adapters::noop::{
    NoOpApprovalPersistence, NoOpMemory, NoOpPersona, NoOpSecurity, NoOpToolSet, NoOpUsageLedger,
};
use rustain::adapters::rap::IdentityKeyStore;
use rustain::adapters::tui::app::{InputAction, submit_message_for_test};
const TEAM_SEND_BLOCK_ID: &str = "team-send-1";
use rustain::adapters::tui::state::TuiState;
use rustain::domain::errors::ProviderError;
use rustain::domain::events::AppEvent;
use rustain::domain::models::capability_registry::CapabilityRegistry;
use rustain::domain::models::{
    A2aPeerSource, A2aPeerSpec, AppConfig, CompletionOptions, Conversation, DispatchAct,
    FeedbackLevel, JournalRecord, Message, ResponseMode, RoomEvent, StopReason, StreamChunk,
};
use rustain::domain::ports::{
    DeliveryPolicy, RoomJournal, RoomJournalError, RoomJournalReader, SecurityPort, StoragePort,
    StreamingProvider, ToolSetPort,
};
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::infrastructure::runtime::app_state::AppState;
use rustain::infrastructure::runtime::transparency_bridge::{team_command, team_send_settled};
use rustain::infrastructure::subagent::{NodeJournal, NodeRoomJournal, NodeTree};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const SETTLE: Duration = Duration::from_secs(45);

struct Journal {
    _workspace: tempfile::TempDir,
    path: std::path::PathBuf,
    node: Arc<NodeJournal>,
    room: Arc<dyn RoomJournal>,
}

async fn journal() -> Journal {
    let workspace = tempfile::tempdir().expect("workspace");
    let node = Arc::new(
        NodeJournal::open_workspace(workspace.path())
            .await
            .expect("open node journal"),
    );
    let room: Arc<dyn RoomJournal> = Arc::new(NodeRoomJournal::new(node.clone(), None));
    Journal {
        path: workspace.path().to_path_buf(),
        _workspace: workspace,
        node,
        room,
    }
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
        _peer_id: &rustain::domain::models::PeerId,
        _message_type: rustain::domain::models::SemanticMessageType,
    ) -> rustain::domain::ports::PeerResponsePolicy {
        rustain::domain::ports::PeerResponsePolicy {
            mode: self.0,
            auto_response: None,
            ..Default::default()
        }
    }
}

struct ScriptedProvider;

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
                content: "automatic reply".to_owned(),
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
        "fanout-scripted".to_owned()
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

/// A real served recipient with the same AttachServer composition as
/// `tests/a2a_server_exec.rs:harness_full_mode` (lines 436-547).
struct Host {
    journal: Journal,
    endpoint: String,
    cancel: CancellationToken,
    http: tokio::task::JoinHandle<anyhow::Result<()>>,
    _keys: tempfile::TempDir,
}

impl Host {
    async fn serve(policy: A2aAdmissionPolicy, mode: ResponseMode) -> Self {
        let host = journal().await;
        let sink = host.room.clone();
        Self::serve_with(host, sink, policy, mode).await
    }

    async fn serve_with(
        journal: Journal,
        sink: Arc<dyn RoomJournal>,
        policy: A2aAdmissionPolicy,
        mode: ResponseMode,
    ) -> Self {
        let keys = tempfile::tempdir().expect("identity directory");
        let (domain_tx, _domain_rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
        let node_tree = NodeTree::with_event_tx(
            domain_tx.clone(),
            Arc::new(|| chrono::Utc::now().timestamp_millis()),
        )
        .with_journal(journal.node.clone());
        let storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
            rustain::infrastructure::paths::sessions_dir(&journal.path),
            journal.path.clone(),
        ));
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
                        Arc::new(ScriptedProvider),
                        storage.clone(),
                        &workspace,
                    ))
                }),
            ))
        };
        let conversation = Arc::new(tokio::sync::Mutex::new(Conversation {
            id: "fanout-recipient".to_owned(),
            ..Conversation::default()
        }));
        let delivery: Arc<dyn DeliveryPolicy> = Arc::new(ModePolicy(mode));
        let bus = rustain::adapters::daemon::server::peer_bus_slot_with_policy(
            &node_tree,
            delivery.clone(),
        );
        let server = AttachServer::new_with_node_tree_bus_policy_and_journal(
            core,
            conversation,
            domain_tx,
            node_tree,
            bus,
            delivery,
            journal.room.clone(),
            journal.node.clone(),
            None,
        );
        let signer = IdentityKeyStore::new(keys.path())
            .load_or_generate()
            .expect("identity");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let endpoint = format!("http://{}/", listener.local_addr().expect("address"));
        let cancel = CancellationToken::new();
        let http = tokio::spawn(serve(
            listener,
            ServeConfig {
                registry: Arc::new(CapabilityRegistry::new(None)),
                signer,
                security: A2aServerSecurity::default(),
                runtime: Some(server as Arc<dyn rustain::domain::ports::InboundPeerRuntime>),
                transparency: Arc::new(TransparencySink::new(sink)),
                policy,
                workspace: journal.path.clone(),
                advertised_host: None,
                cards: Arc::new(SignedCardCache::new()),
            },
            cancel.child_token(),
        ));
        Self {
            journal,
            endpoint,
            cancel,
            http,
            _keys: keys,
        }
    }

    fn peer(&self, alias: &str) -> A2aPeerSpec {
        A2aPeerSpec::new(
            alias,
            rustain::domain::models::RedactedUrl::from(self.endpoint.clone()),
            A2aPeerSource::Workspace,
        )
    }

    async fn stop(self) {
        self.cancel.cancel();
        self.http
            .await
            .expect("server task")
            .expect("server shutdown");
    }
}

struct Sender {
    journal: Journal,
    tree: NodeTree,
    app_state: AppState,
    domain_rx: tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
}

async fn sender(peers: Vec<A2aPeerSpec>) -> Sender {
    let journal = journal().await;
    let sink = journal.room.clone();
    sender_with_journal(peers, sink, journal).await
}

async fn sender_with_journal(
    peers: Vec<A2aPeerSpec>,
    sink: Arc<dyn RoomJournal>,
    journal: Journal,
) -> Sender {
    let (egress_tx, _egress_rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    let tree = NodeTree::new();
    let egress = A2aEgress::compose(peers, tree.clone(), sink, journal.node.clone(), egress_tx)
        .expect("compose production sender egress");
    tokio::time::timeout(SETTLE, egress.await_cards_settled())
        .await
        .expect("sender card fetches settle");
    let (mut app_state, domain_rx) = app_state(&journal.path);
    app_state.a2a_send = Some(egress.runtime().clone());
    Sender {
        journal,
        tree,
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

    async fn rows(&self) -> Vec<rustain::domain::models::JournalEntry> {
        RoomJournalReader::load_entries(self.journal.node.as_ref())
            .await
            .expect("read sender durable journal")
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

async fn settle(sender: &mut Sender, state: &mut TuiState, expected: usize) {
    let mut tabs = rustain::domain::models::tab::TabManager::new(CancellationToken::new());
    for _ in 0..expected {
        let event = tokio::time::timeout(SETTLE, sender.domain_rx.recv())
            .await
            .expect("delivery settled before deadline")
            .expect("domain event channel remains open");
        let AppEvent::TeamSendSettled {
            conversation_id,
            block_id,
            index,
            outcome,
        } = event
        else {
            panic!("the send action must publish TeamSendSettled, got {event:?}");
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

fn send_rows(rows: &[(u64, RoomEvent)]) -> Vec<(u64, String, Option<String>)> {
    rows.iter()
        .filter_map(|(seq, row)| match row {
            RoomEvent::RemoteEnvelopeDispatched {
                peer,
                task,
                act: DispatchAct::Task,
                ..
            } => Some((*seq, peer.to_string(), task.clone())),
            _ => None,
        })
        .collect()
}

fn rejected(rows: &[(u64, RoomEvent)]) -> usize {
    rows.iter()
        .filter(|(_, row)| matches!(row, RoomEvent::RemoteEnvelopeRejected { .. }))
        .count()
}

fn received_tasks(rows: &[(u64, RoomEvent)]) -> Vec<String> {
    rows.iter()
        .filter_map(|(_, row)| match row {
            RoomEvent::RecipientItemReceived { task, .. } => Some(task.clone()),
            _ => None,
        })
        .collect()
}

#[async_trait::async_trait]
impl RoomJournal for GateJournal {
    async fn record_event(&self, event: RoomEvent) -> Result<(), RoomJournalError> {
        if matches!(event, RoomEvent::RecipientItemReceived { .. }) {
            self.started.notify_one();
            self.release.notified().await;
        }
        self.inner.record_event(event).await
    }
}

struct GateJournal {
    inner: Arc<dyn RoomJournal>,
    started: Arc<Notify>,
    release: Arc<Notify>,
}

struct FailAcceptedJournal {
    inner: Arc<dyn RoomJournal>,
}

#[async_trait::async_trait]
impl RoomJournal for FailAcceptedJournal {
    async fn record_event(&self, event: RoomEvent) -> Result<(), RoomJournalError> {
        if matches!(event, RoomEvent::RemoteEnvelopeAccepted { .. }) {
            return Err(RoomJournalError::Append("accepted row failure".to_owned()));
        }
        self.inner.record_event(event).await
    }
}

struct FailDispatchJournal {
    inner: Arc<dyn RoomJournal>,
}

#[async_trait::async_trait]
impl RoomJournal for FailDispatchJournal {
    async fn record_event(&self, event: RoomEvent) -> Result<(), RoomJournalError> {
        if matches!(
            event,
            RoomEvent::RemoteEnvelopeDispatched {
                act: DispatchAct::Task,
                ..
            }
        ) {
            return Err(RoomJournalError::Append("dispatch row failure".to_owned()));
        }
        self.inner.record_event(event).await
    }
}

/// K02: validation is one whole action. An unknown alias cannot leak a partial
/// POST or a sender row to the known recipient.
#[tokio::test]
async fn unknown_recipient_refuses_the_entire_front_door_action_before_io() {
    let a = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut sender = sender(vec![a.peer("a")]).await;
    let mut state = TuiState::new(120, 40);

    sender
        .type_command(&mut state, "/team send a,zz hello")
        .await;

    let block = state
        .feedback_blocks
        .get(TEAM_SEND_BLOCK_ID)
        .expect("one action block");
    assert_eq!(block.level, FeedbackLevel::Info);
    assert!(block.message.contains("zz"));
    assert!(block.message.contains("a"));
    assert!(
        room_events(&a.journal).await.is_empty(),
        "known host received no partial POST"
    );
    assert!(
        sender.rows().await.is_empty(),
        "invalid action made no sender journal row"
    );
    assert!(
        sender.domain_rx.try_recv().is_err(),
        "invalid action spawned no delivery"
    );
    a.stop().await;
}

/// K03 + K07: a single typed action reaches each real host independently. The
/// default waiting recipient is delivered at its first answer and remains
/// `auth-required`; send never delegates or cancels it.
#[tokio::test]
async fn first_answers_render_per_recipient_and_waiting_tasks_remain_owned_by_recipients() {
    let a = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let b = Host::serve(A2aAdmissionPolicy::Deny, ResponseMode::NotifyAndWait).await;
    let c = Host::serve(A2aAdmissionPolicy::Ask, ResponseMode::NotifyAndWait).await;
    let d = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndAuto).await;
    let f = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut missing_credential = f.peer("f");
    missing_credential.auth = Some("RUSTAIN_19_17_INTENTIONALLY_UNSET".to_owned());
    let g = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let uncached = g.peer("g");
    let g_identity = uncached.resolved_identity().to_string();
    g.stop().await;
    let h_journal = journal().await;
    let h_sink: Arc<dyn RoomJournal> = Arc::new(FailAcceptedJournal {
        inner: h_journal.room.clone(),
    });
    let h = Host::serve_with(
        h_journal,
        h_sink,
        A2aAdmissionPolicy::Allow,
        ResponseMode::NotifyAndWait,
    )
    .await;
    let mut sender = sender(vec![
        a.peer("a"),
        b.peer("b"),
        c.peer("c"),
        d.peer("d"),
        missing_credential,
        uncached,
        h.peer("h"),
    ])
    .await;
    let mut state = TuiState::new(120, 40);

    sender
        .type_command(&mut state, "/team send a,b,c,d,f,g,h hello")
        .await;
    settle(&mut sender, &mut state, 7).await;

    let block = state
        .feedback_blocks
        .get(TEAM_SEND_BLOCK_ID)
        .expect("single outcome block");
    assert_eq!(block.level, FeedbackLevel::Info);
    assert_eq!(block.actions.len(), 0);
    assert_eq!(
        block.message,
        concat!(
            "  ● a  delivered\n",
            "  ✗ b  declined\n",
            "    c  awaiting their approval\n",
            "  ● d  delivered\n",
            "  ⚠ f  unreachable\n",
            "    no credential for f: set the env var named in its auth field\n",
            "  ⚠ g  unreachable\n",
            "    peer `g` is configured but its AgentCard is not cached — discovery runs once at startup and may still be in flight, or the peer was unreachable when this session started. If it stays refused, restart the daemon with the peer up; on-demand discovery is `DF-18-9-CARD-REFRESH`.\n",
            "    h  no usable answer — '/team board' shows whether it arrived"
        )
    );
    assert!(!block.message.contains("cancelled"));
    assert!(!block.message.contains("7/"));

    let a_tasks = received_tasks(&room_events(&a.journal).await);
    assert_eq!(
        a_tasks.len(),
        1,
        "waiting host durably received exactly one item"
    );
    let response = reqwest::Client::new()
        .post(&a.endpoint)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tasks/get", "params": { "id": a_tasks[0] }
        }))
        .send()
        .await
        .expect("tasks/get request")
        .json::<serde_json::Value>()
        .await
        .expect("tasks/get JSON");
    assert_eq!(response["result"]["status"]["state"], "auth-required");
    let c_events = room_events(&c.journal).await;
    assert!(
        c_events
            .iter()
            .any(|(_, event)| matches!(event, RoomEvent::AdmissionDeferred { .. })),
        "Ask enters recipient admission before its unattended approval gate decides: {c_events:?}"
    );
    assert!(
        c_events.iter().all(|(_, event)| !matches!(
            event,
            RoomEvent::RemoteEnvelopeRejected {
                reason: rustain::domain::models::RejectReason::Policy { detail },
                ..
            } if detail == rustain::adapters::a2a::exec::CANCEL_DETAIL
        )),
        "the sender must not issue tasks/cancel for Ask; an unattended host can decline its own pending approval: {c_events:?}"
    );
    assert_eq!(
        received_tasks(&room_events(&h.journal).await).len(),
        1,
        "accepted append failed only after H received its item"
    );
    assert!(
        sender.tree.list().await.is_empty(),
        "first-answer delivery must not materialize an a2a-peer node in the sender's Agents panel"
    );
    let rows = room_events(&sender.journal).await;
    let dispatches = send_rows(&rows);
    assert_eq!(
        dispatches.len(),
        5,
        "one independently journaled dispatch per recipient"
    );
    assert_eq!(
        dispatches
            .iter()
            .map(|(_, _, task)| task.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        5,
        "a fan-out has one distinct task correlation per addressed peer"
    );
    let addressed = [
        a.peer("a").resolved_identity().to_string(),
        b.peer("b").resolved_identity().to_string(),
        c.peer("c").resolved_identity().to_string(),
        d.peer("d").resolved_identity().to_string(),
        f.peer("f").resolved_identity().to_string(),
        g_identity,
        h.peer("h").resolved_identity().to_string(),
    ];
    for (_, event) in &rows {
        let peer = match event {
            RoomEvent::RemoteEnvelopeDispatched { peer, .. }
            | RoomEvent::RemoteEnvelopeAccepted { peer, .. }
            | RoomEvent::RemoteEnvelopeRejected { peer, .. } => Some(peer.to_string()),
            _ => None,
        };
        if let Some(peer) = peer {
            assert!(
                addressed.contains(&peer),
                "no group or stray-peer journal row"
            );
        }
    }
    assert_eq!(
        rejected(&rows),
        3,
        "deny and both pre-flight refusals are the only proven refusals"
    );
    a.stop().await;
    b.stop().await;
    c.stop().await;
    d.stop().await;
    f.stop().await;
    h.stop().await;
}

/// K04 + K08 + K11: gates are inside the recipient's real durable inbound
/// append, proving both sends are in flight while the block is still pending;
/// reverse completion cannot reorder the operator's rows.
#[tokio::test]
async fn concurrent_real_hosts_settle_rows_in_operator_order_after_pre_render() {
    let a_journal = journal().await;
    let a_started = Arc::new(Notify::new());
    let a_release = Arc::new(Notify::new());
    let a_sink: Arc<dyn RoomJournal> = Arc::new(GateJournal {
        inner: a_journal.room.clone(),
        started: a_started.clone(),
        release: a_release.clone(),
    });
    let a = Host::serve_with(
        a_journal,
        a_sink,
        A2aAdmissionPolicy::Allow,
        ResponseMode::NotifyAndWait,
    )
    .await;
    let b_journal = journal().await;
    let b_started = Arc::new(Notify::new());
    let b_release = Arc::new(Notify::new());
    let b_sink: Arc<dyn RoomJournal> = Arc::new(GateJournal {
        inner: b_journal.room.clone(),
        started: b_started.clone(),
        release: b_release.clone(),
    });
    let b = Host::serve_with(
        b_journal,
        b_sink,
        A2aAdmissionPolicy::Allow,
        ResponseMode::NotifyAndWait,
    )
    .await;
    let mut sender = sender(vec![a.peer("a"), b.peer("b")]).await;
    let mut state = TuiState::new(120, 40);

    sender
        .type_command(&mut state, "/team send a,b hello")
        .await;
    let pending = state
        .feedback_blocks
        .get(TEAM_SEND_BLOCK_ID)
        .expect("block pre-rendered before IO");
    assert_eq!(pending.level, FeedbackLevel::Info);
    assert_eq!(pending.message, "    a  sending…\n    b  sending…");
    assert_eq!(
        state.feedback_blocks.len(),
        1,
        "one action owns one persistent Info block while all recipients wait"
    );
    tokio::time::timeout(SETTLE, a_started.notified())
        .await
        .expect("A inbound append held");
    tokio::time::timeout(SETTLE, b_started.notified())
        .await
        .expect("B inbound append held");
    let sender_rows = room_events(&sender.journal).await;
    assert_eq!(
        send_rows(&sender_rows).len(),
        2,
        "dispatches are durable before either POST returns"
    );

    b_release.notify_one();
    settle(&mut sender, &mut state, 1).await;
    assert_eq!(
        state
            .feedback_blocks
            .get(TEAM_SEND_BLOCK_ID)
            .unwrap()
            .message,
        "    a  sending…\n  ● b  delivered"
    );
    a_release.notify_one();
    settle(&mut sender, &mut state, 1).await;
    assert_eq!(
        state
            .feedback_blocks
            .get(TEAM_SEND_BLOCK_ID)
            .unwrap()
            .message,
        "  ● a  delivered\n  ● b  delivered"
    );
    assert_eq!(
        state.feedback_blocks.len(),
        1,
        "settlements replace rows in that same block, never stack notices"
    );
    let text = &state
        .feedback_blocks
        .get(TEAM_SEND_BLOCK_ID)
        .unwrap()
        .message;
    for aggregate in [
        "N/M",
        "N of M",
        "%",
        "all",
        "the team",
        "partial",
        "recipients",
    ] {
        assert!(
            !text.contains(aggregate),
            "outcome block has no aggregate {aggregate:?}"
        );
    }
    a.stop().await;
    b.stop().await;
}

/// K12: stopping the session while the recipient is still inside its durable
/// append must not publish a late settlement into the now-closed conversation.
#[tokio::test]
async fn a_cancelled_session_publishes_no_late_send_settlement() {
    let recipient_journal = journal().await;
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let sink: Arc<dyn RoomJournal> = Arc::new(GateJournal {
        inner: recipient_journal.room.clone(),
        started: started.clone(),
        release: release.clone(),
    });
    let recipient = Host::serve_with(
        recipient_journal,
        sink,
        A2aAdmissionPolicy::Allow,
        ResponseMode::NotifyAndWait,
    )
    .await;
    let mut sender = sender(vec![recipient.peer("a")]).await;
    let mut state = TuiState::new(120, 40);
    sender.type_command(&mut state, "/team send a hello").await;
    tokio::time::timeout(SETTLE, started.notified())
        .await
        .expect("recipient blocked after the POST started");
    let Sender {
        app_state,
        mut domain_rx,
        ..
    } = sender;
    app_state.session_cancel.cancel();
    release.notify_one();
    drop(app_state);
    assert!(
        tokio::time::timeout(SETTLE, domain_rx.recv())
            .await
            .expect("the spawned action releases its bus after cancellation")
            .is_none(),
        "a canceled action must publish no late TeamSendSettled event"
    );
    assert_eq!(
        state.feedback_blocks[TEAM_SEND_BLOCK_ID].message,
        "    a  sending…"
    );
    recipient.stop().await;
}

/// K05: a card cached at composition time does not authorize a retry when its
/// real host disappears. Re-typing is the only second attempt.
#[tokio::test]
async fn a_closed_real_host_is_attempted_once_and_a_new_action_is_distinct() {
    let e = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let peer = e.peer("e");
    let mut sender = sender(vec![peer]).await;
    e.stop().await;
    let mut state = TuiState::new(120, 40);

    sender.type_command(&mut state, "/team send e hello").await;
    settle(&mut sender, &mut state, 1).await;
    assert_eq!(
        state
            .feedback_blocks
            .get(TEAM_SEND_BLOCK_ID)
            .unwrap()
            .message,
        "  ⚠ e  unreachable"
    );
    let first = room_events(&sender.journal).await;
    assert_eq!(send_rows(&first).len(), 1);
    assert_eq!(rejected(&first), 1);

    sender.type_command(&mut state, "/team send e hello").await;
    settle(&mut sender, &mut state, 1).await;
    let second = room_events(&sender.journal).await;
    let dispatches = send_rows(&second);
    assert_eq!(
        dispatches.len(),
        2,
        "only an explicit second command retries"
    );
    assert_ne!(
        dispatches[0].2, dispatches[1].2,
        "each action has a distinct message id"
    );
}

/// K09 + K10 + K14: pre-flight failures record a refusal but no false dispatch;
/// a dispatch append failure fails closed before the real recipient sees a POST.
#[tokio::test]
async fn per_recipient_journals_follow_the_iff_rule_and_dispatch_failure_fails_closed() {
    let a = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let f = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    // This was a real AttachServer host; stopping it before composition leaves
    // its slot uncached, so the send path reaches the actual pre-I/O branch.
    let g = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let uncached = g.peer("g");
    g.stop().await;
    let mut missing_credential = f.peer("f");
    missing_credential.auth = Some("RUSTAIN_19_17_INTENTIONALLY_UNSET".to_owned());
    let mut sender = sender(vec![a.peer("a"), missing_credential, uncached]).await;
    let mut state = TuiState::new(120, 40);
    sender
        .type_command(&mut state, "/team send a,f,g hello")
        .await;
    settle(&mut sender, &mut state, 3).await;
    let rendered = &state
        .feedback_blocks
        .get(TEAM_SEND_BLOCK_ID)
        .unwrap()
        .message;
    assert!(rendered.contains("  ● a  delivered"));
    assert!(rendered.contains(
        "  ⚠ f  unreachable\n    no credential for f: set the env var named in its auth field"
    ));
    assert!(rendered.contains("  ⚠ g  unreachable"));
    assert!(rendered.contains("peer `g` is configured but its AgentCard is not cached"));
    let rows = room_events(&sender.journal).await;
    let dispatches = send_rows(&rows);
    assert_eq!(
        dispatches.len(),
        1,
        "pre-flight checks run before the dispatch hook"
    );
    assert_eq!(dispatches[0].1, a.peer("a").resolved_identity().to_string());
    assert_eq!(
        rejected(&rows),
        2,
        "each roster pre-flight refusal is durable exactly once"
    );
    assert!(
        room_events(&f.journal).await.is_empty(),
        "credential failure made no POST"
    );

    let fail_sender_journal = journal().await;
    let fail_sink: Arc<dyn RoomJournal> = Arc::new(FailDispatchJournal {
        inner: fail_sender_journal.room.clone(),
    });
    let mut failed_sender =
        sender_with_journal(vec![a.peer("a")], fail_sink, fail_sender_journal).await;
    let mut failed_state = TuiState::new(120, 40);
    failed_sender
        .type_command(&mut failed_state, "/team send a hello again")
        .await;
    settle(&mut failed_sender, &mut failed_state, 1).await;
    assert_eq!(
        failed_state
            .feedback_blocks
            .get(TEAM_SEND_BLOCK_ID)
            .unwrap()
            .message,
        "    a  not sent — this host could not record the attempt"
    );
    assert_eq!(
        received_tasks(&room_events(&a.journal).await).len(),
        1,
        "failed dispatch hook issued no second POST to the real recipient"
    );
    a.stop().await;
    f.stop().await;
}
