//! Story 19.19: consultation front-door keystones.
//! Host/runtime, sender and journal composition follows conformance_19_18_a2a_reply_thread.
//! Recipient decisions use EffectiveDeliveryPolicy, never a type-ignoring mode double.
#![cfg(feature = "a2a")]

use arc_swap::ArcSwap;
use futures::stream::BoxStream;
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::a2a::auth::A2aServerSecurity;
use rustain::adapters::a2a::card_cache::SignedCardCache;
use rustain::adapters::a2a::egress::A2aEgress;
use rustain::adapters::a2a::server::{ServeConfig, serve};
use rustain::adapters::a2a::transparency::TransparencySink;
use rustain::adapters::daemon::protocol::{
    AttachMode, AttachSnapshot, ClientFrame, ConnectionTier, DaemonFrame, PROTOCOL_VERSION,
    PeerDraftAction, ProtocolError, RecipientAction, RecipientActionOutcome, RecipientActionTarget,
    read_frame, write_frame,
};
use rustain::adapters::daemon::runtime::{DaemonCore, DaemonTurnRuntime};
use rustain::adapters::daemon::server::AttachServer;
use rustain::adapters::filesystem::FileSystemStorage;
use rustain::adapters::noop::{
    NoOpApprovalPersistence, NoOpMemory, NoOpPersona, NoOpSecurity, NoOpToolSet, NoOpUsageLedger,
};
use rustain::adapters::rap::IdentityKeyStore;
use rustain::adapters::tui::app::{InputAction, submit_message_for_test};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::errors::ProviderError;
use rustain::domain::events::AppEvent;
use rustain::domain::models::capability_registry::CapabilityRegistry;
use rustain::domain::models::{
    A2aPeerSource, A2aPeerSpec, AppConfig, CompletionOptions, Conversation, JournalRecord, Message,
    PeerRowOperation, PeerRowStage, ResponseMode, RoomEvent, SemanticMessageType, StopReason,
    StreamChunk,
};
use rustain::domain::ports::{
    DeliveryPolicy, RoomJournal, RoomJournalReader, SecurityPort, StoragePort, StreamingProvider,
    ToolSetPort,
};
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::infrastructure::runtime::app_state::AppState;
use rustain::infrastructure::runtime::attach_loop::{apply_peer_row_operation, seed_conversation};
use rustain::infrastructure::runtime::transparency_bridge::{team_command, team_send_settled};
use rustain::infrastructure::subagent::{NodeJournal, NodeRoomJournal, NodeTree};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const SETTLE: Duration = Duration::from_secs(45);
type RequestTrace = Arc<parking_lot::Mutex<Vec<(Vec<Message>, CompletionOptions)>>>;

fn consultation_policy(mode: ResponseMode) -> Arc<dyn DeliveryPolicy> {
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
        "consultation".to_owned(),
        toml::Value::try_from(TeamTypeOverride {
            response_mode: Some(mode),
            notification: None,
        })
        .expect("valid production type policy"),
    );
    let effective = rustain::domain::services::team_policy::resolve_effective_policy(
        &individual,
        Some(&team),
        &[],
    );
    Arc::new(rustain::domain::ports::EffectiveDeliveryPolicy::new(
        Arc::new(effective),
    ))
}
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

struct ScriptedProvider {
    text: String,
    requests: RequestTrace,
    gate: Option<Arc<tokio::sync::Notify>>,
}

#[async_trait::async_trait]
impl StreamingProvider for ScriptedProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<Message>,
        _options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        use futures::StreamExt;
        self.requests.lock().push((_messages, _options));
        if let Some(gate) = &self.gate {
            gate.notified().await;
        }
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

type StorageWrap = Arc<dyn Fn(Arc<dyn StoragePort>) -> Arc<dyn StoragePort> + Send + Sync>;
type RuntimeFactory = Arc<
    dyn Fn(&Path, Arc<dyn StoragePort>, Arc<ScriptedProvider>) -> Arc<DaemonTurnRuntime>
        + Send
        + Sync,
>;

/// Closure used by test hosts to wrap their actual daemon `StoragePort`.
#[derive(Clone)]
struct HostOptions {
    policy: A2aAdmissionPolicy,
    mode: ResponseMode,
    draft_mentions_workspace: bool,
    draft_gate: Option<Arc<tokio::sync::Notify>>,
    serve_wrap: Option<Wrap>,
    attach_wrap: Option<Wrap>,
    storage_wrap: Option<StorageWrap>,
    runtime_factory: Option<RuntimeFactory>,
    security: A2aServerSecurity,
    bind: &'static str,
}

#[derive(Default)]
struct StorageFaultHarness {
    fail_plan: parking_lot::Mutex<Option<FailPeerRowSaves>>,
    next_save_gate: parking_lot::Mutex<Option<Arc<SaveGate>>>,
    target_save_gate: parking_lot::Mutex<Option<(String, Arc<SaveGate>)>>,
}

impl StorageFaultHarness {
    fn fail_ready_and_removal(&self, row_id: String) {
        *self.fail_plan.lock() = Some(FailPeerRowSaves {
            row_id,
            remaining: 2,
        });
    }

    fn gate_next_save(&self, gate: Arc<SaveGate>) {
        *self.next_save_gate.lock() = Some(gate);
    }

    fn gate_row_transition(&self, row_id: String, gate: Arc<SaveGate>) {
        *self.target_save_gate.lock() = Some((row_id, gate));
    }
}
fn is_ready_or_removed(conversation: &Conversation, row_id: &str) -> bool {
    let row_present = conversation
        .messages
        .iter()
        .any(|message| message.id == row_id);
    conversation
        .peer_row_controls
        .get(row_id)
        .is_some_and(|control| control.stage == PeerRowStage::Ready)
        || (!row_present && !conversation.peer_row_controls.contains_key(row_id))
}

impl SaveGate {
    async fn wait_until_entered(&self) {
        tokio::time::timeout(SETTLE, self.entered.notified())
            .await
            .expect("save gate entry deadline");
    }

    fn unblock(&self) {
        self.release.notify_one();
    }
}

struct FailPeerRowSaves {
    row_id: String,
    remaining: usize,
}

#[derive(Default)]
struct SaveGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

struct FaultStorage {
    inner: Arc<dyn StoragePort>,
    faults: Arc<StorageFaultHarness>,
}

fn fault_storage_wrap(faults: Arc<StorageFaultHarness>) -> StorageWrap {
    Arc::new(move |inner| {
        Arc::new(FaultStorage {
            inner,
            faults: faults.clone(),
        })
    })
}

#[async_trait::async_trait]
impl StoragePort for FaultStorage {
    async fn save_conversation(
        &self,
        conversation: &Conversation,
    ) -> Result<(), rustain::domain::errors::StorageError> {
        let gate = self.faults.next_save_gate.lock().take().or_else(|| {
            let mut pending = self.faults.target_save_gate.lock();
            let should_gate = pending
                .as_ref()
                .is_some_and(|(row_id, _)| is_ready_or_removed(conversation, row_id));
            if should_gate {
                pending.take().map(|(_, gate)| gate)
            } else {
                None
            }
        });
        if let Some(gate) = gate {
            gate.entered.notify_one();
            gate.release.notified().await;
        }

        let should_fail = {
            let mut guard = self.faults.fail_plan.lock();
            let (should_fail, clear_plan) = match guard.as_mut() {
                Some(plan) => {
                    let should_fail =
                        is_ready_or_removed(conversation, &plan.row_id) && plan.remaining > 0;
                    if should_fail {
                        plan.remaining -= 1;
                        (true, plan.remaining == 0)
                    } else {
                        (false, false)
                    }
                }
                None => (false, false),
            };
            if clear_plan {
                *guard = None;
            }
            should_fail
        };
        if should_fail {
            return Err(rustain::domain::errors::StorageError::IoError(
                "injected peer-row transaction save failure".to_owned(),
            ));
        }
        self.inner.save_conversation(conversation).await
    }

    async fn load_conversation(
        &self,
        id: &str,
    ) -> Result<Option<Conversation>, rustain::domain::errors::StorageError> {
        self.inner.load_conversation(id).await
    }

    async fn list_conversations(
        &self,
    ) -> Result<
        Vec<rustain::domain::models::ConversationSummary>,
        rustain::domain::errors::StorageError,
    > {
        self.inner.list_conversations().await
    }

    async fn save_session_meta(
        &self,
        id: &str,
        meta: &rustain::domain::models::SessionMeta,
    ) -> Result<(), rustain::domain::errors::StorageError> {
        self.inner.save_session_meta(id, meta).await
    }

    async fn load_session_meta(
        &self,
        id: &str,
    ) -> Result<Option<rustain::domain::models::SessionMeta>, rustain::domain::errors::StorageError>
    {
        self.inner.load_session_meta(id).await
    }

    async fn read_snapshot(
        &self,
        conversation_id: &str,
        checkpoint: rustain::domain::models::checkpoint::CheckpointId,
        path: &Path,
    ) -> Result<Option<Vec<u8>>, rustain::domain::errors::StorageError> {
        self.inner
            .read_snapshot(conversation_id, checkpoint, path)
            .await
    }
}

type Wrap = Arc<dyn Fn(Arc<dyn RoomJournal>) -> Arc<dyn RoomJournal> + Send + Sync>;

impl HostOptions {
    fn new(policy: A2aAdmissionPolicy, mode: ResponseMode) -> Self {
        Self {
            policy,
            mode,
            draft_mentions_workspace: false,
            draft_gate: None,
            serve_wrap: None,
            attach_wrap: None,
            storage_wrap: None,
            runtime_factory: None,
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
    storage: Arc<dyn StoragePort>,
    node_tree: NodeTree,
    _domain_rx: Option<tokio::sync::mpsc::UnboundedReceiver<AppEvent>>,
    requests: RequestTrace,
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
        let base_storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
            rustain::infrastructure::paths::sessions_dir(&journal.path),
            journal.path.clone(),
        ));
        let storage = if let Some(wrap) = &options.storage_wrap {
            wrap(base_storage)
        } else {
            base_storage
        };
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
        let requests: RequestTrace = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let core = {
            let requests = requests.clone();
            let draft_gate = options.draft_gate.clone();
            let workspace = journal.path.clone();
            let storage = storage.clone();
            let runtime_factory = options.runtime_factory.clone();
            Arc::new(DaemonCore::new(
                workspace.clone(),
                Arc::new(ArcSwap::from_pointee(AppConfig::default())),
                Arc::new(NoOpMemory),
                storage.clone(),
                Arc::new(NoOpSecurity),
                Arc::new(NoOpPersona),
                Arc::new(rustain::adapters::rap::PeerTopicStore::new()),
                Box::new(move || {
                    let provider = Arc::new(ScriptedProvider {
                        text: draft.clone(),
                        requests: requests.clone(),
                        gate: draft_gate.clone(),
                    });
                    Ok(match &runtime_factory {
                        Some(factory) => factory(&workspace, storage.clone(), provider),
                        None => build_runtime(provider, storage.clone(), &workspace),
                    })
                }),
            ))
        };
        let delivery = consultation_policy(options.mode);
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
            storage,
            node_tree,
            _domain_rx: Some(domain_rx),
            requests,
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

fn app_state_recording(
    workspace: &Path,
) -> (
    AppState,
    tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
    RequestTrace,
) {
    let (app, events) = app_state(workspace);
    let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
    app.provider.store(Arc::new(Arc::new(ScriptedProvider {
        text: String::new(),
        requests: requests.clone(),
        gate: None,
    }) as Arc<dyn StreamingProvider>));
    (app, events, requests)
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
        panic!("K01: typed operator send must reach TeamSendSettled, got {event:?}");
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

#[tokio::test]
async fn k01_typed_operator_fanout_preserves_bytes_policy_and_unknown_refusal() {
    let first = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut second = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndDraft).await;
    let _second_socket = SocketService::start(&mut second).await;
    let mut sender = new_sender(vec![first.peer("first"), second.peer("second")]).await;
    let mut state = TuiState::new(130, 30);

    sender
        .type_command(&mut state, "/team send first baseline untyped control")
        .await;
    settle_send(&mut sender, &mut state).await;
    let baseline = received(&room_events(&first.journal).await);
    assert_eq!(
        baseline[0].2, "baseline untyped control",
        "K01: old untyped grammar retains received bytes"
    );
    let completed = poll_state(&first.endpoint, &baseline[0].1, "completed").await;
    assert_eq!(
        completed["result"]["status"]["state"], "completed",
        "K01: untyped auto-policy control completes"
    );

    let body = "consultation first second\nfn before() {  old(); }\nfn after() {  new(); }\nerror:   repeated   spacing";
    sender
        .type_command(
            &mut state,
            &format!("/team send --type consultation first,second {body}"),
        )
        .await;
    let pending_id = format!("team-send-{}", state.team_send_next_id);
    assert!(
        matches!(
            state.team_send_blocks.get(&pending_id).map(Vec::as_slice),
            Some([first, second]) if first.alias == "first" && second.alias == "second"
        ),
        "K01: canonical typed fanout must start both recipient outcomes, not a local refusal: {:?}",
        state.feedback_blocks
    );
    settle_send(&mut sender, &mut state).await;
    settle_send(&mut sender, &mut state).await;
    for host in [&first, &second] {
        let rows = room_events(&host.journal).await;
        let root = rows
            .iter()
            .rev()
            .find_map(|(_, event)| match event {
                RoomEvent::RecipientItemReceived { task, content, .. } if content == body => {
                    Some((task, event))
                }
                _ => None,
            })
            .expect("K01: each recipient retains the actual multiline body");
        let value = serde_json::to_value(root.1).expect("serialize received record");
        assert_eq!(
            value["message_type"], "consultation",
            "K01: canonical type reaches each recipient's durable Received producer"
        );
        waiting_node(host, root.0).await;
    }
    let second_task = received(&room_events(&second.journal).await)
        .last()
        .expect("typed item")
        .1
        .clone();
    let second_node = waiting_node(&second, &second_task).await;
    let storage = FileSystemStorage::with_workspace_root(
        rustain::infrastructure::paths::sessions_dir(&second.journal.path),
        second.journal.path.clone(),
    );
    let deadline = tokio::time::Instant::now() + SETTLE;
    loop {
        let conversation = storage
            .load_conversation(CONVERSATION_ID)
            .await
            .expect("read persisted draft")
            .expect("persisted conversation");
        if conversation.messages.iter().any(|message| {
            message.id == format!("peer-response-{second_node}")
                && message.content.contains("automatic reply")
        }) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "K01: typed draft policy must retain its composed answer locally"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        first.requests.lock().len(),
        0,
        "K01: consultation wait policy invokes no new model turn"
    );
    assert_eq!(
        second.requests.lock().len(),
        1,
        "K01: consultation draft policy invokes one local draft"
    );
    assert!(
        !room_events(&second.journal)
            .await
            .iter()
            .any(|(_, event)| { matches!(event, RoomEvent::RecipientItemReplied { .. }) }),
        "K01: an unapproved draft never becomes a remote reply"
    );

    let snapshot = |entries| serde_json::to_vec(&entries).expect("journal snapshot");
    let sender_before = snapshot(
        sender
            .journal
            .node
            .load_entries()
            .await
            .expect("sender snapshot"),
    );
    let first_before = snapshot(
        first
            .journal
            .node
            .load_entries()
            .await
            .expect("first snapshot"),
    );
    let second_before = snapshot(
        second
            .journal
            .node
            .load_entries()
            .await
            .expect("second snapshot"),
    );
    sender
        .type_command(
            &mut state,
            "/team send --type future_consultation first,second must not leave",
        )
        .await;
    let event = tokio::time::timeout(SETTLE, sender.domain_rx.recv())
        .await
        .expect("refusal event")
        .expect("open event channel");
    let AppEvent::SystemNotice { message, .. } = event else {
        panic!("K01: unknown type refuses the entire action before delivery, got {event:?}");
    };
    assert!(
        message.contains("future_consultation"),
        "K01: refuse unknown type by its own name: {message}"
    );
    for token in rustain::domain::models::semantic_message_type_tokens() {
        assert!(
            message.contains(token),
            "K01: refusal names valid type {token}: {message}"
        );
    }
    assert_eq!(
        snapshot(
            sender
                .journal
                .node
                .load_entries()
                .await
                .expect("sender after refusal")
        ),
        sender_before,
        "K01: unknown type writes no sender journal record"
    );
    assert_eq!(
        snapshot(
            first
                .journal
                .node
                .load_entries()
                .await
                .expect("first after refusal")
        ),
        first_before,
        "K01: unknown type reaches no recipient mutation"
    );
    assert_eq!(
        snapshot(
            second
                .journal
                .node
                .load_entries()
                .await
                .expect("second after refusal")
        ),
        second_before,
        "K01: unknown type reaches no recipient mutation"
    );
    assert_eq!(first.requests.lock().len(), 0);
    assert_eq!(second.requests.lock().len(), 1);

    let first = first.restart().await;
    let replayed = room_events(&first.journal).await;
    let typed = replayed
        .iter()
        .find_map(|(_, event)| match event {
            RoomEvent::RecipientItemReceived { content, .. } if content == body => Some(event),
            _ => None,
        })
        .expect("K01: typed received item survives restart");
    assert_eq!(
        serde_json::to_value(typed).expect("replayed record")["message_type"],
        "consultation",
        "K01: durable type does not depend on transient policy or body sniffing"
    );
    first.cancel.cancel();
    first
        .http
        .await
        .expect("first server task")
        .expect("first shutdown");
    second.cancel.cancel();
    second
        .http
        .await
        .expect("second server task")
        .expect("second shutdown");
}

struct SocketService {
    path: PathBuf,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for SocketService {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.task.abort();
    }
}

impl SocketService {
    async fn start(host: &mut Host) -> Self {
        let path = host.journal.path.join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&path).expect("bind actual daemon socket");
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(host.server.clone().run(
            listener,
            host._domain_rx.take().expect("one actual domain receiver"),
            None,
            None,
            shutdown.child_token(),
        ));
        Self {
            path,
            shutdown,
            task,
        }
    }

    async fn attach(
        &self,
        tier: ConnectionTier,
        read_only: bool,
    ) -> (tokio::net::UnixStream, AttachMode, AttachSnapshot) {
        attach_socket(&self.path, tier, read_only, None).await
    }
}

async fn attach_socket(
    path: &Path,
    tier: ConnectionTier,
    read_only: bool,
    attach_started: Option<tokio::sync::oneshot::Sender<()>>,
) -> (tokio::net::UnixStream, AttachMode, AttachSnapshot) {
    let mut stream = tokio::net::UnixStream::connect(path)
        .await
        .expect("connect actual daemon");
    let nonce = match socket_frame(&mut stream).await {
        DaemonFrame::AttachChallenge { nonce } => nonce,
        other => panic!("actual challenge required, got {other:?}"),
    };
    let keys = tempfile::tempdir().expect("isolated attachment keys");
    let signer = IdentityKeyStore::new(keys.path())
        .load_or_generate()
        .expect("actual signer");
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
    .expect("signed attachment frame");
    if let Some(started) = attach_started {
        let _ = started.send(());
    }
    match socket_frame(&mut stream).await {
        DaemonFrame::AttachAck {
            granted_mode,
            snapshot,
        } => (stream, granted_mode, snapshot),
        other => panic!("authenticated attach must succeed, got {other:?}"),
    }
}

async fn socket_frame(stream: &mut tokio::net::UnixStream) -> DaemonFrame {
    tokio::time::timeout(SETTLE, read_frame::<_, DaemonFrame>(stream))
        .await
        .expect("actual socket frame deadline")
        .expect("valid actual daemon frame")
        .expect("open actual socket")
}
async fn recipient_action_result(
    stream: &mut tokio::net::UnixStream,
    mut view: Option<&mut Conversation>,
) -> (
    RecipientAction,
    RecipientActionTarget,
    RecipientActionOutcome,
) {
    loop {
        match socket_frame(stream).await {
            DaemonFrame::RecipientActionResult {
                action,
                target,
                outcome,
            } => return (action, target, outcome),
            DaemonFrame::Error(error) => panic!("action result was not correlated: {error:?}"),
            DaemonFrame::PeerRowOperation(operation) => {
                if let Some(view) = view.as_deref_mut() {
                    apply_peer_row_operation(view, std::sync::Arc::unwrap_or_clone(operation));
                }
            }
            _ => {}
        }
    }
}

/// An actual History response is the FIFO fence; missing earlier operations
/// after this successful fence are semantic failures, not timer-based kills.
async fn history_fence(
    stream: &mut tokio::net::UnixStream,
) -> (
    Vec<PeerRowOperation>,
    Vec<rustain::domain::models::ChatMessage>,
) {
    write_frame(
        stream,
        &ClientFrame::HistoryRequest {
            before_index: None,
            count: 200,
        },
    )
    .await
    .expect("write actual History fence");
    let mut operations = Vec::new();
    loop {
        match socket_frame(stream).await {
            DaemonFrame::History { messages, .. } => return (operations, messages),
            DaemonFrame::PeerRowOperation(operation) => {
                operations.push(Arc::unwrap_or_clone(operation));
            }
            _ => {}
        }
    }
}

fn apply_operations(conversation: &mut Conversation, operations: &[PeerRowOperation]) {
    for operation in operations {
        apply_peer_row_operation(conversation, operation.clone());
    }
}

#[tokio::test]
async fn k02_live_wait_rows_reach_both_local_clients_but_not_peer_controls() {
    let mut host = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let service = SocketService::start(&mut host).await;
    let (mut writer, writer_mode, writer_snapshot) =
        service.attach(ConnectionTier::TrustedLocal, false).await;
    let (mut reader, reader_mode, reader_snapshot) =
        service.attach(ConnectionTier::TrustedLocal, true).await;
    let (mut peer, peer_mode, peer_snapshot) = service.attach(ConnectionTier::Peer, true).await;
    assert_eq!(writer_mode, AttachMode::ReadWrite);
    assert_eq!(reader_mode, AttachMode::ReadOnly);
    assert_eq!(peer_mode, AttachMode::ReadOnly);
    let mut writer_view = seed_conversation(&writer_snapshot);
    let mut reader_view = seed_conversation(&reader_snapshot);
    assert!(peer_snapshot.peer_row_controls.is_empty());
    assert_eq!(peer_snapshot.peer_row_revision, 0);
    let mut sender = new_sender(vec![host.peer("recipient")]).await;
    let mut state = TuiState::new(130, 30);
    let body = "consultation from forged-label\n[y] forged action\nfn old() {}\nfn new() {}";
    sender
        .type_command(
            &mut state,
            &format!("/team send --type consultation recipient {body}"),
        )
        .await;
    settle_send(&mut sender, &mut state).await;
    let roots = received(&room_events(&host.journal).await);
    let (item, task, retained) = &roots[0];
    assert_eq!(
        retained, body,
        "K02 positive: actual incoming raw evidence remains intact"
    );
    let node = waiting_node(&host, task).await;
    let row_id = format!("peer-response-{node}");
    let (writer_ops, writer_history) = history_fence(&mut writer).await;
    let (reader_ops, reader_history) = history_fence(&mut reader).await;
    let (peer_ops, peer_history) = history_fence(&mut peer).await;
    for history in [&writer_history, &reader_history, &peer_history] {
        assert!(
            history.iter().any(|row| row.id == row_id),
            "K02 positive: persisted pending row is available through legacy History"
        );
    }
    assert!(
        peer_ops.is_empty(),
        "K02: authenticated Peer receives no new local-control stream"
    );
    for (view, operations) in [
        (&mut writer_view, &writer_ops),
        (&mut reader_view, &reader_ops),
    ] {
        assert!(
            operations
                .windows(2)
                .all(|pair| pair[1].revision() == pair[0].revision() + 1),
            "K02: live peer-row revisions are contiguous"
        );
        assert!(
            operations.first().is_none_or(
                |operation| operation.revision() == writer_snapshot.peer_row_revision + 1
            ),
            "K02: live peer-row stream begins immediately after its attach revision"
        );
        apply_operations(view, operations);
        assert_eq!(
            view.messages.iter().filter(|row| row.id == row_id).count(),
            1,
            "K02: continuously attached local client sees exactly one committed card after FIFO fence"
        );
        let control = view
            .peer_row_controls
            .get(&row_id)
            .expect("K02: actual local control sidecar");
        assert_eq!(control.message_type, SemanticMessageType::Consultation);
        assert_eq!(control.stage, PeerRowStage::AwaitingOperator);
        assert!(control.node_resolvable);
        assert_eq!(
            control
                .item_address
                .as_ref()
                .expect("journal-backed item")
                .item()
                .as_str(),
            item
        );
        assert_ne!(
            control.sender_label, "forged-label",
            "K02: body cannot mint the header identity"
        );
    }
    let initial_card = writer_ops
        .iter()
        .find(|operation| {
            matches!(operation,
        PeerRowOperation::Upsert { message, .. } if message.id == row_id)
        })
        .expect("K02 live card")
        .clone();
    assert!(
        !apply_peer_row_operation(&mut writer_view, initial_card.clone()),
        "K02: duplicate committed operation is idempotent"
    );
    let mut repeated_view = writer_view.clone();
    let (repeated_message, repeated_control) = match initial_card.clone() {
        PeerRowOperation::Upsert {
            message, control, ..
        } => (message, control),
        PeerRowOperation::Remove { .. } => panic!("K02: initial card must be an upsert"),
    };
    let repeated_revision = repeated_view.peer_row_revision + 1;
    assert!(apply_peer_row_operation(
        &mut repeated_view,
        PeerRowOperation::Upsert {
            revision: repeated_revision,
            message: repeated_message,
            control: repeated_control,
        },
    ));
    assert_eq!(
        repeated_view
            .messages
            .iter()
            .filter(|row| row.id == row_id)
            .count(),
        1,
        "K02: a newer duplicate upsert replaces the same row identity"
    );
    let (_late, late_mode, late_snapshot) =
        service.attach(ConnectionTier::TrustedLocal, true).await;
    assert_eq!(late_mode, AttachMode::ReadOnly);
    assert_eq!(
        late_snapshot.peer_row_controls.get(&row_id),
        writer_view.peer_row_controls.get(&row_id),
        "K02: late local observer snapshot has the live control metadata"
    );
    let late_view = seed_conversation(&late_snapshot);
    let message_view = |view: &Conversation| {
        view.messages
            .iter()
            .filter(|message| message.is_peer_display())
            .map(|message| (message.id.clone(), message.content.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(message_view(&writer_view), message_view(&late_view));
    assert_eq!(writer_view.peer_row_controls, late_view.peer_row_controls);
    assert_eq!(writer_view.peer_row_revision, late_view.peer_row_revision);
    let (_late_peer, _, late_peer_snapshot) = service.attach(ConnectionTier::Peer, true).await;
    assert!(late_peer_snapshot.peer_row_controls.is_empty());
    assert_eq!(late_peer_snapshot.peer_row_revision, 0);
    assert!(
        late_peer_snapshot
            .transcript
            .iter()
            .any(|row| row.id == row_id),
        "K02: omission of new controls does not revoke legacy Peer transcript observation"
    );
    write_frame(
        &mut writer,
        &ClientFrame::ResolvePeerDraft {
            node: node.clone(),
            action: PeerDraftAction::WriteOwn {
                content: "the actual local answer".to_owned(),
            },
        },
    )
    .await
    .expect("actual local node resolution");
    let (action, target, outcome) =
        recipient_action_result(&mut writer, Some(&mut writer_view)).await;
    assert_eq!(action, RecipientAction::Response);
    assert_eq!(
        target,
        RecipientActionTarget::Node {
            node_id: node.clone()
        }
    );
    assert!(matches!(
        &outcome,
        RecipientActionOutcome::Applied {
            reply: Some(receipt),
        } if receipt.item_id.as_str() == item.as_str()
    ));
    write_frame(
        &mut writer,
        &ClientFrame::ResolvePeerDraft {
            node: node.clone(),
            action: PeerDraftAction::WriteOwn {
                content: "stale composed text".to_owned(),
            },
        },
    )
    .await
    .expect("send deliberate resolution retry");
    let (action, target, outcome) =
        recipient_action_result(&mut writer, Some(&mut writer_view)).await;
    assert_eq!(action, RecipientAction::Response);
    assert_eq!(
        target,
        RecipientActionTarget::Node {
            node_id: node.clone()
        }
    );
    assert_eq!(outcome, RecipientActionOutcome::Idempotent);
    let resolved_rows = room_events(&host.journal).await;
    assert_eq!(
        resolved_rows
            .iter()
            .filter(|(_, event)| matches!(
                event,
                RoomEvent::RecipientItemReplied { via_node: Some(reply_node), .. }
                    if reply_node.as_str() == node
            ))
            .count(),
        1,
        "settled retry creates no second reply turn"
    );
    let terminal = poll_state(&host.endpoint, task, "completed").await;
    assert_eq!(terminal["result"]["status"]["state"], "completed");
    for (stream, view) in [
        (&mut writer, &mut writer_view),
        (&mut reader, &mut reader_view),
    ] {
        let (operations, _) = history_fence(stream).await;
        apply_operations(view, &operations);
        let rows: Vec<_> = view
            .messages
            .iter()
            .filter(|row| row.id == row_id)
            .collect();
        assert_eq!(rows.len(), 1, "K02: resolution replaces, never duplicates");
        assert_eq!(rows[0].content, "the actual local answer");
        assert!(
            view.peer_row_controls
                .get(&row_id)
                .is_none_or(
                    |control| control.stage == PeerRowStage::Settled && !control.node_resolvable
                ),
            "K02: resolved node never remains an actionable pending card"
        );
        assert!(
            !apply_peer_row_operation(view, initial_card.clone()),
            "K02: late old draft operation cannot resurrect the resolved card"
        );
        assert_eq!(
            view.messages
                .iter()
                .find(|row| row.id == row_id)
                .unwrap()
                .content,
            "the actual local answer"
        );
    }
    host.cancel.cancel();
    host.http.await.expect("host task").expect("HTTP shutdown");
}

async fn provider_started(host: &Host) {
    let deadline = tokio::time::Instant::now() + SETTLE;
    while host.requests.lock().is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "controlled actual provider must start"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn k02_draft_ready_and_resolution_replace_the_continuously_visible_card() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let mut options = HostOptions::new(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndDraft);
    options.draft_gate = Some(gate.clone());
    let mut host = Host::serve_with(options).await;
    let service = SocketService::start(&mut host).await;
    let (mut writer, _, writer_snapshot) =
        service.attach(ConnectionTier::TrustedLocal, false).await;
    let (mut reader, _, reader_snapshot) = service.attach(ConnectionTier::TrustedLocal, true).await;
    let mut writer_view = seed_conversation(&writer_snapshot);
    let mut reader_view = seed_conversation(&reader_snapshot);
    let mut sender = new_sender(vec![host.peer("drafting")]).await;
    let mut state = TuiState::new(130, 30);
    sender
        .type_command(
            &mut state,
            "/team send --type consultation drafting draft question",
        )
        .await;
    settle_send(&mut sender, &mut state).await;
    provider_started(&host).await;
    let roots = received(&room_events(&host.journal).await);
    let task = &roots[0].1;
    let mut row_id = None;
    for (stream, view) in [
        (&mut writer, &mut writer_view),
        (&mut reader, &mut reader_view),
    ] {
        let (operations, history) = history_fence(stream).await;
        apply_operations(view, &operations);
        let (id, control) = view
            .peer_row_controls
            .iter()
            .find(|(_, control)| control.stage == PeerRowStage::Drafting)
            .expect("K02: actual live Drafting control after FIFO fence");
        assert_eq!(control.message_type, SemanticMessageType::Consultation);
        assert!(
            history.iter().any(|row| &row.id == id),
            "K02 positive persisted Drafting row"
        );
        if let Some(previous) = &row_id {
            assert_eq!(
                id, previous,
                "K02: both local clients see the same host-minted row"
            );
        }
        row_id = Some(id.clone());
    }
    let row_id = row_id.expect("actual Drafting row id");
    assert!(
        !room_events(&host.journal)
            .await
            .iter()
            .any(|(_, event)| matches!(event, RoomEvent::RecipientItemReplied { .. })),
        "K02: an unapproved draft is not a reply"
    );
    gate.notify_one();
    let node = waiting_node(&host, task).await;
    for (stream, view) in [
        (&mut writer, &mut writer_view),
        (&mut reader, &mut reader_view),
    ] {
        let (operations, _) = history_fence(stream).await;
        apply_operations(view, &operations);
        assert_eq!(
            view.messages.iter().filter(|row| row.id == row_id).count(),
            1,
            "K02: Drafting-to-ready updates one row, not a second card"
        );
        let control = &view.peer_row_controls[&row_id];
        assert_eq!(control.stage, PeerRowStage::Ready);
        assert!(control.node_resolvable);
        assert!(
            view.messages
                .iter()
                .find(|row| row.id == row_id)
                .unwrap()
                .content
                .contains("automatic reply")
        );
    }
    let task_before_approval = rpc(
        &host.endpoint,
        serde_json::json!({"jsonrpc":"2.0","id":10,"method":"tasks/get","params":{"id":task}}),
    )
    .await;
    assert!(
        !serde_json::to_string(&task_before_approval)
            .unwrap()
            .contains("automatic reply"),
        "K02: live local draft does not become an unapproved remote answer"
    );
    write_frame(
        &mut writer,
        &ClientFrame::ResolvePeerDraft {
            node: node.clone(),
            action: PeerDraftAction::Approve,
        },
    )
    .await
    .expect("actual operator approves buffered draft");
    let (action, target, outcome) =
        recipient_action_result(&mut writer, Some(&mut writer_view)).await;
    assert_eq!(action, RecipientAction::Response);
    assert_eq!(
        target,
        RecipientActionTarget::Node {
            node_id: node.clone()
        }
    );
    assert!(matches!(
        &outcome,
        RecipientActionOutcome::Applied {
            reply: Some(receipt),
        } if receipt.item_id.as_str() == roots[0].0.as_str()
    ));
    let terminal = poll_state(&host.endpoint, task, "completed").await;
    assert_eq!(terminal["result"]["status"]["state"], "completed");
    for (stream, view) in [
        (&mut writer, &mut writer_view),
        (&mut reader, &mut reader_view),
    ] {
        let (operations, _) = history_fence(stream).await;
        apply_operations(view, &operations);
        let rows: Vec<_> = view
            .messages
            .iter()
            .filter(|row| row.id == row_id)
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].content, "automatic reply",
            "K02: approval replaces the local preview"
        );
        assert!(
            view.peer_row_controls
                .get(&row_id)
                .is_none_or(
                    |control| control.stage == PeerRowStage::Settled && !control.node_resolvable
                )
        );
    }
    host.cancel.cancel();
    host.http.await.expect("host task").expect("HTTP shutdown");
}

#[tokio::test]
async fn k02_canceled_draft_removes_the_card_and_old_updates_cannot_resurrect_it() {
    let mut options = HostOptions::new(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndDraft);
    options.draft_gate = Some(Arc::new(tokio::sync::Notify::new()));
    let mut host = Host::serve_with(options).await;
    let service = SocketService::start(&mut host).await;
    let (mut writer, _, writer_snapshot) =
        service.attach(ConnectionTier::TrustedLocal, false).await;
    let (mut reader, _, reader_snapshot) = service.attach(ConnectionTier::TrustedLocal, true).await;
    let mut writer_view = seed_conversation(&writer_snapshot);
    let mut reader_view = seed_conversation(&reader_snapshot);
    let mut sender = new_sender(vec![host.peer("canceled")]).await;
    let mut state = TuiState::new(130, 30);
    sender
        .type_command(
            &mut state,
            "/team send --type consultation canceled cancel the composing draft",
        )
        .await;
    settle_send(&mut sender, &mut state).await;
    provider_started(&host).await;
    let roots = received(&room_events(&host.journal).await);
    let task = &roots[0].1;
    let (initial, _) = history_fence(&mut writer).await;
    apply_operations(&mut writer_view, &initial);
    let (id, control) = writer_view
        .peer_row_controls
        .iter()
        .find(|(_, control)| control.stage == PeerRowStage::Drafting)
        .expect("K02: live Drafting positive before cancellation");
    let id = id.clone();
    let node = control.node_id.clone();
    let old = initial
        .iter()
        .find(|operation| {
            matches!(operation,
        PeerRowOperation::Upsert { message, .. } if message.id == id)
        })
        .unwrap()
        .clone();
    let (initial_reader, _) = history_fence(&mut reader).await;
    apply_operations(&mut reader_view, &initial_reader);
    let canceled = rpc(
        &host.endpoint,
        serde_json::json!({"jsonrpc":"2.0","id":11,"method":"tasks/cancel","params":{"id":task}}),
    )
    .await;
    assert_eq!(canceled["result"]["status"]["state"], "canceled");
    let deadline = tokio::time::Instant::now() + SETTLE;
    while host
        .node_tree
        .list()
        .await
        .iter()
        .any(|entry| entry.agent_id.as_str() == node)
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "actual cancellation cleanup must finish before FIFO fence"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    for (stream, view) in [
        (&mut writer, &mut writer_view),
        (&mut reader, &mut reader_view),
    ] {
        let (operations, history) = history_fence(stream).await;
        assert!(
            !history.iter().any(|row| row.id == id),
            "K02 positive: removal was actually persisted"
        );
        apply_operations(view, &operations);
        assert!(
            !view.messages.iter().any(|row| row.id == id),
            "K02: live committed removal clears both clients after FIFO fence"
        );
        assert!(!view.peer_row_controls.contains_key(&id));
        assert!(
            !apply_peer_row_operation(view, old.clone()),
            "K02: delayed old draft is rejected after removal"
        );
        assert!(!view.messages.iter().any(|row| row.id == id));
    }
    host.cancel.cancel();
    host.http.await.expect("host task").expect("HTTP shutdown");
}

#[tokio::test]
async fn k02_auto_response_retraction_replaces_both_clients_persisted_mark() {
    let mut host = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndAuto).await;
    let service = SocketService::start(&mut host).await;
    let (mut writer, _, writer_snapshot) =
        service.attach(ConnectionTier::TrustedLocal, false).await;
    let (mut reader, _, reader_snapshot) = service.attach(ConnectionTier::TrustedLocal, true).await;
    let mut writer_view = seed_conversation(&writer_snapshot);
    let mut reader_view = seed_conversation(&reader_snapshot);
    let mut sender = new_sender(vec![host.peer("auto")]).await;
    let mut state = TuiState::new(130, 30);
    sender
        .type_command(
            &mut state,
            "/team send --type consultation auto auto template question",
        )
        .await;
    settle_send(&mut sender, &mut state).await;
    let roots = received(&room_events(&host.journal).await);
    let terminal = poll_state(&host.endpoint, &roots[0].1, "completed").await;
    assert_eq!(terminal["result"]["status"]["state"], "completed");
    let (initial, _) = history_fence(&mut writer).await;
    apply_operations(&mut writer_view, &initial);
    let target = writer_view
        .messages
        .iter()
        .find(|row| row.authorship == rustain::domain::models::MessageAuthorship::AgentComposed)
        .expect("K02: auto template has an actual live retractable row")
        .id
        .clone();
    let (reader_initial, _) = history_fence(&mut reader).await;
    apply_operations(&mut reader_view, &reader_initial);
    write_frame(
        &mut writer,
        &ClientFrame::RetractAutoResponse {
            message_id: target.clone(),
            target_seq: None,
        },
    )
    .await
    .expect("actual trusted-local retract act");
    // The same connection's History fence follows the awaited retract handler.
    for (stream, view) in [
        (&mut writer, &mut writer_view),
        (&mut reader, &mut reader_view),
    ] {
        let (operations, history) = history_fence(stream).await;
        let persisted = history.iter().find(|row| row.id == target).unwrap();
        assert!(
            persisted.retracted_at_ms.is_some(),
            "K02 positive: actual retracted mark persisted"
        );
        apply_operations(view, &operations);
        let visible = view.messages.iter().find(|row| row.id == target).unwrap();
        assert_eq!(
            visible.retracted_at_ms, persisted.retracted_at_ms,
            "K02: persisted retract replacement reaches both continuously attached clients"
        );
        assert!(
            !view.messages.iter().any(|row| row.authorship
                == rustain::domain::models::MessageAuthorship::AgentComposed
                && row.retracted_at_ms.is_none()),
            "K02: another Ctrl+X has no surviving retract target"
        );
    }
    host.cancel.cancel();
    host.http.await.expect("host task").expect("HTTP shutdown");
}

#[tokio::test]
async fn k02_concurrent_attach_serializes_snapshot_with_live_peer_row_commit() {
    let faults = Arc::new(StorageFaultHarness::default());
    let mut options = HostOptions::new(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait);
    options.storage_wrap = Some(fault_storage_wrap(faults.clone()));
    let mut host = Host::serve_with(options).await;
    let service = SocketService::start(&mut host).await;
    let (mut writer, _, writer_snapshot) =
        service.attach(ConnectionTier::TrustedLocal, false).await;
    let mut writer_view = seed_conversation(&writer_snapshot);

    let save_gate = Arc::new(SaveGate::default());
    faults.gate_next_save(save_gate.clone());
    let peer = host.peer("racing");
    let send_task = tokio::spawn(async move {
        let mut sender = new_sender(vec![peer]).await;
        let mut state = TuiState::new(130, 30);
        sender
            .type_command(
                &mut state,
                "/team send --type consultation racing serialized arrival",
            )
            .await;
        settle_send(&mut sender, &mut state).await;
    });
    save_gate.wait_until_entered().await;

    let path = service.path.clone();
    let (attach_started, attach_sent) = tokio::sync::oneshot::channel();
    let mut attach_task = tokio::spawn(async move {
        attach_socket(
            &path,
            ConnectionTier::TrustedLocal,
            true,
            Some(attach_started),
        )
        .await
    });
    let attach_sent_at = tokio::time::Instant::now();
    attach_sent
        .await
        .expect("late signed attach sent while save is held");
    let attach_deadline = attach_sent_at + Duration::from_millis(300);
    assert!(
        tokio::time::timeout_at(attach_deadline, &mut attach_task)
            .await
            .is_err(),
        "K02: attach acknowledgement must wait until the in-flight row save commits"
    );
    save_gate.unblock();
    let (mut late, mode, late_snapshot) = attach_task.await.expect("concurrent attach task");
    assert_eq!(mode, AttachMode::ReadOnly);
    send_task.await.expect("actual sender task");

    let roots = received(&room_events(&host.journal).await);
    let task = &roots[0].1;
    let node = waiting_node(&host, task).await;
    let row_id = format!("peer-response-{node}");
    let (live_ops, _) = history_fence(&mut writer).await;
    assert!(
        live_ops.iter().any(|operation| matches!(
            operation,
            PeerRowOperation::Upsert { message, .. } if message.id == row_id
        )),
        "K02: pre-existing writer receives the actual committed row operation"
    );
    assert!(
        live_ops
            .iter()
            .all(|operation| operation.revision() > writer_snapshot.peer_row_revision)
    );
    assert!(
        live_ops
            .windows(2)
            .all(|pair| pair[0].revision() < pair[1].revision()),
        "K02: live batch revisions preserve mutation order"
    );
    apply_operations(&mut writer_view, &live_ops);

    let mut late_view = seed_conversation(&late_snapshot);
    assert_eq!(
        late_snapshot
            .transcript
            .iter()
            .filter(|message| message.id == row_id)
            .count(),
        1,
        "K02: the concurrent attach snapshot contains one persisted row"
    );
    assert_eq!(
        late_snapshot.peer_row_controls.get(&row_id),
        writer_view.peer_row_controls.get(&row_id),
        "K02: concurrent snapshot carries the same host control metadata as the live operation"
    );
    let (late_ops, late_history) = history_fence(&mut late).await;
    assert!(
        late_ops.is_empty(),
        "K02: a commit included in the snapshot is not also queued as a live operation"
    );
    assert!(late_history.iter().any(|message| message.id == row_id));
    apply_operations(&mut late_view, &late_ops);
    assert_eq!(
        late_view
            .messages
            .iter()
            .filter(|message| message.id == row_id)
            .count(),
        1,
        "K02: snapshot/live arrival cannot miss or double-apply the row"
    );

    let durable = host
        .storage
        .load_conversation(CONVERSATION_ID)
        .await
        .expect("load durable conversation")
        .expect("committed conversation");
    assert_eq!(late_snapshot.peer_row_revision, durable.peer_row_revision);
    assert_eq!(late_view.peer_row_revision, durable.peer_row_revision);
    assert_eq!(writer_view.peer_row_revision, durable.peer_row_revision);
    host.cancel.cancel();
    host.http.await.expect("host task").expect("HTTP shutdown");
}

#[tokio::test]
async fn k02_failed_ready_replacement_and_cleanup_remove_roll_back_without_publication() {
    let provider_gate = Arc::new(tokio::sync::Notify::new());
    let faults = Arc::new(StorageFaultHarness::default());
    let mut options = HostOptions::new(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndDraft);
    options.draft_gate = Some(provider_gate.clone());
    options.storage_wrap = Some(fault_storage_wrap(faults.clone()));
    let mut host = Host::serve_with(options).await;
    let service = SocketService::start(&mut host).await;
    let (mut writer, _, writer_snapshot) =
        service.attach(ConnectionTier::TrustedLocal, false).await;
    let (mut reader, _, reader_snapshot) = service.attach(ConnectionTier::TrustedLocal, true).await;
    let mut writer_view = seed_conversation(&writer_snapshot);
    let mut reader_view = seed_conversation(&reader_snapshot);
    let mut sender = new_sender(vec![host.peer("failed-draft")]).await;
    let mut state = TuiState::new(130, 30);
    sender
        .type_command(
            &mut state,
            "/team send --type consultation failed-draft fail the durable replacement",
        )
        .await;
    settle_send(&mut sender, &mut state).await;
    provider_started(&host).await;

    let roots = received(&room_events(&host.journal).await);
    let task = &roots[0].1;
    let (writer_initial, _) = history_fence(&mut writer).await;
    apply_operations(&mut writer_view, &writer_initial);
    let (reader_initial, _) = history_fence(&mut reader).await;
    apply_operations(&mut reader_view, &reader_initial);
    let (row_id, control) = writer_view
        .peer_row_controls
        .iter()
        .find(|(_, control)| control.stage == PeerRowStage::Drafting)
        .expect("actual incoming draft card");
    let row_id = row_id.clone();
    let initial_control = control.clone();
    let durable_before = host
        .storage
        .load_conversation(CONVERSATION_ID)
        .await
        .expect("load pre-failure conversation")
        .expect("persisted Drafting conversation");
    let initial_content = durable_before
        .messages
        .iter()
        .find(|message| message.id == row_id)
        .expect("persisted Drafting row")
        .content
        .clone();
    let initial_revision = durable_before.peer_row_revision;
    assert_eq!(
        durable_before.peer_row_controls.get(&row_id),
        Some(&initial_control),
        "K02 positive control: the original row control is durable"
    );

    let target_save_gate = Arc::new(SaveGate::default());
    faults.gate_row_transition(row_id.clone(), target_save_gate.clone());
    faults.fail_ready_and_removal(row_id.clone());
    provider_gate.notify_one();
    target_save_gate.wait_until_entered().await;
    let meta_before = host
        .storage
        .load_session_meta(CONVERSATION_ID)
        .await
        .expect("load durable sidecar while row save is blocked")
        .expect("persisted session sidecar");
    target_save_gate.unblock();
    let failed = poll_state(&host.endpoint, task, "failed").await;
    assert_eq!(failed["result"]["status"]["state"], "failed");

    for (stream, view) in [
        (&mut writer, &mut writer_view),
        (&mut reader, &mut reader_view),
    ] {
        let (operations, history) = history_fence(stream).await;
        assert!(
            operations.is_empty(),
            "K02: failed replacement/removal published no peer-row update"
        );
        assert_eq!(view.peer_row_controls.get(&row_id), Some(&initial_control));
        let persisted_row = history
            .iter()
            .find(|message| message.id == row_id)
            .expect("FIFO fence retains the pre-failure card");
        assert_eq!(persisted_row.content, initial_content);
        apply_operations(view, &operations);
        assert_eq!(
            view.messages
                .iter()
                .filter(|message| message.id == row_id)
                .count(),
            1
        );
        assert_eq!(
            view.messages
                .iter()
                .find(|message| message.id == row_id)
                .unwrap()
                .content,
            initial_content
        );
    }

    let durable_after = host
        .storage
        .load_conversation(CONVERSATION_ID)
        .await
        .expect("load post-failure conversation")
        .expect("conversation remains durable");
    assert_eq!(durable_after.peer_row_revision, initial_revision);
    assert_eq!(
        durable_after.peer_row_controls.get(&row_id),
        Some(&initial_control),
        "K02: rollback restores the durable control sidecar exactly"
    );
    assert_eq!(
        durable_after
            .messages
            .iter()
            .find(|message| message.id == row_id)
            .expect("durable row survived")
            .content,
        initial_content
    );
    let meta_after = host
        .storage
        .load_session_meta(CONVERSATION_ID)
        .await
        .expect("load post-failure sidecar")
        .expect("session sidecar remains");
    assert_eq!(
        serde_json::to_value(meta_after).unwrap(),
        serde_json::to_value(meta_before).unwrap(),
        "K02: failed writes leave the filesystem storage sidecar unchanged"
    );

    let (mut late, _, snapshot) = service.attach(ConnectionTier::TrustedLocal, true).await;
    assert_eq!(snapshot.peer_row_revision, initial_revision);
    assert_eq!(
        snapshot
            .transcript
            .iter()
            .filter(|message| message.id == row_id)
            .count(),
        1
    );
    let snapshot_control = &snapshot.peer_row_controls[&row_id];
    assert_eq!(snapshot_control.stage, PeerRowStage::Drafting);
    assert_eq!(snapshot_control.node_id, initial_control.node_id);
    assert_eq!(snapshot_control.message_type, initial_control.message_type);
    assert_eq!(snapshot_control.sender_label, initial_control.sender_label);
    assert_eq!(snapshot_control.item_address, initial_control.item_address);
    assert!(
        !snapshot_control.node_resolvable,
        "K02: failed draft is now an orphan, not an actionable stale node"
    );
    let (late_ops, _) = history_fence(&mut late).await;
    assert!(
        late_ops.is_empty(),
        "K02: the failed operation was not queued for late attach"
    );
    host.cancel.cancel();
    host.http.await.expect("host task").expect("HTTP shutdown");
}

#[tokio::test]
async fn k02_restart_orphan_drafting_row_retains_item_but_disables_stale_node_action() {
    let provider_gate = Arc::new(tokio::sync::Notify::new());
    let mut options = HostOptions::new(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndDraft);
    options.draft_gate = Some(provider_gate.clone());
    let mut host = Host::serve_with(options).await;
    let service = SocketService::start(&mut host).await;
    let (mut writer, _, _) = service.attach(ConnectionTier::TrustedLocal, false).await;
    let mut sender = new_sender(vec![host.peer("orphan")]).await;
    let mut state = TuiState::new(130, 30);
    sender
        .type_command(
            &mut state,
            "/team send --type consultation orphan restart while drafting",
        )
        .await;
    settle_send(&mut sender, &mut state).await;
    provider_started(&host).await;

    let (initial_ops, _) = history_fence(&mut writer).await;
    let (row_id, control) = initial_ops
        .iter()
        .find_map(|operation| match operation {
            PeerRowOperation::Upsert {
                message,
                control: Some(control),
                ..
            } if control.stage == PeerRowStage::Drafting => {
                Some((message.id.clone(), control.clone()))
            }
            _ => None,
        })
        .expect("actual live Drafting row before restart");
    let item_address = control
        .item_address
        .clone()
        .expect("Drafting row retains its journal-backed item address");
    let before = host
        .storage
        .load_conversation(CONVERSATION_ID)
        .await
        .expect("load pre-restart conversation")
        .expect("persisted Drafting conversation");
    let before_revision = before.peer_row_revision;
    assert_eq!(
        before.peer_row_controls[&row_id].stage,
        PeerRowStage::Drafting
    );

    drop(writer);
    drop(service);
    let mut host = host.restart().await;
    let service = SocketService::start(&mut host).await;
    let (mut observer, mode, snapshot) = service.attach(ConnectionTier::TrustedLocal, true).await;
    assert_eq!(mode, AttachMode::ReadOnly);
    assert_eq!(snapshot.peer_row_revision, before_revision);
    assert_eq!(
        snapshot
            .transcript
            .iter()
            .filter(|message| message.id == row_id)
            .count(),
        1,
        "K02: restart snapshot retains the persisted response row"
    );
    let orphan = &snapshot.peer_row_controls[&row_id];
    assert_eq!(orphan.stage, PeerRowStage::Drafting);
    assert_eq!(orphan.item_address.as_ref(), Some(&item_address));
    assert!(
        !orphan.node_resolvable,
        "K02: restarted orphan does not advertise an unresolvable node action"
    );
    let (post_attach_ops, history) = history_fence(&mut observer).await;
    assert!(
        post_attach_ops.is_empty(),
        "K02: the persisted row is in the snapshot, not replayed live"
    );
    assert!(history.iter().any(|message| message.id == row_id));

    let after = host
        .storage
        .load_conversation(CONVERSATION_ID)
        .await
        .expect("load post-restart conversation")
        .expect("conversation remains durable");
    assert_eq!(after.peer_row_revision, before_revision);
    assert_eq!(
        after.peer_row_controls[&row_id].item_address.as_ref(),
        Some(&item_address)
    );
    assert_eq!(
        after.peer_row_controls[&row_id].stage,
        PeerRowStage::Drafting
    );
    host.cancel.cancel();
    host.http.await.expect("host task").expect("HTTP shutdown");
}

#[tokio::test]
async fn k02_clear_removes_peer_row_sidecars_with_revisioned_local_removal() {
    let mut host = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let service = SocketService::start(&mut host).await;
    let (mut writer, _, writer_snapshot) =
        service.attach(ConnectionTier::TrustedLocal, false).await;
    let (mut reader, _, reader_snapshot) = service.attach(ConnectionTier::TrustedLocal, true).await;
    let (mut peer, _, peer_snapshot) = service.attach(ConnectionTier::Peer, true).await;
    let mut writer_view = seed_conversation(&writer_snapshot);
    let mut reader_view = seed_conversation(&reader_snapshot);
    assert!(peer_snapshot.peer_row_controls.is_empty());
    assert_eq!(peer_snapshot.peer_row_revision, 0);

    let mut sender = new_sender(vec![host.peer("clear")]).await;
    let mut state = TuiState::new(130, 30);
    sender
        .type_command(
            &mut state,
            "/team send --type consultation clear clear the active transcript",
        )
        .await;
    settle_send(&mut sender, &mut state).await;
    let roots = received(&room_events(&host.journal).await);
    let node = waiting_node(&host, &roots[0].1).await;
    let row_id = format!("peer-response-{node}");
    for (stream, view) in [
        (&mut writer, &mut writer_view),
        (&mut reader, &mut reader_view),
    ] {
        let (operations, history) = history_fence(stream).await;
        assert!(history.iter().any(|message| message.id == row_id));
        apply_operations(view, &operations);
        assert!(view.peer_row_controls.contains_key(&row_id));
    }
    let starting_revision = writer_view.peer_row_revision;
    assert_eq!(reader_view.peer_row_revision, starting_revision);

    write_frame(
        &mut writer,
        &ClientFrame::UserMessage {
            text: "/clear".to_owned(),
            images: Vec::new(),
        },
    )
    .await
    .expect("actual attached operator clear command");

    let mut cleared_removals = 0u64;
    for (stream, view) in [
        (&mut writer, &mut writer_view),
        (&mut reader, &mut reader_view),
    ] {
        let (operations, history) = history_fence(stream).await;
        assert_eq!(
            operations
                .iter()
                .filter(|operation| matches!(
                    operation,
                    PeerRowOperation::Remove { row_id: removed, .. } if removed == &row_id
                ))
                .count(),
            1,
            "K02: local clients receive one revisioned removal for the cleared peer row"
        );
        assert!(
            history.is_empty(),
            "K02: clear is persisted before the History fence"
        );
        apply_operations(view, &operations);
        assert!(!view.messages.iter().any(|message| message.id == row_id));
        assert!(!view.peer_row_controls.contains_key(&row_id));
        assert!(
            !view
                .messages
                .iter()
                .any(|message| message.is_peer_display()),
            "K02: every cleared peer row, controlled or not, leaves the attached transcript"
        );
        let removals = operations
            .iter()
            .filter(|operation| matches!(operation, PeerRowOperation::Remove { .. }))
            .count() as u64;
        assert!(removals >= 1);
        assert_eq!(view.peer_row_revision, starting_revision + removals);
        match cleared_removals {
            0 => cleared_removals = removals,
            seen => assert_eq!(seen, removals, "K02: every client sees the same removals"),
        }
    }
    let (peer_ops, peer_history) = history_fence(&mut peer).await;
    assert!(
        peer_ops.is_empty(),
        "K02: Peer tier receives no local row-control operation"
    );
    assert!(
        peer_history.is_empty(),
        "K02: Peer tier retains legacy transcript semantics after clear"
    );

    let durable = host
        .storage
        .load_conversation(CONVERSATION_ID)
        .await
        .expect("load cleared conversation")
        .expect("cleared conversation remains persisted");
    assert!(durable.messages.is_empty());
    assert!(durable.peer_row_controls.is_empty());
    assert_eq!(
        durable.peer_row_revision,
        starting_revision + cleared_removals
    );
    let meta = host
        .storage
        .load_session_meta(CONVERSATION_ID)
        .await
        .expect("load cleared session sidecar")
        .expect("cleared sidecar remains");
    assert_eq!(meta.message_count, 0);
    host.cancel.cancel();
    host.http.await.expect("host task").expect("HTTP shutdown");
}

/// K03/K04: a real attached operator defers over the socket frame, then answers
/// the same still-pending item. Defer appends an independent turn and does not
/// resolve the card; the subsequent answer remains a separate turn.
#[tokio::test]
async fn k03_socket_defer_is_durable_and_does_not_consume_the_answer() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let mut options = HostOptions::new(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndDraft);
    options.draft_gate = Some(gate.clone());
    let mut host = Host::serve_with(options).await;

    let socket = SocketService::start(&mut host).await;
    let (mut writer, mode, _) = socket.attach(ConnectionTier::TrustedLocal, false).await;
    assert_eq!(mode, AttachMode::ReadWrite);
    let mut sender = new_sender(vec![host.peer("defer")]).await;
    let mut state = TuiState::new(130, 30);
    sender
        .type_command(&mut state, "/team send --type consultation defer question")
        .await;
    settle_send(&mut sender, &mut state).await;
    provider_started(&host).await;
    let (item_id, task, _) = received(&room_events(&host.journal).await)
        .into_iter()
        .next()
        .expect("actual incoming item from HTTP producer");
    let _ = history_fence(&mut writer).await;
    let received_address = room_events(&host.journal)
        .await
        .iter()
        .find_map(|(_, event)| match event {
            RoomEvent::RecipientItemReceived { address, .. }
                if address.item().as_str() == item_id =>
            {
                Some(address.clone())
            }
            _ => None,
        })
        .expect("actual received address");
    host.journal
        .room
        .record_event(RoomEvent::RecipientItemRetracted {
            address: received_address,
            retracted_at_ms: chrono::Utc::now().timestamp_millis(),
            principal_collapsed: true,
        })
        .await
        .expect("seed accepted retraction fact");
    write_frame(
        &mut writer,
        &ClientFrame::DeferRecipientItem {
            item_id: "never-minted-defer-id".to_owned(),
        },
    )
    .await
    .expect("send unknown-item defer");
    let (action, target, outcome) = recipient_action_result(&mut writer, None).await;
    assert_eq!(action, RecipientAction::Defer);
    assert_eq!(
        target,
        RecipientActionTarget::Item {
            item_id: "never-minted-defer-id".to_owned()
        }
    );
    assert!(matches!(
        outcome,
        RecipientActionOutcome::Refused {
            error: ProtocolError::Malformed(message),
        } if message == "recipient item not found"
    ));
    let (mut observer, _, _) = socket.attach(ConnectionTier::TrustedLocal, true).await;
    write_frame(
        &mut observer,
        &ClientFrame::DeferRecipientItem {
            item_id: item_id.clone(),
        },
    )
    .await
    .expect("send read-only defer");
    let (action, target, outcome) = recipient_action_result(&mut observer, None).await;
    assert_eq!(action, RecipientAction::Defer);
    assert_eq!(
        target,
        RecipientActionTarget::Item {
            item_id: item_id.clone()
        }
    );
    assert_eq!(
        outcome,
        RecipientActionOutcome::Refused {
            error: ProtocolError::ReadOnly,
        }
    );
    drop(observer);
    assert!(
        room_events(&host.journal)
            .await
            .iter()
            .all(|(_, event)| !matches!(event, RoomEvent::RecipientItemReplied { .. })),
        "unknown and read-only defers have no durable effect"
    );

    write_frame(
        &mut writer,
        &ClientFrame::AcknowledgeRecipientItem {
            item_id: item_id.clone(),
        },
    )
    .await
    .expect("acknowledge still-pending item");
    let (action, target, outcome) = recipient_action_result(&mut writer, None).await;
    assert_eq!(action, RecipientAction::Acknowledge);
    assert_eq!(
        target,
        RecipientActionTarget::Item {
            item_id: item_id.clone()
        }
    );
    assert_eq!(outcome, RecipientActionOutcome::Applied { reply: None });
    let task_before = rpc(
        &host.endpoint,
        serde_json::json!({"jsonrpc":"2.0","id":77,"method":"tasks/get","params":{"id":task}}),
    )
    .await;
    let conversation_before = host
        .storage
        .load_conversation(CONVERSATION_ID)
        .await
        .expect("read pre-defer card")
        .expect("persisted held draft");
    let card_before = conversation_before
        .messages
        .iter()
        .find(|row| row.id.starts_with("peer-response-"))
        .expect("held draft card")
        .clone();
    let controls_before = conversation_before.peer_row_controls.clone();
    write_frame(
        &mut writer,
        &ClientFrame::DeferRecipientItem {
            item_id: item_id.clone(),
        },
    )
    .await
    .expect("send actual defer frame");
    let (action, target, outcome) = recipient_action_result(&mut writer, None).await;
    assert_eq!(action, RecipientAction::Defer);
    assert_eq!(
        target,
        RecipientActionTarget::Item {
            item_id: item_id.clone()
        }
    );
    let receipt = match outcome {
        RecipientActionOutcome::Applied {
            reply: Some(receipt),
        } => receipt,
        other => panic!("defer must return a durable reply receipt: {other:?}"),
    };
    assert_eq!(receipt.item_id, item_id);
    assert_eq!(
        receipt.kind,
        rustain::domain::models::ReplyKind::Defer { minutes: 30 }
    );
    let deferred_turn = receipt.turn_id;
    write_frame(
        &mut writer,
        &ClientFrame::DeferRecipientItem {
            item_id: item_id.clone(),
        },
    )
    .await
    .expect("send second deliberate defer");
    let (action, target, outcome) = recipient_action_result(&mut writer, None).await;
    assert_eq!(action, RecipientAction::Defer);
    assert_eq!(
        target,
        RecipientActionTarget::Item {
            item_id: item_id.clone()
        }
    );
    let second_receipt = match outcome {
        RecipientActionOutcome::Applied {
            reply: Some(receipt),
        } => receipt,
        other => panic!("second defer must also be durably applied: {other:?}"),
    };
    assert_eq!(
        second_receipt.kind,
        rustain::domain::models::ReplyKind::Defer { minutes: 30 }
    );
    let second_defer_turn = second_receipt.turn_id;
    assert_ne!(
        second_defer_turn, deferred_turn,
        "each defer is an independent turn"
    );
    assert_eq!(
        room_events(&host.journal)
            .await
            .iter()
            .filter(|(_, event)| matches!(event, RoomEvent::RecipientItemAcknowledged { .. }))
            .count(),
        1,
        "defer does not append another acknowledgement event"
    );
    let task_after = rpc(
        &host.endpoint,
        serde_json::json!({"jsonrpc":"2.0","id":78,"method":"tasks/get","params":{"id":task}}),
    )
    .await;
    assert_eq!(
        task_after["result"]["status"]["state"], task_before["result"]["status"]["state"],
        "defer leaves the actual remote task status unchanged"
    );
    let conversation_after = host
        .storage
        .load_conversation(CONVERSATION_ID)
        .await
        .expect("read post-defer card")
        .expect("persisted held draft remains");
    let card_after = conversation_after
        .messages
        .iter()
        .find(|row| row.id.starts_with("peer-response-"))
        .expect("held draft card remains");
    assert_eq!(
        serde_json::to_value(card_after).unwrap(),
        serde_json::to_value(&card_before).unwrap(),
        "defer cannot alter the persisted answer card",
    );
    assert_eq!(conversation_after.peer_row_controls, controls_before);
    let projection = rustain::adapters::policy::JournalRecipientItemProjection::from_entries(
        &RoomJournalReader::load_entries(host.journal.node.as_ref())
            .await
            .expect("read durable item and defer"),
    );
    assert!(matches!(
        projection.find_by_id(&item_id).expect("pending item").state,
        rustain::domain::models::RecipientItemState::Acknowledged { .. }
    ));
    let events_before_answer = room_events(&host.journal).await;
    assert!(events_before_answer.iter().any(|(_, event)| matches!(
        event,
        RoomEvent::RecipientItemReplied {
            address: reply_address,
            content,
            kind: rustain::domain::models::ReplyKind::Defer { minutes: 30 },
            via_node: None,
            authorship: rustain::domain::models::ReplyAuthorship::HumanWritten,
            ..
        } if reply_address.turn().as_str() == deferred_turn && content == "Busy — defer 30m"
    )));

    gate.notify_one();
    let _node = waiting_node(&host, &task).await;
    write_frame(
        &mut writer,
        &ClientFrame::ReplyToRecipientItem {
            item_id: item_id.clone(),
            text: "answer".to_owned(),
        },
    )
    .await
    .expect("send actual answer frame");
    let (action, target, outcome) = recipient_action_result(&mut writer, None).await;
    assert_eq!(action, RecipientAction::Reply);
    assert_eq!(
        target,
        RecipientActionTarget::Item {
            item_id: item_id.clone()
        }
    );
    let receipt = match outcome {
        RecipientActionOutcome::Applied {
            reply: Some(receipt),
        } => receipt,
        other => panic!("reply must return a durable receipt: {other:?}"),
    };
    assert_eq!(receipt.item_id, item_id);
    assert_eq!(receipt.kind, rustain::domain::models::ReplyKind::Answer);
    let answer_turn = receipt.turn_id;
    assert_ne!(deferred_turn, answer_turn);
    let events_after_answer = room_events(&host.journal).await;
    let replies: Vec<_> = events_after_answer
        .iter()
        .filter_map(|(_, event)| match event {
            RoomEvent::RecipientItemReplied {
                address,
                kind,
                content,
                ..
            } => Some((address.turn().as_str().to_owned(), *kind, content.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(replies.len(), 3);
    assert_eq!(replies[0].0, deferred_turn);
    assert_eq!(replies[1].0, second_defer_turn);
    assert_eq!(
        replies[1].1,
        rustain::domain::models::ReplyKind::Defer { minutes: 30 }
    );
    assert_eq!(replies[2].0, answer_turn);
    assert_eq!(replies[2].1, rustain::domain::models::ReplyKind::Answer);
    assert_eq!(replies[2].2, "answer");
    let thread = rpc(
        &host.endpoint,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 79,
            "method": "x-rustain-items/thread",
            "params": {"itemId": item_id},
        }),
    )
    .await;
    let turns = thread["result"]["turns"]
        .as_array()
        .expect("served thread turns");
    let deferred = turns
        .iter()
        .find(|turn| turn["turnId"] == deferred_turn)
        .expect("wire thread retains defer");
    assert_eq!(deferred["kind"], "reply");
    assert_eq!(deferred["replyKind"], "defer");
    assert_eq!(deferred["deferMinutes"], 30);
    let answer = turns
        .iter()
        .find(|turn| turn["turnId"] == answer_turn)
        .expect("wire thread retains answer");
    assert_eq!(answer["kind"], "reply");
    assert!(answer.get("replyKind").is_none());
    let second_deferred = turns
        .iter()
        .find(|turn| turn["turnId"] == second_defer_turn)
        .expect("wire thread retains second defer");
    assert_eq!(second_deferred["replyKind"], "defer");
    assert_eq!(second_deferred["deferMinutes"], 30);
    let turn_ids: Vec<_> = turns
        .iter()
        .filter_map(|turn| turn["turnId"].as_str())
        .collect();
    assert!(
        turn_ids.iter().position(|id| *id == deferred_turn).unwrap()
            < turn_ids
                .iter()
                .position(|id| *id == second_defer_turn)
                .unwrap()
            && turn_ids
                .iter()
                .position(|id| *id == second_defer_turn)
                .unwrap()
                < turn_ids.iter().position(|id| *id == answer_turn).unwrap()
    );
    drop(socket);
    let restarted = host.restart().await;
    let after_restart = rpc(
        &restarted.endpoint,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 80,
            "method": "x-rustain-items/thread",
            "params": {"itemId": item_id},
        }),
    )
    .await;
    let mut reopened_sender = new_sender(vec![restarted.peer("defer")]).await;
    reopened_sender
        .type_command(&mut state, &format!("/team thread defer {item_id}"))
        .await;
    let viewed = tokio::time::timeout(SETTLE, reopened_sender.domain_rx.recv())
        .await
        .expect("production thread read completes")
        .expect("thread-read event");
    let AppEvent::TeamThreadReady {
        conversation_id,
        message,
    } = viewed
    else {
        panic!("thread view publishes TeamThreadReady: {viewed:?}");
    };
    let mut tabs = rustain::domain::models::tab::TabManager::new(CancellationToken::new());
    rustain::infrastructure::runtime::transparency_bridge::team_thread_ready(
        "conv",
        &mut state,
        &mut tabs,
        &conversation_id,
        message,
    );
    let rendered = &state.feedback_blocks["team-thread"];
    assert!(rendered.message.contains(&item_id));
    assert!(rendered.message.contains("defer"));
    assert!(rendered.message.contains("30"));
    assert!(rendered.message.contains("answer"));
    assert_eq!(
        room_events(&restarted.journal)
            .await
            .iter()
            .filter(|(_, event)| matches!(event, RoomEvent::RecipientItemAcknowledged { .. }))
            .count(),
        1,
        "reading an item thread leaves the recipient's prior acknowledgement history unchanged"
    );
    assert_eq!(after_restart["result"]["turns"], thread["result"]["turns"]);
    restarted.cancel.cancel();
    restarted
        .http
        .await
        .expect("restarted host task")
        .expect("shutdown");
}

struct FailReplyAppend {
    inner: Arc<dyn RoomJournal>,
}

#[async_trait::async_trait]
impl RoomJournal for FailReplyAppend {
    async fn record_event(
        &self,
        event: RoomEvent,
    ) -> Result<(), rustain::domain::ports::RoomJournalError> {
        if matches!(event, RoomEvent::RecipientItemReplied { .. }) {
            return Err(rustain::domain::ports::RoomJournalError::Append(
                "injected reply append failure".to_owned(),
            ));
        }
        self.inner.record_event(event).await
    }
}

#[tokio::test]
async fn k03_failed_defer_append_is_not_confirmed_and_has_no_reply_event() {
    let mut options = HostOptions::new(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait);
    options.attach_wrap = Some(Arc::new(|inner| Arc::new(FailReplyAppend { inner })));
    let mut host = Host::serve_with(options).await;
    let mut sender = new_sender(vec![host.peer("failed-defer")]).await;
    let mut state = TuiState::new(130, 30);
    sender
        .type_command(
            &mut state,
            "/team send --type consultation failed-defer failure case",
        )
        .await;
    settle_send(&mut sender, &mut state).await;
    let (item_id, _, _) = received(&room_events(&host.journal).await)
        .into_iter()
        .next()
        .expect("actual incoming item");
    let socket = SocketService::start(&mut host).await;
    let (mut writer, _, _) = socket.attach(ConnectionTier::TrustedLocal, false).await;
    write_frame(
        &mut writer,
        &ClientFrame::DeferRecipientItem {
            item_id: item_id.clone(),
        },
    )
    .await
    .expect("send actual defer frame");
    let (action, target, outcome) = recipient_action_result(&mut writer, None).await;
    assert_eq!(action, RecipientAction::Defer);
    assert_eq!(target, RecipientActionTarget::Item { item_id });
    assert!(matches!(
        outcome,
        RecipientActionOutcome::Refused {
            error: ProtocolError::Internal(message),
        } if message.contains("injected reply append failure")
    ));
    assert!(
        room_events(&host.journal)
            .await
            .iter()
            .all(|(_, event)| !matches!(event, RoomEvent::RecipientItemReplied { .. })),
        "failed appends leave no reply turn"
    );
}

#[tokio::test]
async fn k03_removed_http_received_item_refuses_socket_defer_without_reply() {
    let mut host = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut sender = new_sender(vec![host.peer("removed-defer")]).await;
    let mut state = TuiState::new(130, 30);
    sender
        .type_command(
            &mut state,
            "/team send --type consultation removed-defer question",
        )
        .await;
    settle_send(&mut sender, &mut state).await;
    let (item_id, _, _) = received(&room_events(&host.journal).await)
        .into_iter()
        .next()
        .expect("actual incoming item");
    let socket = SocketService::start(&mut host).await;
    let (mut writer, _, _) = socket.attach(ConnectionTier::TrustedLocal, false).await;
    write_frame(
        &mut writer,
        &ClientFrame::RemoveRecipientItem {
            item_id: item_id.clone(),
        },
    )
    .await
    .expect("remove actual incoming item");
    write_frame(
        &mut writer,
        &ClientFrame::DeferRecipientItem {
            item_id: item_id.clone(),
        },
    )
    .await
    .expect("send defer for removed item");
    let (action, target, outcome) = recipient_action_result(&mut writer, None).await;
    assert_eq!(action, RecipientAction::Defer);
    assert_eq!(
        target,
        RecipientActionTarget::Item {
            item_id: item_id.clone()
        }
    );
    assert!(matches!(
        outcome,
        RecipientActionOutcome::Refused {
            error: ProtocolError::Malformed(message),
        } if message == "recipient item already removed"
    ));
    assert!(
        room_events(&host.journal)
            .await
            .iter()
            .all(|(_, event)| !matches!(event, RoomEvent::RecipientItemReplied { .. }))
    );
}

#[tokio::test]
async fn k03_peer_tier_socket_defer_is_refused_without_reply() {
    let mut host = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut sender = new_sender(vec![host.peer("peer-tier-defer")]).await;
    let mut state = TuiState::new(130, 30);
    sender
        .type_command(
            &mut state,
            "/team send --type consultation peer-tier-defer question",
        )
        .await;
    settle_send(&mut sender, &mut state).await;
    let (item_id, _, _) = received(&room_events(&host.journal).await)
        .into_iter()
        .next()
        .expect("actual incoming item");
    let socket = SocketService::start(&mut host).await;
    let (mut peer, _, _) = socket.attach(ConnectionTier::Peer, false).await;
    write_frame(
        &mut peer,
        &ClientFrame::DeferRecipientItem {
            item_id: item_id.clone(),
        },
    )
    .await
    .expect("send peer-tier defer frame");
    let (action, target, outcome) = recipient_action_result(&mut peer, None).await;
    assert_eq!(action, RecipientAction::Defer);
    assert_eq!(
        target,
        RecipientActionTarget::Item {
            item_id: item_id.clone()
        }
    );
    assert!(matches!(
        outcome,
        RecipientActionOutcome::Refused {
            error: ProtocolError::PeerVerification(_),
        }
    ));
    assert!(
        room_events(&host.journal)
            .await
            .iter()
            .all(|(_, event)| !matches!(event, RoomEvent::RecipientItemReplied { .. }))
    );
}

#[tokio::test]
async fn k03_real_wait_host_accepts_defer_and_subsequent_answer() {
    let mut host = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut sender = new_sender(vec![host.peer("wait-defer")]).await;
    let mut state = TuiState::new(130, 30);
    sender
        .type_command(
            &mut state,
            "/team send --type consultation wait-defer question",
        )
        .await;
    settle_send(&mut sender, &mut state).await;
    let (item_id, task, _) = received(&room_events(&host.journal).await)
        .into_iter()
        .next()
        .expect("actual wait-host item");
    let node = waiting_node(&host, &task).await;
    let socket = SocketService::start(&mut host).await;
    let (mut writer, _, _) = socket.attach(ConnectionTier::TrustedLocal, false).await;
    write_frame(
        &mut writer,
        &ClientFrame::DeferRecipientItem {
            item_id: item_id.clone(),
        },
    )
    .await
    .expect("send wait-host defer frame");
    let (action, target, outcome) = recipient_action_result(&mut writer, None).await;
    assert_eq!(action, RecipientAction::Defer);
    assert_eq!(
        target,
        RecipientActionTarget::Item {
            item_id: item_id.clone()
        }
    );
    let defer_receipt = match outcome {
        RecipientActionOutcome::Applied {
            reply: Some(receipt),
        } => receipt,
        other => panic!("wait-host defer must be durably applied: {other:?}"),
    };
    assert_eq!(
        defer_receipt.kind,
        rustain::domain::models::ReplyKind::Defer { minutes: 30 }
    );
    let task_before = rpc(
        &host.endpoint,
        serde_json::json!({"jsonrpc":"2.0","id":81,"method":"tasks/get","params":{"id":task}}),
    )
    .await;
    let still_waiting = rpc(
        &host.endpoint,
        serde_json::json!({"jsonrpc":"2.0","id":82,"method":"tasks/get","params":{"id":task}}),
    )
    .await;
    assert_eq!(
        still_waiting["result"]["status"]["state"],
        task_before["result"]["status"]["state"]
    );
    assert!(host.node_tree.list().await.iter().any(|entry| {
        entry.agent_id.as_str() == node
            && entry.current_status == rustain::domain::models::NodeState::Waiting
    }));
    write_frame(
        &mut writer,
        &ClientFrame::ReplyToRecipientItem {
            item_id: item_id.clone(),
            text: "answer after wait".to_owned(),
        },
    )
    .await
    .expect("answer wait-host item");
    let (action, target, outcome) = recipient_action_result(&mut writer, None).await;
    assert_eq!(action, RecipientAction::Reply);
    assert_eq!(
        target,
        RecipientActionTarget::Item {
            item_id: item_id.clone()
        }
    );
    let reply_receipt = match outcome {
        RecipientActionOutcome::Applied {
            reply: Some(receipt),
        } => receipt,
        other => panic!("wait-host answer must be durably applied: {other:?}"),
    };
    assert_eq!(reply_receipt.item_id, item_id);
    assert_eq!(
        reply_receipt.kind,
        rustain::domain::models::ReplyKind::Answer
    );
    let events = room_events(&host.journal).await;
    assert!(events.iter().any(|(_, event)| matches!(
        event,
        RoomEvent::RecipientItemReplied {
            via_node: Some(reply_node),
            ..
        } if reply_node.as_str() == node
    )));
    let terminal = poll_state(&host.endpoint, &task, "completed").await;
    assert_eq!(terminal["result"]["status"]["state"], "completed");
}

#[tokio::test]
async fn k03_socket_acknowledgement_race_is_correlated_and_appends_once() {
    let mut host = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut sender = new_sender(vec![host.peer("ack-race")]).await;
    let mut state = TuiState::new(130, 30);
    sender
        .type_command(
            &mut state,
            "/team send --type consultation ack-race question",
        )
        .await;
    settle_send(&mut sender, &mut state).await;
    let (item_id, _, _) = received(&room_events(&host.journal).await)
        .into_iter()
        .next()
        .expect("actual incoming item");
    let socket = SocketService::start(&mut host).await;
    let (mut writer, _, _) = socket.attach(ConnectionTier::TrustedLocal, false).await;
    for _ in 0..2 {
        write_frame(
            &mut writer,
            &ClientFrame::AcknowledgeRecipientItem {
                item_id: item_id.clone(),
            },
        )
        .await
        .expect("send acknowledgement");
    }
    let first = recipient_action_result(&mut writer, None).await;
    let second = recipient_action_result(&mut writer, None).await;
    for (action, target, _) in [&first, &second] {
        assert_eq!(*action, RecipientAction::Acknowledge);
        assert_eq!(
            target,
            &RecipientActionTarget::Item {
                item_id: item_id.clone()
            }
        );
    }
    assert_eq!(first.2, RecipientActionOutcome::Applied { reply: None });
    assert_eq!(second.2, RecipientActionOutcome::Idempotent);
    let acknowledgements = room_events(&host.journal)
        .await
        .iter()
        .filter(|(_, event)| matches!(event, RoomEvent::RecipientItemAcknowledged { .. }))
        .count();
    assert_eq!(acknowledgements, 1);
    drop(socket);
    host.cancel.cancel();
    host.http.await.expect("host task").expect("HTTP shutdown");
}

#[tokio::test]
async fn k06_wait_host_restart_keeps_defer_and_direct_answer_without_reviving_task() {
    let mut host = Host::serve(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let socket = SocketService::start(&mut host).await;
    let mut sender = new_sender(vec![host.peer("restart-answer")]).await;
    let mut state = TuiState::new(130, 30);
    sender
        .type_command(
            &mut state,
            "/team send --type consultation restart-answer question before restart",
        )
        .await;
    settle_send(&mut sender, &mut state).await;
    let (mut writer, _, snapshot) = socket.attach(ConnectionTier::TrustedLocal, false).await;
    let control = snapshot.peer_row_controls.values().next().unwrap();
    let item_id = control
        .item_address
        .as_ref()
        .unwrap()
        .item()
        .as_str()
        .to_owned();
    let node = control.node_id.clone();
    let task = received(&room_events(&host.journal).await)
        .into_iter()
        .find(|(id, _, _)| id == &item_id)
        .unwrap()
        .1;
    assert_eq!(
        waiting_node(&host, &task).await,
        node,
        "restart only after the real inbound task has durably reached Waiting"
    );
    write_frame(
        &mut writer,
        &ClientFrame::DeferRecipientItem {
            item_id: item_id.clone(),
        },
    )
    .await
    .unwrap();
    let (action, target, outcome) = recipient_action_result(&mut writer, None).await;
    assert_eq!(action, RecipientAction::Defer);
    assert_eq!(
        target,
        RecipientActionTarget::Item {
            item_id: item_id.clone()
        }
    );
    let defer_receipt = match outcome {
        RecipientActionOutcome::Applied {
            reply: Some(receipt),
        } => receipt,
        other => panic!("defer before restart must be durably applied: {other:?}"),
    };
    assert_eq!(
        defer_receipt.kind,
        rustain::domain::models::ReplyKind::Defer { minutes: 30 }
    );
    drop(writer);
    drop(socket);
    let mut host = host.restart().await;
    let failed = rpc(
        &host.endpoint,
        serde_json::json!({
            "jsonrpc":"2.0","id":90,"method":"tasks/get","params":{"id":task},
        }),
    )
    .await;
    assert_eq!(
        failed["result"]["status"]["state"],
        "failed",
        "restarted tasks/get must preserve the failed predecessor: {failed}; nodes: {:?}",
        host.node_tree.list().await
    );
    let socket = SocketService::start(&mut host).await;
    let (mut writer, _, snapshot) = socket.attach(ConnectionTier::TrustedLocal, false).await;
    let control = snapshot
        .peer_row_controls
        .values()
        .find(|control| control.node_id == node)
        .unwrap();
    assert!(
        !control.node_resolvable,
        "a persisted wait row is not a live restarted A2A task"
    );
    write_frame(
        &mut writer,
        &ClientFrame::ReplyToRecipientItem {
            item_id: item_id.clone(),
            text: "direct answer after restart".to_owned(),
        },
    )
    .await
    .unwrap();
    let (action, target, outcome) = recipient_action_result(&mut writer, None).await;
    assert_eq!(action, RecipientAction::Reply);
    assert_eq!(
        target,
        RecipientActionTarget::Item {
            item_id: item_id.clone()
        }
    );
    let reply_receipt = match outcome {
        RecipientActionOutcome::Applied {
            reply: Some(receipt),
        } => receipt,
        other => panic!("direct answer after restart must be durably applied: {other:?}"),
    };
    assert_eq!(reply_receipt.item_id, item_id);
    assert_eq!(
        reply_receipt.kind,
        rustain::domain::models::ReplyKind::Answer
    );
    let failed_after = rpc(
        &host.endpoint,
        serde_json::json!({
            "jsonrpc":"2.0","id":91,"method":"tasks/get","params":{"id":task},
        }),
    )
    .await;
    assert_eq!(
        failed_after["result"]["status"], failed["result"]["status"],
        "direct item reply cannot revive or complete the failed predecessor task"
    );
    let events = room_events(&host.journal).await;
    let replies: Vec<_> = events
        .iter()
        .filter_map(|(_, event)| match event {
            RoomEvent::RecipientItemReplied {
                kind,
                content,
                via_node,
                ..
            } => Some((kind, content.as_str(), via_node)),
            _ => None,
        })
        .collect();
    assert_eq!(replies.len(), 2);
    assert!(matches!(
        replies[0],
        (
            rustain::domain::models::ReplyKind::Defer { minutes: 30 },
            "Busy — defer 30m",
            None
        )
    ));
    assert!(matches!(
        replies[1],
        (
            rustain::domain::models::ReplyKind::Answer,
            "direct answer after restart",
            None
        )
    ));
    assert!(
        !events
            .iter()
            .any(|(_, event)| matches!(event, RoomEvent::PeerDraftResolved { .. }))
    );
    history_fence(&mut writer).await;
    let conversation = host
        .storage
        .load_conversation(&snapshot.conversation_id)
        .await
        .unwrap()
        .unwrap();
    let control = conversation
        .peer_row_controls
        .values()
        .find(|control| control.node_id == node)
        .unwrap();
    assert_eq!(control.stage, PeerRowStage::Settled);
    assert!(!control.node_resolvable);
    assert!(
        conversation
            .messages
            .iter()
            .any(|message| message.content == "direct answer after restart"
                && message.authorship == rustain::domain::models::MessageAuthorship::HumanWritten)
    );
}

#[path = "conformance_19_19/k07_view.rs"]
mod k07_view;
#[path = "conformance_19_19/k08_ownership.rs"]
mod k08_ownership;
#[path = "conformance_19_19/k09_actions.rs"]
mod k09_actions;
#[path = "conformance_19_19/k10_consent.rs"]
mod k10_consent;
#[path = "conformance_19_19/k10_context.rs"]
mod k10_context;
#[path = "conformance_19_19/k10_tools.rs"]
mod k10_tools;
#[path = "conformance_19_19/k11_authority.rs"]
mod k11_authority;
async fn attach_user_turn(stream: &mut tokio::net::UnixStream, text: &str) {
    write_frame(
        stream,
        &ClientFrame::UserMessage {
            text: text.to_owned(),
            images: Vec::new(),
        },
    )
    .await
    .expect("actual socket user turn");
}
