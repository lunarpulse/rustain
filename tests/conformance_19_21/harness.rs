use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use futures::stream::BoxStream;
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::a2a::auth::A2aServerSecurity;
use rustain::adapters::a2a::card_cache::SignedCardCache;
use rustain::adapters::a2a::egress::A2aEgress;
use rustain::adapters::a2a::exec::SubmitterKey;
use rustain::adapters::a2a::server::{ServeConfig, serve};
use rustain::adapters::a2a::transparency::TransparencySink;
use rustain::adapters::daemon::runtime::{DaemonCore, DaemonTurnRuntime};
use rustain::adapters::filesystem::FileSystemStorage;
use rustain::adapters::noop::{
    NoOpApprovalPersistence, NoOpMemory, NoOpPersona, NoOpSecurity, NoOpToolSet, NoOpUsageLedger,
};
use rustain::adapters::rap::IdentityKeyStore;
use rustain::adapters::tui::app::{InputAction, handle_input, submit_message_for_test};
use rustain::adapters::tui::handlers::team_command::{
    open_team_retract_card, resolve_team_retract_card,
};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::errors::ProviderError;
use rustain::domain::events::{AppEvent, DomainInputEvent};
use rustain::domain::models::capability_registry::CapabilityRegistry;
use rustain::domain::models::{
    A2aPeerSource, A2aPeerSpec, AppConfig, CompletionOptions, Conversation, ItemAddress, ItemId,
    JournalEntry, JournalRecord, Message, ResponseMode, RoomEvent, SemanticMessageType, StopReason,
    StreamChunk,
};
use rustain::domain::ports::{
    DeliveryPolicy, ProbeOutcome, RoomJournal, RoomJournalError, RoomJournalReader, SecurityPort,
    StoragePort, StreamingProvider, ToolSetPort,
};
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::domain::services::send_action_ledger::{SendActionView, fold_send_actions};
use rustain::infrastructure::runtime::app_state::AppState;
use rustain::infrastructure::runtime::transparency_bridge::{
    team_command, team_retract_dispatch, team_send_settled,
};
use rustain::infrastructure::subagent::{NodeJournal, NodeRoomJournal, NodeTree};
use tokio_util::sync::CancellationToken;

const SETTLE: Duration = Duration::from_secs(45);

/// A real workspace journal and its production node/room ports.
#[derive(Clone)]
pub(crate) struct Journal {
    pub(crate) workspace: Arc<tempfile::TempDir>,
    pub(crate) node: Arc<NodeJournal>,
    pub(crate) room: Arc<dyn RoomJournal>,
}

impl Journal {
    pub(crate) async fn open() -> Self {
        let workspace = Arc::new(tempfile::tempdir().expect("workspace"));
        let node = Arc::new(
            NodeJournal::open_workspace(workspace.path())
                .await
                .expect("open real node journal"),
        );
        let room: Arc<dyn RoomJournal> = Arc::new(NodeRoomJournal::new(node.clone(), None));
        Self {
            workspace,
            node,
            room,
        }
    }
    pub(crate) async fn open_with_clock(clock: Arc<rustain::domain::clock::MockClock>) -> Self {
        let workspace = Arc::new(tempfile::tempdir().expect("workspace"));
        let node = Arc::new(
            NodeJournal::open_workspace(workspace.path())
                .await
                .expect("open real node journal")
                .with_clock(clock),
        );
        let room: Arc<dyn RoomJournal> = Arc::new(NodeRoomJournal::new(node.clone(), None));
        Self {
            workspace,
            node,
            room,
        }
    }
    pub(crate) async fn reopen(&self) -> Self {
        let node = Arc::new(
            NodeJournal::open_workspace(self.workspace.path())
                .await
                .expect("reopen journal for a fresh runtime"),
        );
        let room: Arc<dyn RoomJournal> = Arc::new(NodeRoomJournal::new(node.clone(), None));
        Self {
            workspace: self.workspace.clone(),
            node,
            room,
        }
    }

    pub(crate) async fn entries(&self) -> Vec<JournalEntry> {
        RoomJournalReader::load_entries(self.node.as_ref())
            .await
            .expect("read sender journal")
    }
}

/// A durable journal decorator that can reject the selected event kind on a
/// selected occurrence while forwarding every other append unchanged.
pub(crate) struct RecordingRoomJournal {
    pub(crate) inner: Arc<dyn RoomJournal>,
    pub(crate) reject: Option<RejectAppend>,
    pub(crate) observed: parking_lot::Mutex<Vec<RoomEvent>>,
    pub(crate) seen_event: tokio::sync::Notify,
}

impl RecordingRoomJournal {
    pub(crate) fn new(inner: Arc<dyn RoomJournal>, reject: Option<RejectAppend>) -> Self {
        Self {
            inner,
            reject,
            observed: parking_lot::Mutex::new(Vec::new()),
            seen_event: tokio::sync::Notify::new(),
        }
    }
    pub(crate) async fn wait_for(&self, matches: fn(&RoomEvent) -> bool) {
        loop {
            let notified = self.seen_event.notified();
            if self.observed.lock().iter().any(matches) {
                return;
            }
            notified.await;
        }
    }
}

pub(crate) struct RejectAppend {
    pub(crate) matches: fn(&RoomEvent) -> bool,
    pub(crate) occurrence: usize,
    seen: std::sync::atomic::AtomicUsize,
}

impl RejectAppend {
    pub(crate) fn new(matches: fn(&RoomEvent) -> bool, occurrence: usize) -> Self {
        Self {
            matches,
            occurrence,
            seen: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl RoomJournal for RecordingRoomJournal {
    async fn record_event(&self, event: RoomEvent) -> Result<(), RoomJournalError> {
        self.observed.lock().push(event.clone());
        self.seen_event.notify_waiters();
        if let Some(reject) = &self.reject {
            if (reject.matches)(&event)
                && reject
                    .seen
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    == reject.occurrence
            {
                return Err(RoomJournalError::Append(
                    "injected targeted append refusal".to_owned(),
                ));
            }
        }
        self.inner.record_event(event).await
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
        _principal: &rustain::domain::models::PolicyPrincipal,
        _message_type: SemanticMessageType,
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
        "conformance-scripted".to_owned()
    }
    fn list_models(&self) -> Vec<rustain::domain::models::provider::ModelDescriptor> {
        Vec::new()
    }
    async fn health_check(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    async fn connectivity_probe(&self) -> Result<ProbeOutcome, ProviderError> {
        Ok(ProbeOutcome {
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

/// Real A2A recipient: signed card, loopback HTTP listener, daemon runtime,
/// NodeJournal, and production admission/policy pipeline.
pub(crate) struct Host {
    pub(crate) journal: Journal,
    endpoint: String,
    cancel: CancellationToken,
    http: tokio::task::JoinHandle<anyhow::Result<()>>,
    _keys: tempfile::TempDir,
}

impl Host {
    pub(crate) async fn start(policy: A2aAdmissionPolicy, mode: ResponseMode) -> Self {
        Self::start_at_authority(policy, mode, None).await
    }
    pub(crate) async fn start_with_journal(
        journal: Journal,
        sink: Arc<dyn RoomJournal>,
        policy: A2aAdmissionPolicy,
        mode: ResponseMode,
    ) -> Self {
        Self::start_with_authority(journal, sink, policy, mode, None).await
    }

    pub(crate) async fn start_at_authority(
        policy: A2aAdmissionPolicy,
        mode: ResponseMode,
        authority: Option<String>,
    ) -> Self {
        let journal = Journal::open().await;
        let sink = journal.room.clone();
        Self::start_with_authority(journal, sink, policy, mode, authority).await
    }

    async fn start_with_authority(
        journal: Journal,
        sink: Arc<dyn RoomJournal>,
        policy: A2aAdmissionPolicy,
        mode: ResponseMode,
        advertised_host: Option<String>,
    ) -> Self {
        let keys = tempfile::tempdir().expect("identity directory");
        let (domain_tx, _domain_rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
        let tree = NodeTree::with_event_tx(
            domain_tx.clone(),
            Arc::new(|| chrono::Utc::now().timestamp_millis()),
        )
        .with_journal(journal.node.clone());
        let storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
            rustain::infrastructure::paths::sessions_dir(journal.workspace.path()),
            journal.workspace.path().to_path_buf(),
        ));
        let core = {
            let workspace = journal.workspace.path().to_path_buf();
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
        let delivery: Arc<dyn DeliveryPolicy> = Arc::new(ModePolicy(mode));
        let bus =
            rustain::adapters::daemon::server::peer_bus_slot_with_policy(&tree, delivery.clone());
        let server = rustain::adapters::daemon::server::AttachServer::new_with_node_tree_bus_policy_and_journal(
            core, Arc::new(tokio::sync::Mutex::new(Conversation { id: "conformance-recipient".to_owned(), ..Conversation::default() })),
            domain_tx, tree, bus, delivery, sink.clone(), journal.node.clone(), None,
        );
        let signer = IdentityKeyStore::new(keys.path())
            .load_or_generate()
            .expect("identity");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let endpoint = format!("http://{}/", listener.local_addr().expect("address"));
        let cancel = CancellationToken::new();
        let http = tokio::spawn(serve(
            listener,
            ServeConfig {
                registry: Arc::new(CapabilityRegistry::new(None)),
                signer,
                security: A2aServerSecurity::default(),
                runtime: Some(server as Arc<dyn rustain::domain::ports::InboundPeerRuntime>),
                transparency: Arc::new(
                    TransparencySink::new(sink).with_reader(journal.node.clone()),
                ),
                policy,
                workspace: journal.workspace.path().to_path_buf(),
                advertised_host,
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
    pub(crate) fn endpoint(&self) -> &str {
        &self.endpoint
    }
    pub(crate) fn peer(&self, alias: &str) -> A2aPeerSpec {
        A2aPeerSpec::new(
            alias,
            rustain::domain::models::RedactedUrl::from(self.endpoint().to_owned()),
            A2aPeerSource::Workspace,
        )
    }
    pub(crate) async fn stop(self) {
        self.cancel.cancel();
        self.http
            .await
            .expect("server task")
            .expect("server shutdown");
    }
}
pub(crate) struct Sender {
    pub(crate) journal: Journal,
    pub(crate) app_state: AppState,
    pub(crate) domain_rx: tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
    pub(crate) egress_rx: tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
    pub(crate) clock: Arc<rustain::domain::clock::MockClock>,
    pub(crate) egress: A2aEgress,
    _keys: tempfile::TempDir,
}

impl Sender {
    pub(crate) async fn start(peers: Vec<A2aPeerSpec>) -> Self {
        let journal = Journal::open().await;
        let sink = journal.room.clone();
        Self::with_journal(peers, sink, journal).await
    }
    pub(crate) async fn start_with_clock(
        peers: Vec<A2aPeerSpec>,
        clock: Arc<rustain::domain::clock::MockClock>,
    ) -> Self {
        let journal = Journal::open_with_clock(clock.clone()).await;
        let sink = journal.room.clone();
        Self::with_journal_and_clock(peers, sink, journal, clock).await
    }

    pub(crate) async fn with_journal(
        peers: Vec<A2aPeerSpec>,
        sink: Arc<dyn RoomJournal>,
        journal: Journal,
    ) -> Self {
        let clock = Arc::new(rustain::domain::clock::MockClock::at_wall_ms(
            1_800_000_000_000,
        ));
        Self::with_journal_and_clock(peers, sink, journal, clock).await
    }

    pub(crate) async fn with_journal_and_clock(
        peers: Vec<A2aPeerSpec>,
        sink: Arc<dyn RoomJournal>,
        journal: Journal,
        clock: Arc<rustain::domain::clock::MockClock>,
    ) -> Self {
        let reader: Arc<dyn RoomJournalReader> = journal.node.clone();
        Self::with_reader_and_clock(peers, sink, journal, reader, clock).await
    }

    /// Compose the production sender with a controlled read side, for tests
    /// that exercise a journal read failure through the command front door.
    pub(crate) async fn with_reader_and_clock(
        peers: Vec<A2aPeerSpec>,
        sink: Arc<dyn RoomJournal>,
        journal: Journal,
        reader: Arc<dyn RoomJournalReader>,
        clock: Arc<rustain::domain::clock::MockClock>,
    ) -> Self {
        let (event_tx, egress_rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
        let keys = tempfile::tempdir().expect("item capability key directory");
        let tree = NodeTree::new();
        let egress = A2aEgress::compose_with_clock(
            peers,
            tree,
            sink,
            reader,
            event_tx,
            rustain::adapters::a2a::item_capability::load_item_capability_key(Ok(keys
                .path()
                .to_path_buf())),
            clock.clone(),
        )
        .expect("compose production sender egress");
        tokio::time::timeout(SETTLE, egress.await_cards_settled())
            .await
            .expect("sender cards settle");
        let (mut app_state, domain_rx) = app_state(journal.workspace.path());
        app_state.a2a_send = Some(egress.runtime().clone());
        Self {
            journal,
            app_state,
            domain_rx,
            egress_rx,
            clock,
            egress,
            _keys: keys,
        }
    }

    pub(crate) async fn command(&mut self, state: &mut TuiState, line: &str) {
        state.input_buffer = line.to_owned();
        let InputAction::ExecuteCommand { name, args } = submit_message_for_test(state) else {
            panic!("{line:?} must reach the command dispatcher");
        };
        assert_eq!(name, "team");
        team_command(state, "conv", args.as_deref(), &self.app_state).await;
    }
    pub(crate) fn runtime(&self) -> &rustain::adapters::a2a::driver::A2aDelegationRuntime {
        self.egress.runtime()
    }

    pub(crate) async fn raise_retract_card(&mut self, state: &mut TuiState, line: &str) {
        self.command(state, line).await;
        let preview = tokio::time::timeout(SETTLE, async {
            loop {
                match self.domain_rx.recv().await {
                    Some(AppEvent::TeamRetractPreviewReady { preview, .. }) => break preview,
                    Some(_) => {}
                    None => panic!("domain channel closed before retract preview"),
                }
            }
        })
        .await
        .expect("retract preview published");
        open_team_retract_card(state, "conv", preview);
    }

    pub(crate) async fn confirm_retract(&self, state: &mut TuiState) -> Option<String> {
        if handle_input(state, &DomainInputEvent::KeyPress('y')) != InputAction::TeamRetractConfirm
        {
            return None;
        }
        let pending = resolve_team_retract_card(state, true)?;
        Some(
            team_retract_dispatch(
                self.runtime(),
                &pending.peer,
                &pending.item_id,
                pending.task,
            )
            .await
            .message,
        )
    }
    pub(crate) async fn confirm_retract_with_board(
        &self,
        state: &mut TuiState,
    ) -> Option<(String, Option<String>)> {
        if handle_input(state, &DomainInputEvent::KeyPress('y')) != InputAction::TeamRetractConfirm
        {
            return None;
        }
        let pending = resolve_team_retract_card(state, true)?;
        let result = team_retract_dispatch(
            self.runtime(),
            &pending.peer,
            &pending.item_id,
            pending.task,
        )
        .await;
        Some((result.message, result.board))
    }

    pub(crate) async fn rows(&self) -> Vec<JournalEntry> {
        self.journal.entries().await
    }
    pub(crate) async fn settle(&mut self, state: &mut TuiState, expected: usize) {
        let mut tabs = rustain::domain::models::tab::TabManager::new(CancellationToken::new());
        for _ in 0..expected {
            let event = match tokio::time::timeout(SETTLE, self.domain_rx.recv()).await {
                Ok(Some(event)) => event,
                other => panic!(
                    "settlement event unavailable ({other:?}); durable journal: {:?}",
                    self.journal.entries().await
                ),
            };
            let AppEvent::TeamSendSettled {
                conversation_id,
                block_id,
                index,
                outcome,
            } = event
            else {
                panic!("expected TeamSendSettled, got {event:?}");
            };
            let rows = read_room_events(&self.journal).await;
            let action_exists = rows
                .iter()
                .any(|(_, event)| matches!(event, RoomEvent::TeamSendActionRecorded { .. }));
            if action_exists {
                assert!(
                    rows.iter().any(|(_, event)| matches!(event, RoomEvent::TeamSendRecipientSettled { index: durable_index, .. } if *durable_index as usize == index)),
                    "visible TeamSendSettled({index}) arrived before its durable typed settlement"
                );
            } else {
                assert_eq!(
                    outcome,
                    rustain::domain::models::RecipientOutcome::NotSent,
                    "without an action only unsent is safe"
                );
            }
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
}

fn app_state(workspace: &Path) -> (AppState, tokio::sync::mpsc::UnboundedReceiver<AppEvent>) {
    use clap::Parser;
    use rustain::adapters::noop::{NoOpProvider, NoOpStorage};
    use rustain::domain::services::plan_manager::PlanManager;
    use rustain::domain::services::plan_mode_injector::DefaultPlanInjector;
    use rustain::infrastructure::composition::ComposeContext;
    use rustain::infrastructure::runtime::agent_core::AgentCore;
    use rustain::infrastructure::runtime::event_bus::EventBus;

    let approval_runtime = ApprovalRuntime::new(16, Arc::new(NoOpApprovalPersistence));
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
            Arc::new(NoOpMemory) as Arc<dyn rustain::domain::ports::MemoryPort>
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
        Arc::new(NoOpUsageLedger),
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

pub(crate) async fn read_room_events(journal: &Journal) -> Vec<(u64, RoomEvent)> {
    RoomJournalReader::load_entries(journal.node.as_ref())
        .await
        .expect("read real journal")
        .into_iter()
        .filter_map(|entry| match entry.record {
            JournalRecord::Room(event) => Some((entry.seq, event)),
            _ => None,
        })
        .collect()
}
/// Append an acknowledgement through the recipient's real room journal.
/// The item address is resolved from that journal's own ingress event.
pub(crate) async fn acknowledge_received_item(journal: &Journal, task: &str) {
    let address = read_room_events(journal)
        .await
        .into_iter()
        .find_map(|(_, event)| match event {
            RoomEvent::RecipientItemReceived {
                task: received_task,
                address,
                ..
            } if received_task == task => Some(address),
            _ => None,
        })
        .expect("recipient journal contains the received item for this task");
    journal
        .room
        .record_event(RoomEvent::RecipientItemAcknowledged {
            address,
            alias: None,
        })
        .await
        .expect("append recipient acknowledgement");
}

pub(crate) async fn folded_actions(journal: &Journal) -> Vec<SendActionView> {
    fold_send_actions(&journal.entries().await)
}
/// Blocks a selected real recipient append and exposes explicit start/release
/// signals so tests prove an in-flight POST without sleeps.
pub(crate) struct GateRoomJournal {
    pub(crate) inner: Arc<dyn RoomJournal>,
    pub(crate) matches: fn(&RoomEvent) -> bool,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl GateRoomJournal {
    pub(crate) fn new(inner: Arc<dyn RoomJournal>, matches: fn(&RoomEvent) -> bool) -> Self {
        Self {
            inner,
            matches,
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        }
    }
    pub(crate) async fn wait_started(&self) {
        self.started.notified().await;
    }
    pub(crate) fn release(&self) {
        self.release.notify_one();
    }
}

#[async_trait::async_trait]
impl RoomJournal for GateRoomJournal {
    async fn record_event(&self, event: RoomEvent) -> Result<(), RoomJournalError> {
        if (self.matches)(&event) {
            self.started.notify_one();
            self.release.notified().await;
        }
        self.inner.record_event(event).await
    }
}
/// Seed a durable incoming item only; retract still travels through the real
/// `/team retract` preview and confirmation path.
pub(crate) async fn seed_recipient_item(journal: &Journal, item: &str, task: &str) {
    let address = ItemAddress::from_a2a_ingress(
        SubmitterKey::loopback().pseudonymous_peer_id(),
        ItemId::from_replay(item),
    );
    journal
        .room
        .record_event(RoomEvent::RecipientItemReceived {
            message_type: SemanticMessageType::Unknown,
            address,
            task: task.to_owned(),
            alias: None,
            content: format!("content for {item}"),
            in_reply_to: None,
            capability_hash: None,
            reply_capability_matched: false,
            authorship: rustain::domain::models::Authorship::Unknown,
            send_authorization: rustain::domain::models::SendAuthorization::Unknown,
        })
        .await
        .expect("seed recipient item");
}
