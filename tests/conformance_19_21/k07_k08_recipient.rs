use super::harness::{Journal, Sender};
use arc_swap::ArcSwap;
use futures::{StreamExt, stream::BoxStream};
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::a2a::auth::A2aServerSecurity;
use rustain::adapters::a2a::card_cache::SignedCardCache;
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
use rustain::adapters::rap::IdentityKeyStore;
use rustain::adapters::tui::app::{InputAction, submit_message_for_test};
use rustain::domain::errors::ProviderError;
use rustain::domain::models::capability_registry::CapabilityRegistry;
use rustain::domain::models::{
    AppConfig, CompletionOptions, Conversation, Message, ResponseMode, RoomEvent, StopReason,
    StreamChunk,
};
use rustain::domain::ports::{
    DeliveryPolicy, EffectiveDeliveryPolicy, ProbeOutcome, SecurityPort, StoragePort,
    StreamingProvider, ToolSetPort,
};
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::infrastructure::paths::sessions_dir;
use rustain::infrastructure::runtime::app_state::AppState;
use rustain::infrastructure::subagent::NodeTree;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const IO_TIMEOUT: Duration = Duration::from_secs(45);
type RequestCount = Arc<parking_lot::Mutex<usize>>;

struct CountingProvider {
    requests: RequestCount,
    started: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl StreamingProvider for CountingProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<Message>,
        _options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        *self.requests.lock() += 1;
        self.started.notify_one();
        Ok(futures::stream::iter(vec![
            StreamChunk::Text {
                content: "scripted response".to_owned(),
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
        "recipient-scripted".to_owned()
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

fn delivery_policy(mode: ResponseMode) -> Arc<dyn DeliveryPolicy> {
    use rustain::domain::models::{
        IndividualDefaults, IndividualPolicy, NotificationUrgency, TeamPolicy, TeamTypeOverride,
    };
    let individual = IndividualPolicy {
        defaults: IndividualDefaults {
            response_mode: Some(ResponseMode::NotifyAndAuto),
            notification: Some(NotificationUrgency::Immediate),
            ..IndividualDefaults::default()
        },
        ..IndividualPolicy::default()
    };
    let mut team = TeamPolicy::default();
    team.overrides.per_type.insert(
        "story_assignment".to_owned(),
        toml::Value::try_from(TeamTypeOverride {
            response_mode: Some(mode),
            notification: None,
        })
        .expect("valid story assignment policy"),
    );
    let effective = rustain::domain::services::team_policy::resolve_effective_policy(
        &individual,
        Some(&team),
        &[],
    );
    Arc::new(EffectiveDeliveryPolicy::new(Arc::new(effective)))
}

fn daemon_runtime(
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
        recipient_context: Arc::new(
            rustain::adapters::daemon::recipient_context::RecipientContextState::default(),
        ),
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
            sessions_dir(workspace),
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

struct Recipient {
    journal: Journal,
    endpoint: String,
    cancel: CancellationToken,
    http: tokio::task::JoinHandle<anyhow::Result<()>>,
    socket_cancel: CancellationToken,
    socket_task: tokio::task::JoinHandle<()>,
    requests: RequestCount,
    started: Arc<tokio::sync::Notify>,
    _keys: tempfile::TempDir,
}

impl Recipient {
    async fn start(mode: ResponseMode) -> Self {
        let journal = Journal::open().await;
        let workspace = journal.workspace.path().to_path_buf();
        let requests = Arc::new(parking_lot::Mutex::new(0));
        let started = Arc::new(tokio::sync::Notify::new());
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let node_tree = NodeTree::with_event_tx(
            event_tx.clone(),
            Arc::new(|| chrono::Utc::now().timestamp_millis()),
        )
        .with_journal(journal.node.clone())
        .with_host_binding(rustain::infrastructure::subagent::current_host_binding(
            &workspace,
        ));
        let storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
            sessions_dir(&workspace),
            workspace.clone(),
        ));
        let provider = Arc::new(CountingProvider {
            requests: requests.clone(),
            started: started.clone(),
        });
        let runtime_workspace = workspace.clone();
        let core = Arc::new(DaemonCore::new(
            workspace.clone(),
            Arc::new(ArcSwap::from_pointee(AppConfig::default())),
            Arc::new(NoOpMemory),
            storage.clone(),
            Arc::new(NoOpSecurity),
            Arc::new(NoOpPersona),
            Arc::new(rustain::adapters::rap::PeerTopicStore::new()),
            Box::new(move || {
                Ok(daemon_runtime(
                    provider.clone(),
                    storage.clone(),
                    &runtime_workspace,
                ))
            }),
        ));
        let policy = delivery_policy(mode);
        let bus = rustain::adapters::daemon::server::peer_bus_slot_with_policy(
            &node_tree,
            policy.clone(),
        );
        let server = AttachServer::new_with_node_tree_bus_policy_and_journal(
            core,
            Arc::new(tokio::sync::Mutex::new(Conversation {
                id: "recipient-test".to_owned(),
                ..Conversation::default()
            })),
            event_tx,
            node_tree,
            bus,
            policy,
            journal.room.clone(),
            journal.node.clone(),
            None,
        );

        let keys = tempfile::tempdir().expect("recipient signing keys");
        let signer = IdentityKeyStore::new(keys.path())
            .load_or_generate()
            .expect("recipient signing identity");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind recipient HTTP listener");
        let endpoint = format!("http://{}/", listener.local_addr().unwrap());
        let cancel = CancellationToken::new();
        let http = tokio::spawn(serve(
            listener,
            ServeConfig {
                registry: Arc::new(CapabilityRegistry::new(None)),
                signer,
                security: A2aServerSecurity::default(),
                runtime: Some(server.clone() as Arc<dyn rustain::domain::ports::InboundPeerRuntime>),
                transparency: Arc::new(
                    TransparencySink::new(journal.room.clone()).with_reader(journal.node.clone()),
                ),
                policy: A2aAdmissionPolicy::Allow,
                workspace: workspace.clone(),
                advertised_host: None,
                cards: Arc::new(SignedCardCache::new()),
            },
            cancel.child_token(),
        ));

        let socket_path = workspace.join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).expect("bind daemon socket");
        let socket_cancel = CancellationToken::new();
        let socket_task =
            tokio::spawn(server.run(listener, event_rx, None, None, socket_cancel.child_token()));
        Self {
            journal,
            endpoint,
            cancel,
            http,
            socket_cancel,
            socket_task,
            requests,
            started,
            _keys: keys,
        }
    }

    fn peer(&self, alias: &str) -> rustain::domain::models::A2aPeerSpec {
        rustain::domain::models::A2aPeerSpec::new(
            alias,
            rustain::domain::models::RedactedUrl::from(self.endpoint.clone()),
            rustain::domain::models::A2aPeerSource::Workspace,
        )
    }

    async fn attach(&self, read_only: bool) -> tokio::net::UnixStream {
        let mut stream =
            tokio::net::UnixStream::connect(self.journal.workspace.path().join("daemon.sock"))
                .await
                .expect("connect attached recipient client");
        let nonce = match frame(&mut stream).await {
            DaemonFrame::AttachChallenge { nonce } => nonce,
            other => panic!("attach challenge expected, got {other:?}"),
        };
        let keys = tempfile::tempdir().expect("attach client keys");
        let signer = IdentityKeyStore::new(keys.path())
            .load_or_generate()
            .expect("attach client identity");
        let tier = ConnectionTier::TrustedLocal;
        let proof = signer.attach_proof(&nonce, PROTOCOL_VERSION, tier.proof_tag(), read_only);
        write_frame(
            &mut stream,
            &ClientFrame::Attach {
                protocol_version: PROTOCOL_VERSION,
                read_only_ok: read_only,
                tier,
                challenge_nonce: nonce,
                identity: signer.identity().clone(),
                proof,
            },
        )
        .await
        .expect("signed attach proof");
        match frame(&mut stream).await {
            DaemonFrame::AttachAck { granted_mode, .. } => {
                assert_eq!(
                    granted_mode,
                    if read_only {
                        AttachMode::ReadOnly
                    } else {
                        AttachMode::ReadWrite
                    }
                );
            }
            other => panic!("attach acknowledgement expected, got {other:?}"),
        }
        stream
    }

    async fn stop(self) {
        self.socket_cancel.cancel();
        self.socket_task.abort();
        self.cancel.cancel();
        self.http.await.expect("HTTP task").expect("HTTP shutdown");
    }
}

async fn frame(stream: &mut tokio::net::UnixStream) -> DaemonFrame {
    tokio::time::timeout(IO_TIMEOUT, read_frame::<_, DaemonFrame>(stream))
        .await
        .expect("daemon socket frame timeout")
        .expect("decode daemon frame")
        .expect("daemon socket remains open")
}

async fn inspect_context(stream: &mut tokio::net::UnixStream) -> (Vec<String>, String) {
    write_frame(stream, &ClientFrame::InspectRecipientContext)
        .await
        .expect("inspect recipient context through actual socket");
    loop {
        match frame(stream).await {
            DaemonFrame::RecipientContextState {
                selected_item_ids,
                inventory,
                ..
            } => return (selected_item_ids, inventory),
            DaemonFrame::Error(error) => panic!("context inspection refused: {error:?}"),
            _ => {}
        }
    }
}
async fn set_context(stream: &mut tokio::net::UnixStream, item_id: &str, selected: bool) {
    write_frame(
        stream,
        &ClientFrame::SetRecipientContext {
            item_id: item_id.to_owned(),
            selected,
        },
    )
    .await
    .expect("send actual recipient context selection frame");
    loop {
        match frame(stream).await {
            DaemonFrame::RecipientActionResult {
                action,
                target,
                outcome:
                    RecipientActionOutcome::ContextSelection {
                        selected: confirmed,
                        changed,
                    },
            } => {
                assert_eq!(
                    action,
                    if selected {
                        RecipientAction::ContextSelect
                    } else {
                        RecipientAction::ContextRemove
                    }
                );
                assert_eq!(
                    target,
                    RecipientActionTarget::Item {
                        item_id: item_id.to_owned()
                    }
                );
                assert_eq!(confirmed, selected);
                assert!(
                    changed,
                    "positive selection/deselection control changes state"
                );
                return;
            }
            DaemonFrame::Error(error) => panic!("recipient context action refused: {error:?}"),
            _ => {}
        }
    }
}
async fn view_received_item(
    stream: &mut tokio::net::UnixStream,
) -> Vec<rustain::domain::models::ChatMessage> {
    write_frame(
        stream,
        &ClientFrame::HistoryRequest {
            before_index: None,
            count: 200,
        },
    )
    .await
    .expect("view received item through attached history");
    loop {
        match frame(stream).await {
            DaemonFrame::History { messages, .. } => return messages,
            DaemonFrame::Error(error) => panic!("received-item view refused: {error:?}"),
            _ => {}
        }
    }
}

async fn received_item(journal: &Journal, body: &str) -> String {
    journal
        .entries()
        .await
        .iter()
        .find_map(|entry| match &entry.record {
            rustain::domain::models::JournalRecord::Room(RoomEvent::RecipientItemReceived {
                address,
                content,
                ..
            }) if content == body => Some(address.item().as_str().to_owned()),
            _ => None,
        })
        .expect("real signed A2A ingress recorded received item")
}
async fn received_task(journal: &Journal, body: &str) -> String {
    journal
        .entries()
        .await
        .iter()
        .find_map(|entry| match &entry.record {
            rustain::domain::models::JournalRecord::Room(RoomEvent::RecipientItemReceived {
                task,
                content,
                ..
            }) if content == body => Some(task.clone()),
            _ => None,
        })
        .expect("real signed A2A ingress records its task correlation")
}

async fn wait_task_completed(recipient: &Recipient, task: &str) {
    let client = reqwest::Client::new();
    tokio::time::timeout(IO_TIMEOUT, async {
        loop {
            let response = client
                .post(&recipient.endpoint)
                .json(&serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 9,
                    "method": "tasks/get",
                    "params": { "id": task },
                }))
                .send()
                .await
                .expect("get actual A2A task status");
            let value: serde_json::Value = response.json().await.expect("decode task status");
            if value["result"]["status"]["state"] == "completed" {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actual auto-response task reaches completed status");
}

fn local_view_app_state(workspace: &Path) -> AppState {
    use clap::Parser;
    use rustain::adapters::noop::{NoOpProvider, NoOpStorage};
    use rustain::domain::services::plan_manager::PlanManager;
    use rustain::domain::services::plan_mode_injector::DefaultPlanInjector;
    use rustain::infrastructure::composition::ComposeContext;
    use rustain::infrastructure::runtime::agent_core::AgentCore;
    use rustain::infrastructure::runtime::event_bus::EventBus;

    let approval = ApprovalRuntime::new(16, Arc::new(NoOpApprovalPersistence));
    let provider = Arc::new(ArcSwap::from_pointee(
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
        approval,
        Arc::new(tokio::sync::RwLock::new(
            rustain::domain::models::SandboxPolicy::Permissive,
        )),
        Arc::new(PlanManager::new(workspace.to_path_buf())),
        Arc::new(DefaultPlanInjector::new()),
        provider,
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
    .0
}

async fn local_view(workspace: &Path, item_id: &str) -> rustain::adapters::tui::state::TuiState {
    let app = local_view_app_state(workspace);
    let mut state = rustain::adapters::tui::state::TuiState::new(130, 30);
    state.input_buffer = format!("/team view {item_id}");
    let InputAction::ExecuteCommand { name, args } = submit_message_for_test(&mut state) else {
        panic!("/team view must use the production command parser");
    };
    assert_eq!(name, "team");
    rustain::infrastructure::runtime::transparency_bridge::team_command(
        &mut state,
        "recipient-view",
        args.as_deref(),
        &app,
    )
    .await;
    state
}

/// **K07 local View.** M50-local-view-appends-acknowledgement → RED at the durable-event
/// assertion and fresh delivered-row assertion. Positive controls are the opened
/// story-assignment surface and K07's real attached-socket acknowledgement → ✓.
#[tokio::test]
async fn k07_local_view_opens_assignment_without_acknowledging() {
    let recipient = Recipient::start(ResponseMode::NotifyAndWait).await;
    let mut sender = Sender::start(vec![recipient.peer("writer")]).await;
    send_story(&mut sender, &recipient, "local view assignment").await;
    let item_id = received_item(&recipient.journal, "local view assignment").await;

    let state = local_view(recipient.journal.workspace.path(), &item_id).await;
    let surface = state
        .recipient_surface
        .expect("production /team view opens a received recipient surface");
    assert_eq!(
        surface.message_type,
        rustain::domain::models::SemanticMessageType::StoryAssignment
    );
    assert_eq!(
        surface.item_state,
        Some("received"),
        "M50-local-view-appends-acknowledgement: Local View preserves received state"
    );
    assert!(
        surface.content.contains("local view assignment"),
        "Local View opens the received assignment body: {}",
        surface.content
    );
    assert_eq!(
        *recipient.requests.lock(),
        0,
        "Local View does not invoke the recipient provider"
    );
    let events = recipient.journal.entries().await;
    assert!(
        !events.iter().any(|entry| matches!(
            &entry.record,
            rustain::domain::models::JournalRecord::Room(
                RoomEvent::RecipientItemAcknowledged { address, .. }
            ) if address.item().as_str() == item_id
        )),
        "M50-local-view-appends-acknowledgement: local View must not append RecipientItemAcknowledged"
    );
    let board = board_fresh(&recipient, &sender).await;
    assert!(
        board.contains("● writer  delivered"),
        "M50-local-view-appends-acknowledgement: local View leaves fresh sender row delivered: {board}"
    );
    recipient.stop().await;
}

async fn board_fresh(recipient: &Recipient, base: &Sender) -> String {
    let mut reader = Sender::with_journal_and_clock(
        vec![recipient.peer("writer")],
        base.journal.room.clone(),
        base.journal.clone(),
        base.clock.clone(),
    )
    .await;
    let mut state = rustain::adapters::tui::state::TuiState::new(100, 36);
    reader.command(&mut state, "/team board --last").await;
    tokio::time::timeout(IO_TIMEOUT, async {
        loop {
            if let Some(rustain::domain::events::AppEvent::TeamBoardReady { message, .. }) =
                reader.domain_rx.recv().await
            {
                return message;
            }
        }
    })
    .await
    .expect("production board command publishes a fresh render")
}

async fn send_story(sender: &mut Sender, recipient: &Recipient, body: &str) {
    let mut state = rustain::adapters::tui::state::TuiState::new(100, 36);
    sender
        .command(
            &mut state,
            &format!("/team send --type story_assignment writer {body}"),
        )
        .await;
    sender.settle(&mut state, 1).await;
    assert!(
        recipient
            .journal
            .entries()
            .await
            .iter()
            .any(|entry| matches!(
                &entry.record,
                rustain::domain::models::JournalRecord::Room(
                    RoomEvent::RecipientItemReceived { content, .. }
                ) if content == body
            )),
        "the real recipient receives the typed assignment"
    );
}

async fn acknowledge(stream: &mut tokio::net::UnixStream, item_id: &str) {
    write_frame(
        stream,
        &ClientFrame::AcknowledgeRecipientItem {
            item_id: item_id.to_owned(),
        },
    )
    .await
    .expect("send acknowledgement selected by attached writer key path");
    loop {
        match frame(stream).await {
            DaemonFrame::RecipientActionResult {
                action,
                target,
                outcome,
            } => {
                assert_eq!(action, RecipientAction::Acknowledge);
                assert_eq!(
                    target,
                    RecipientActionTarget::Item {
                        item_id: item_id.to_owned()
                    }
                );
                assert!(matches!(
                    outcome,
                    RecipientActionOutcome::Applied { reply: None }
                        | RecipientActionOutcome::Idempotent
                ));
                break;
            }
            DaemonFrame::Error(error) => panic!("acknowledgement refused: {error:?}"),
            _ => {}
        }
    }
}

/// **K07.** M50 (View acknowledges) → RED when the fresh board is not `● writer  delivered`;
/// M49 is killed by the attached-composer key-to-frame unit test in `attach_loop.rs`
/// (`story_assignment_surface_a_key_dispatches_ack_only_for_received_writer`).
/// Positive controls: the sender's delivered row before ack and acknowledged row after
/// a real signed-socket acknowledgement frame; `notify-and-auto` remains delivered (M51).
#[tokio::test]
async fn k07_story_assignment_view_stays_delivered_ack_refines_and_auto_stays_delivered() {
    let recipient = Recipient::start(ResponseMode::NotifyAndWait).await;
    let mut sender = Sender::start(vec![recipient.peer("writer")]).await;
    send_story(&mut sender, &recipient, "assignment payload").await;
    let item_id = received_item(&recipient.journal, "assignment payload").await;
    let mut attached = recipient.attach(false).await;

    let _ = inspect_context(&mut attached).await;
    let _ = view_received_item(&mut attached).await;
    let before = board_fresh(&recipient, &sender).await;
    assert!(
        before.contains("● writer  delivered"),
        "M50: viewing must retain delivered state: {before}"
    );
    acknowledge(&mut attached, &item_id).await;
    let after = board_fresh(&recipient, &sender).await;
    assert!(
        after.contains("✓ writer  acknowledged"),
        "fresh board reads durable ack: {after}"
    );
    drop(attached);
    recipient.stop().await;

    let auto = Recipient::start(ResponseMode::NotifyAndAuto).await;
    let mut auto_sender = Sender::start(vec![auto.peer("writer")]).await;
    send_story(&mut auto_sender, &auto, "automatic assignment").await;
    let task = received_task(&auto.journal, "automatic assignment").await;
    wait_task_completed(&auto, &task).await;
    let mut auto_attached = auto.attach(false).await;
    let history = view_received_item(&mut auto_attached).await;
    assert!(
        history.iter().any(|message| {
            message.authorship == rustain::domain::models::Authorship::AgentComposed
                && message.send_authorization
                    == rustain::domain::models::SendAuthorization::Automatic
        }),
        "M51 positive control: notify-and-auto produced an agent-composed automatic response"
    );
    let auto_board = board_fresh(&auto, &auto_sender).await;
    assert!(
        auto_board.contains("● writer  delivered"),
        "M51: FR93 automatic response is not acknowledgement: {auto_board}"
    );
    drop(auto_attached);
    auto.stop().await;
}

/// **K08(i).** M52 is killed only by the selection oracle in K08(ii); this test kills
/// provider work on wait-mode arrival/View/Ack. Positive control: draft-mode arrival
/// calls the scripted provider and increments its request counter.
#[tokio::test]
async fn k08_wait_arrival_view_ack_are_provider_zero_with_draft_positive_control() {
    let recipient = Recipient::start(ResponseMode::NotifyAndWait).await;
    let mut sender = Sender::start(vec![recipient.peer("writer")]).await;
    send_story(&mut sender, &recipient, "wait assignment").await;
    let item_id = received_item(&recipient.journal, "wait assignment").await;
    let mut attached = recipient.attach(false).await;
    let _ = inspect_context(&mut attached).await;
    view_received_item(&mut attached).await;
    acknowledge(&mut attached, &item_id).await;
    let _ = inspect_context(&mut attached).await;
    assert_eq!(
        *recipient.requests.lock(),
        0,
        "M49/M50-independent K08(i): wait arrival, view, and ack do not request provider completion"
    );
    drop(attached);
    recipient.stop().await;

    let draft = Recipient::start(ResponseMode::NotifyAndDraft).await;
    let mut draft_sender = Sender::start(vec![draft.peer("writer")]).await;
    send_story(&mut draft_sender, &draft, "draft assignment").await;
    if *draft.requests.lock() == 0 {
        tokio::time::timeout(IO_TIMEOUT, draft.started.notified())
            .await
            .expect("notify-and-draft positive control invokes scripted provider");
    }
    assert!(*draft.requests.lock() > 0);
    draft.stop().await;
}

/// **K08(ii).** M52(a/b/c) → RED if any of arrival/View/Ack selects this item.
/// Positive control selects with the production context-selection frame, observes it
/// through InspectRecipientContext, then deselects; the empty selected ids after the
/// ordinary actions and the unchanged inventory item are the independent oracle.
#[tokio::test]
async fn k08_arrival_view_ack_do_not_select_context_and_selection_oracle_can_fire() {
    let recipient = Recipient::start(ResponseMode::NotifyAndWait).await;
    let mut sender = Sender::start(vec![recipient.peer("writer")]).await;
    send_story(&mut sender, &recipient, "context assignment").await;
    let item_id = received_item(&recipient.journal, "context assignment").await;
    let mut attached = recipient.attach(false).await;
    let (selected, inventory) = inspect_context(&mut attached).await;
    assert!(
        selected.is_empty(),
        "M52 arrival must not select context: {selected:?}"
    );
    assert!(
        !inventory.contains(&item_id),
        "M52 arrival item must be absent from selected-context inventory: {inventory}"
    );

    let local = local_view(recipient.journal.workspace.path(), &item_id).await;
    let surface = local
        .recipient_surface
        .expect("production /team view opens the received item surface");
    assert_eq!(surface.item_state, Some("received"));
    assert!(surface.context_available && !surface.context_selected);
    let _ = view_received_item(&mut attached).await;
    let _ = inspect_context(&mut attached).await;
    acknowledge(&mut attached, &item_id).await;
    let after_ack = local_view(recipient.journal.workspace.path(), &item_id).await;
    let surface = after_ack
        .recipient_surface
        .expect("fresh Local View opens the acknowledged item surface");
    assert_eq!(surface.item_state, Some("acknowledged"));
    assert!(
        surface.context_available && !surface.context_selected,
        "the acknowledged surface still offers context without selecting it"
    );
    let (selected, inventory) = inspect_context(&mut attached).await;
    assert!(
        selected.is_empty(),
        "M52 View/Ack must not select context: {selected:?}"
    );
    assert!(
        !inventory.contains(&item_id),
        "M52 View/Ack item remains absent from selected-context inventory: {inventory}"
    );

    set_context(&mut attached, &item_id, true).await;
    let (selected, inventory) = inspect_context(&mut attached).await;
    assert!(
        selected.contains(&item_id) && inventory.contains(&item_id),
        "context selection positive control: ids={selected:?}, inventory={inventory}"
    );
    set_context(&mut attached, &item_id, false).await;
    let (selected, _) = inspect_context(&mut attached).await;
    assert!(
        !selected.contains(&item_id),
        "context deselection positive control: {selected:?}"
    );
    drop(attached);
    recipient.stop().await;
}
