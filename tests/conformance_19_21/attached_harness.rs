//! Shared real-socket fixture for Story 19.21 distribution and 19.21g proposals.
//! All socket and HTTP work enters through production `AttachServer::run` and
//! the production signed-card A2A server. Fixture timeouts are hang guards only.

use crate::harness::Journal;
use arc_swap::ArcSwap;
use base64::Engine as _;
use futures::{StreamExt, stream::BoxStream};
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::a2a::auth::{A2aServerSecurity, API_KEY_HEADER, resolve_server_auth};
use rustain::adapters::a2a::card_cache::SignedCardCache;
use rustain::adapters::a2a::config::{
    parse_workspace_a2a_config, parse_workspace_a2a_server_config,
};
use rustain::adapters::a2a::egress::A2aEgress;
use rustain::adapters::a2a::item_capability::load_item_capability_key;
use rustain::adapters::a2a::server::{ServeConfig, serve};
use rustain::adapters::a2a::transparency::TransparencySink;
use rustain::adapters::daemon::protocol::{
    AttachMode, ClientFrame, ConnectionTier, DaemonFrame, PROTOCOL_VERSION, RecipientActionOutcome,
    RecipientActionTarget, TeamBoardRequest, read_frame, write_frame,
};
use rustain::adapters::daemon::runtime::{DaemonCore, DaemonTurnRuntime};
use rustain::adapters::daemon::server::AttachServer;
#[cfg(feature = "test-instrumentation")]
use rustain::adapters::daemon::server::TeamSendTestHooks;
use rustain::adapters::filesystem::FileSystemStorage;
use rustain::adapters::noop::{NoOpMemory, NoOpPersona, NoOpSecurity, NoOpUsageLedger};
use rustain::adapters::policy::JournalConsentProjection;
use rustain::adapters::rap::{AgentSigner, IdentityKeyStore};
use rustain::domain::errors::ProviderError;
use rustain::domain::models::capability_registry::CapabilityRegistry;
use rustain::domain::models::{
    AppConfig, ApprovalOutcome, ApprovalScope, CompletionOptions, Conversation, Message,
    PeerIdentity, PolicyPrincipal, PrincipalKind, RequestId, ResponseMode, RoomEvent, StopReason,
    StreamChunk,
};
use rustain::domain::ports::{
    ConsentProjectionQuery, ConsentSlot, ConsentState, DeliveryPolicy, EffectiveDeliveryPolicy,
    ProbeOutcome, RoomJournal, RoomJournalError, SecurityPort, StoragePort, StreamingProvider,
    ToolSetPort,
};
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::infrastructure::paths::sessions_dir;
use rustain::infrastructure::subagent::NodeTree;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, UnixListener, UnixStream};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub const FIXTURE_TIMEOUT: Duration = Duration::from_secs(20);
pub type ProviderRequestTrace = Arc<parking_lot::Mutex<Vec<(Vec<Message>, CompletionOptions)>>>;

/// Queued provider exchanges still run through the real daemon turn/scheduler.
/// Each enqueue includes its tool-result continuation, so later turns cannot
/// accidentally consume an earlier turn's response.
#[derive(Default)]
pub struct ToolScript {
    exchanges: parking_lot::Mutex<std::collections::VecDeque<Vec<StreamChunk>>>,
}
impl ToolScript {
    /// Queue one response without a continuation for interleaving or interruption.
    #[cfg(any(feature = "cron", feature = "test-instrumentation"))]
    pub fn exchange(&self, chunks: Vec<StreamChunk>) {
        self.exchanges.lock().push_back(chunks);
    }

    pub fn tool(&self, name: &str, input: serde_json::Value) -> String {
        let mut exchanges = self.exchanges.lock();
        let id = RequestId::new().0;
        exchanges.push_back(vec![
            StreamChunk::ToolUse {
                id: id.clone(),
                name: name.into(),
                input,
            },
            StreamChunk::TurnComplete {
                stop_reason: StopReason::ToolUse,
            },
        ]);
        exchanges.push_back(vec![
            StreamChunk::Text {
                content: "Scripted tool turn completed.".into(),
                parent_tool_use_id: None,
            },
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
        ]);
        id
    }
}

/// Real daemon-side provider used only by the turn runtime factory. Its trace is
/// per daemon and its calls are not shared with recipients or standalone rails.
struct ObservedProvider {
    requests: ProviderRequestTrace,
    turns: Arc<std::sync::atomic::AtomicUsize>,
    script: Arc<ToolScript>,
}

#[async_trait::async_trait]
impl StreamingProvider for ObservedProvider {
    async fn stream_completion(
        &self,
        messages: Vec<Message>,
        options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        self.requests.lock().push((messages, options));
        self.turns.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        if let Some(chunks) = self.script.exchanges.lock().pop_front() {
            return Ok(futures::stream::iter(chunks).boxed());
        }
        Ok(futures::stream::iter(vec![
            StreamChunk::Text {
                content: "attached-fixture response".into(),
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
        "attached-fixture-provider".into()
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

fn daemon_runtime(
    provider: Arc<dyn StreamingProvider>,
    storage: Arc<dyn StoragePort>,
    workspace: &Path,
    egress: Option<&A2aEgress>,
    proposals: Arc<rustain::adapters::daemon::team_proposal::TeamProposalRegistry>,
    app_config: Arc<ArcSwap<AppConfig>>,
    security: Arc<dyn SecurityPort>,
) -> Arc<DaemonTurnRuntime> {
    let builtin = Arc::new(rustain::adapters::toolset_adapter::ToolSetAdapter::new(
        workspace.to_path_buf(),
        storage.clone(),
        Arc::new(ArcSwap::from_pointee(
            Arc::new(rustain::adapters::sandbox::NoOpSandbox)
                as Arc<dyn rustain::domain::ports::SandboxManager>,
        )),
        Arc::new(tokio::sync::RwLock::new(
            rustain::domain::models::SandboxPolicy::Permissive,
        )),
    ));
    let composite = Arc::new(
        rustain::adapters::composite_toolset_adapter::CompositeToolsetAdapter::new(
            builtin,
            vec![],
            vec![],
            true,
            None,
            Some(Arc::new(
                rustain::adapters::skill_activation::SkillActivator::new(),
            )),
            None,
        ),
    );
    if let Some(egress) = egress {
        egress.install(&composite, Some(proposals));
    }
    let tools: Arc<dyn ToolSetPort> = composite;
    let persistence = Arc::new(
        rustain::adapters::approval_persistence_toml::ApprovalPersistenceToml::new(
            workspace.join(".rustain-test-home/config.toml"),
            workspace.join(".rustain/permissions.toml"),
        ),
    );
    let approval = ApprovalRuntime::new_with_subagent_policy(
        64,
        persistence,
        app_config.load().subagents.auto_approve,
    );
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
        app_config,
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

/// A journal decorator with deterministic append observation and an optional
/// hold. `before_append` is signalled before the inner durable append; it is not
/// itself evidence of durability. `release_append` opens the held append.
pub struct AppendGateJournal {
    pub inner: Arc<dyn RoomJournal>,
    matches: fn(&RoomEvent) -> bool,
    seen: parking_lot::Mutex<Vec<RoomEvent>>,
    observed: Notify,
    release: Notify,
    hold: bool,
    matched_count: std::sync::atomic::AtomicUsize,
    reject: parking_lot::Mutex<Option<(usize, String)>>,
    durable: parking_lot::Mutex<Vec<RoomEvent>>,
    durable_observed: Notify,
}
impl AppendGateJournal {
    pub fn new(inner: Arc<dyn RoomJournal>, matches: fn(&RoomEvent) -> bool, hold: bool) -> Self {
        Self {
            inner,
            matches,
            seen: parking_lot::Mutex::new(Vec::new()),
            observed: Notify::new(),
            release: Notify::new(),
            hold,
            matched_count: std::sync::atomic::AtomicUsize::new(0),
            reject: parking_lot::Mutex::new(None),
            durable: parking_lot::Mutex::new(Vec::new()),
            durable_observed: Notify::new(),
        }
    }
    /// Reject the selected zero-based matching append after recording the
    /// attempted event and releasing any configured gate.
    pub fn reject_matching_occurrence(&self, occurrence: usize, message: impl Into<String>) {
        *self.reject.lock() = Some((occurrence, message.into()));
    }
    pub fn seen(&self) -> Vec<RoomEvent> {
        self.seen.lock().clone()
    }
    pub fn durable_seen(&self) -> Vec<RoomEvent> {
        self.durable.lock().clone()
    }
    pub async fn wait_for(&self, count: usize) {
        tokio::time::timeout(FIXTURE_TIMEOUT, async {
            loop {
                let notified = self.observed.notified();
                if self.seen.lock().len() >= count {
                    return;
                }
                notified.await;
            }
        })
        .await
        .expect("fixture append observation deadline");
    }
    /// Wait until this many selected appends have succeeded in the real inner journal.
    pub async fn wait_durable_for(&self, count: usize) {
        tokio::time::timeout(FIXTURE_TIMEOUT, async {
            loop {
                let notified = self.durable_observed.notified();
                if self.durable.lock().len() >= count {
                    return;
                }
                notified.await;
            }
        })
        .await
        .expect("fixture durable append deadline");
    }
    pub fn release_append(&self) {
        self.release.notify_waiters();
        self.release.notify_one();
    }
}
#[async_trait::async_trait]
impl RoomJournal for AppendGateJournal {
    async fn record_event(&self, event: RoomEvent) -> Result<(), RoomJournalError> {
        let matching = (self.matches)(&event);
        if matching {
            let occurrence = self
                .matched_count
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            self.seen.lock().push(event.clone());
            self.observed.notify_waiters();
            if self.hold {
                self.release.notified().await;
            }
            if let Some((reject_at, message)) = self.reject.lock().clone()
                && occurrence == reject_at
            {
                return Err(RoomJournalError::Append(message));
            }
        }
        self.inner.record_event(event.clone()).await?;
        if matching {
            self.durable.lock().push(event);
            self.durable_observed.notify_waiters();
        }
        Ok(())
    }
}

/// Predicate for consent grant/revocation observation or append gating.
pub fn is_consent_event(event: &RoomEvent) -> bool {
    matches!(
        event,
        RoomEvent::ConsentGranted { .. } | RoomEvent::ConsentRevoked { .. }
    )
}

/// Named alias emphasizing the held recipient-append use case.
pub type GateRoomJournal = AppendGateJournal;

#[cfg(feature = "test-instrumentation")]
struct ScriptedRuntimeOptions {
    config: AppConfig,
    install_egress: bool,
}

/// One real daemon composition: one AppEvent channel, real journal/tree/core,
/// one injected-clock A2aEgress and its original runtime, then the production
/// attached-team constructor and a real Unix listener.
pub struct AttachedDaemon {
    pub journal: Journal,
    pub room_journal: Arc<dyn RoomJournal>,
    pub consent_projection: Arc<JournalConsentProjection>,
    pub server: Arc<AttachServer>,
    pub egress: Arc<A2aEgress>,
    pub requests: ProviderRequestTrace,
    pub model_turns: Arc<std::sync::atomic::AtomicUsize>,
    pub script: Arc<ToolScript>,
    pub proposals: Arc<rustain::adapters::daemon::team_proposal::TeamProposalRegistry>,
    #[cfg(feature = "cron")]
    pub core: Arc<DaemonCore>,
    pub runtime: Arc<DaemonTurnRuntime>,
    pub conversation: Arc<tokio::sync::Mutex<Conversation>>,
    pub domain_tx: mpsc::UnboundedSender<rustain::domain::events::AppEvent>,
    pub channel_tx: mpsc::UnboundedSender<rustain::domain::models::ChannelTurnRequest>,
    approval_events: tokio::sync::Mutex<
        tokio::sync::broadcast::Receiver<
            rustain::domain::services::approval_runtime::ApprovalRuntimeEvent,
        >,
    >,
    observed_approval_events:
        parking_lot::Mutex<Vec<rustain::domain::services::approval_runtime::ApprovalRuntimeEvent>>,
    approval_prompt_cursors: parking_lot::Mutex<HashMap<rustain::domain::models::PeerId, usize>>,
    #[cfg(feature = "test-instrumentation")]
    pub team_send_hooks: TeamSendTestHooks,
    pub path: PathBuf,
    shutdown: CancellationToken,
    task: JoinHandle<()>,
    _domain_rx: Option<mpsc::UnboundedReceiver<rustain::domain::events::AppEvent>>,
    _keys: tempfile::TempDir,
}
impl AttachedDaemon {
    pub async fn start(peers: Vec<rustain::domain::models::A2aPeerSpec>) -> Self {
        let journal = Journal::open().await;
        let sink = journal.room.clone();
        Self::compose(journal, peers, sink).await
    }
    pub async fn start_with_board_now(
        peers: Vec<rustain::domain::models::A2aPeerSpec>,
        board_now: Arc<dyn Fn() -> Instant + Send + Sync>,
    ) -> Self {
        let journal = Journal::open().await;
        let sink = journal.room.clone();
        Self::compose_with_board_now(journal, peers, sink, board_now).await
    }
    pub async fn start_with_journal(
        journal: Journal,
        peers: Vec<rustain::domain::models::A2aPeerSpec>,
        wrapped: Option<Arc<dyn RoomJournal>>,
    ) -> Self {
        let sink = wrapped.unwrap_or_else(|| journal.room.clone());
        Self::compose(journal, peers, sink).await
    }
    #[cfg(feature = "test-instrumentation")]
    pub async fn compose(
        journal: Journal,
        peers: Vec<rustain::domain::models::A2aPeerSpec>,
        room_journal: Arc<dyn RoomJournal>,
    ) -> Self {
        let clock = Arc::new(rustain::domain::clock::MockClock::at_wall_ms(
            1_800_000_000_000,
        ));
        Self::compose_with_clock(journal, peers, room_journal, clock).await
    }
    pub async fn compose_with_clock(
        journal: Journal,
        peers: Vec<rustain::domain::models::A2aPeerSpec>,
        room_journal: Arc<dyn RoomJournal>,
        clock: Arc<rustain::domain::clock::MockClock>,
    ) -> Self {
        Self::compose_with_clock_and_board_now(
            journal,
            peers,
            room_journal,
            clock,
            Arc::new(Instant::now),
        )
        .await
    }
    pub async fn compose_with_board_now(
        journal: Journal,
        peers: Vec<rustain::domain::models::A2aPeerSpec>,
        room_journal: Arc<dyn RoomJournal>,
        board_now: Arc<dyn Fn() -> Instant + Send + Sync>,
    ) -> Self {
        let clock = Arc::new(rustain::domain::clock::MockClock::at_wall_ms(
            1_800_000_000_000,
        ));
        Self::compose_with_clock_and_board_now(journal, peers, room_journal, clock, board_now).await
    }
    pub async fn compose_with_clock_and_board_now(
        journal: Journal,
        peers: Vec<rustain::domain::models::A2aPeerSpec>,
        room_journal: Arc<dyn RoomJournal>,
        clock: Arc<rustain::domain::clock::MockClock>,
        board_now: Arc<dyn Fn() -> Instant + Send + Sync>,
    ) -> Self {
        #[cfg(feature = "test-instrumentation")]
        {
            let hooks = TeamSendTestHooks::new(None, None, false).with_board_now_source(board_now);
            Self::compose_with_clock_and_test_hooks(journal, peers, room_journal, clock, hooks)
                .await
        }
        #[cfg(not(feature = "test-instrumentation"))]
        {
            let _ = (journal, peers, room_journal, clock, board_now);
            panic!("attached fixture requires the test-instrumentation feature")
        }
    }
    #[cfg(feature = "test-instrumentation")]
    pub async fn start_with_test_hooks(
        peers: Vec<rustain::domain::models::A2aPeerSpec>,
        hooks: TeamSendTestHooks,
    ) -> Self {
        let journal = Journal::open().await;
        let sink = journal.room.clone();
        let clock = Arc::new(rustain::domain::clock::MockClock::at_wall_ms(
            1_800_000_000_000,
        ));
        Self::compose_with_clock_and_test_hooks(journal, peers, sink, clock, hooks).await
    }
    #[cfg(feature = "test-instrumentation")]
    pub async fn compose_with_clock_and_test_hooks(
        journal: Journal,
        peers: Vec<rustain::domain::models::A2aPeerSpec>,
        room_journal: Arc<dyn RoomJournal>,
        clock: Arc<rustain::domain::clock::MockClock>,
        test_hooks: TeamSendTestHooks,
    ) -> Self {
        Self::compose_with_clock_and_test_hooks_and_policy(
            journal,
            peers,
            room_journal,
            clock,
            test_hooks,
            None,
        )
        .await
    }
    #[cfg(feature = "test-instrumentation")]
    pub async fn compose_with_clock_and_test_hooks_and_policy(
        journal: Journal,
        peers: Vec<rustain::domain::models::A2aPeerSpec>,
        room_journal: Arc<dyn RoomJournal>,
        clock: Arc<rustain::domain::clock::MockClock>,
        test_hooks: TeamSendTestHooks,
        effective_policy: Option<Arc<rustain::domain::models::EffectivePolicy>>,
    ) -> Self {
        Self::compose_scripted(
            journal,
            peers,
            room_journal,
            clock,
            test_hooks,
            effective_policy,
            ScriptedRuntimeOptions {
                config: AppConfig::default(),
                install_egress: true,
            },
        )
        .await
    }
    #[cfg(feature = "test-instrumentation")]
    pub async fn start_scripted(
        peers: Vec<rustain::domain::models::A2aPeerSpec>,
        config: AppConfig,
        install_egress: bool,
        hooks: TeamSendTestHooks,
    ) -> Self {
        let journal = Journal::open().await;
        let room = journal.room.clone();
        Self::compose_scripted(
            journal,
            peers,
            room,
            Arc::new(rustain::domain::clock::MockClock::at_wall_ms(
                1_800_000_000_000,
            )),
            hooks,
            None,
            ScriptedRuntimeOptions {
                config,
                install_egress,
            },
        )
        .await
    }
    #[cfg(feature = "test-instrumentation")]
    async fn compose_scripted(
        journal: Journal,
        peers: Vec<rustain::domain::models::A2aPeerSpec>,
        room_journal: Arc<dyn RoomJournal>,
        clock: Arc<rustain::domain::clock::MockClock>,
        test_hooks: TeamSendTestHooks,
        effective_policy: Option<Arc<rustain::domain::models::EffectivePolicy>>,
        options: ScriptedRuntimeOptions,
    ) -> Self {
        let ScriptedRuntimeOptions {
            config,
            install_egress,
        } = options;
        let workspace = journal.workspace.path().to_path_buf();
        let (domain_tx, domain_rx) = mpsc::unbounded_channel();
        // Default fixtures use the production journal-to-domain-event path.
        // Explicit append-failure/gating wrappers remain caller-owned.
        let room_journal: Arc<dyn RoomJournal> = if Arc::ptr_eq(&room_journal, &journal.room) {
            Arc::new(rustain::infrastructure::subagent::NodeRoomJournal::new(
                journal.node.clone(),
                Some(domain_tx.clone()),
            ))
        } else {
            room_journal
        };
        let node_tree = NodeTree::with_event_tx(
            domain_tx.clone(),
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
        let requests: ProviderRequestTrace = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let model_turns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let egress = Arc::new(
            A2aEgress::compose_with_clock(
                peers.clone(),
                node_tree.clone(),
                room_journal.clone(),
                journal.node.clone(),
                domain_tx.clone(),
                load_item_capability_key(rustain::infrastructure::paths::data_dir()),
                clock,
            )
            .expect("compose one daemon egress"),
        );
        egress.await_cards_settled().await;
        let script = Arc::new(ToolScript::default());
        let proposals =
            Arc::new(rustain::adapters::daemon::team_proposal::TeamProposalRegistry::new());
        let config = Arc::new(ArcSwap::from_pointee(config));
        let security: Arc<dyn SecurityPort> = Arc::new(
            rustain::adapters::security_adapter::SecurityAdapter::new(workspace.clone()),
        );
        let core = {
            let requests = requests.clone();
            let model_turns = model_turns.clone();
            let storage = storage.clone();
            let workspace = workspace.clone();
            let script = script.clone();
            let proposals = proposals.clone();
            let egress = egress.clone();
            let config = config.clone();
            let security = security.clone();
            Arc::new(DaemonCore::new(
                workspace.clone(),
                config.clone(),
                Arc::new(NoOpMemory),
                storage.clone(),
                security.clone(),
                Arc::new(NoOpPersona),
                Arc::new(rustain::adapters::rap::PeerTopicStore::new()),
                Box::new(move || {
                    Ok(daemon_runtime(
                        Arc::new(ObservedProvider {
                            requests: requests.clone(),
                            turns: model_turns.clone(),
                            script: script.clone(),
                        }),
                        storage.clone(),
                        &workspace,
                        install_egress.then_some(egress.as_ref()),
                        proposals.clone(),
                        config.clone(),
                        security.clone(),
                    ))
                }),
            ))
        };
        let projection = Arc::new(JournalConsentProjection::from_entries(
            &journal.entries().await,
        ));
        let effective = effective_policy.unwrap_or_else(|| {
            let (policy, _) = rustain::adapters::policy::resolve_workspace_policy(
                &workspace,
                &peers,
                projection.as_ref(),
            )
            .expect("resolve production workspace interaction policy");
            Arc::new(policy)
        });
        let runtime = core
            .ensure_runtime()
            .await
            .expect("create daemon runtime for consent observations");
        runtime.approval.load_session().await;
        let approval_events = tokio::sync::Mutex::new(runtime.approval.subscribe());
        let delivery: Arc<dyn DeliveryPolicy> =
            Arc::new(EffectiveDeliveryPolicy::new(effective.clone()));
        let bus = rustain::adapters::daemon::server::peer_bus_slot_with_policy(
            &node_tree,
            delivery.clone(),
        );
        let conversation = Arc::new(tokio::sync::Mutex::new(Conversation {
            id: "attached-fixture".into(),
            ..Conversation::default()
        }));
        let server = AttachServer::new_with_node_tree_bus_policy_journal_and_team(
            core.clone(),
            conversation.clone(),
            domain_tx.clone(),
            node_tree,
            bus,
            delivery,
            room_journal.clone(),
            journal.node.clone(),
            Some(projection.clone()),
            Some(egress.runtime().clone()),
            peers,
            proposals.clone(),
            #[cfg(feature = "test-instrumentation")]
            Some(test_hooks.clone()),
        );
        server.configure_consent_policy(effective.clone()).await;
        let path = workspace.join("daemon.sock");
        let listener = UnixListener::bind(&path).expect("bind attached fixture socket");
        let shutdown = CancellationToken::new();
        let (channel_tx, channel_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(server.clone().run(
            listener,
            domain_rx,
            Some(channel_rx),
            None,
            shutdown.child_token(),
        ));
        Self {
            journal,
            room_journal,
            consent_projection: projection,
            server,
            egress,
            requests,
            model_turns,
            script,
            proposals,
            #[cfg(feature = "cron")]
            core,
            runtime,
            conversation,
            domain_tx,
            channel_tx,
            approval_events,
            observed_approval_events: parking_lot::Mutex::new(Vec::new()),
            approval_prompt_cursors: parking_lot::Mutex::new(HashMap::new()),
            #[cfg(feature = "test-instrumentation")]
            team_send_hooks: test_hooks,
            path,
            shutdown,
            task,
            _domain_rx: None,
            _keys: tempfile::tempdir().expect("fixture key directory"),
        }
    }
    pub fn provider_request_count(&self) -> usize {
        self.requests.lock().len()
    }
    pub fn model_turn_count(&self) -> usize {
        self.model_turns.load(std::sync::atomic::Ordering::Acquire)
    }
    /// Return consent prompts newly published since the previous call for this
    /// authenticated pinned sender. A completed message/send response is a
    /// causal fence for the new events; it does not infer from task status.
    pub async fn sender_consent_prompts_for(
        &self,
        peer: &rustain::domain::models::PeerId,
    ) -> Vec<RequestId> {
        use rustain::domain::models::tool_call::ApprovalSource;
        let mut receiver = self.approval_events.lock().await;
        loop {
            match receiver.try_recv() {
                Ok(event) => self.observed_approval_events.lock().push(event),
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(
                    tokio::sync::broadcast::error::TryRecvError::Empty
                    | tokio::sync::broadcast::error::TryRecvError::Closed,
                ) => break,
            }
        }
        let observed = self.observed_approval_events.lock();
        let mut cursors = self.approval_prompt_cursors.lock();
        let cursor = cursors.entry(peer.clone()).or_default();
        let prompts = observed[*cursor..]
            .iter()
            .filter_map(|event| match event {
                rustain::domain::services::approval_runtime::ApprovalRuntimeEvent::Requested {
                    id,
                    source:
                        ApprovalSource::RemotePeer {
                            peer_id: event_peer,
                            ..
                        },
                    tool,
                    ..
                } if tool == "a2a/sender-consent" && event_peer == peer => Some(id.clone()),
                _ => None,
            })
            .collect();
        *cursor = observed.len();
        prompts
    }
    /// Wait for the same consent projection used by the production manager to
    /// reflect a durable pinned-sender grant; the timeout is only a hang guard.
    pub async fn wait_for_pinned_sender_consent(&self, peer: &rustain::domain::models::PeerId) {
        tokio::time::timeout(FIXTURE_TIMEOUT, async {
            while self
                .consent_projection
                .slot_state(ConsentSlot::Pinned, peer)
                != ConsentState::Trusted
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("pinned consent projection deadline");
    }
    /// Wait for real inbound acceptance, not just the external auth-required
    /// response or a consent grant append.
    pub async fn wait_for_received_task(&self, task: &str) {
        tokio::time::timeout(FIXTURE_TIMEOUT, async {
            loop {
                if crate::harness::read_room_events(&self.journal)
                    .await
                    .iter()
                    .any(|(_, event)| {
                        matches!(event, RoomEvent::RecipientItemReceived { task: received, .. } if received == task)
                    })
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("inbound received-task deadline");
    }
    /// Start production inbound A2A HTTP on this same daemon/server and consent
    /// manager, resolving each caller through a written config, both production
    /// parsers, and `resolve_server_auth` against its distinct pinned identity.
    pub async fn serve_http(&self, senders: Vec<InboundSenderIdentity>) -> InboundHttpHost {
        let listener = TcpListener::bind("0.0.0.0:0")
            .await
            .expect("bind daemon inbound A2A listener");
        let port = listener.local_addr().expect("inbound address").port();
        let workspace = self.journal.workspace.path();
        let tls_root = workspace.join(".rustain/attached-inbound-tls");
        std::fs::create_dir_all(&tls_root).expect("create inbound TLS directory");
        let cert_path = tls_root.join("cert.pem");
        let key_path = tls_root.join("key.pem");
        if !cert_path.exists() || !key_path.exists() {
            let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
                .expect("generate inbound server certificate");
            std::fs::write(&cert_path, cert.cert.pem()).expect("write inbound certificate");
            std::fs::write(&key_path, cert.signing_key.serialize_pem())
                .expect("write inbound private key");
        }

        let mut agents = serde_json::Map::new();
        let mut peer_keys = serde_json::Map::new();
        let mut credentials = HashMap::new();
        let mut clients = HashMap::new();
        let mut seen_peers = std::collections::HashSet::new();
        for sender in &senders {
            assert!(
                seen_peers.insert(sender.identity.peer_id.clone()),
                "inbound sender identities must be distinct"
            );
            agents.insert(
                sender.alias.clone(),
                serde_json::json!({
                    "url": format!("https://{}.example/", sender.alias),
                    "pinnedKey": {
                        "alg": "EdDSA",
                        "x": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&sender.identity.public_key),
                    },
                }),
            );
            peer_keys.insert(
                sender.key_env.clone(),
                serde_json::Value::String(sender.alias.clone()),
            );
            credentials.insert(sender.key_env.clone(), sender.secret.clone());
            clients.insert(
                sender.alias.clone(),
                InboundHttpSender::new(
                    sender.secret.clone(),
                    format!("https://localhost:{port}/"),
                    cert_path.clone(),
                ),
            );
        }
        let config_path = rustain::infrastructure::paths::workspace_a2a_config_path(workspace);
        std::fs::create_dir_all(config_path.parent().expect("a2a config directory"))
            .expect("create a2a config directory");
        std::fs::write(
            &config_path,
            serde_json::json!({
                "agents": serde_json::Value::Object(agents),
                "server": {
                    "admission": "allow",
                    "peerKeys": serde_json::Value::Object(peer_keys),
                    "tls": {
                        "cert": cert_path.strip_prefix(workspace).expect("relative cert").to_string_lossy(),
                        "key": key_path.strip_prefix(workspace).expect("relative key").to_string_lossy(),
                    },
                    "advertisedHost": format!("localhost:{port}"),
                },
            })
            .to_string(),
        )
        .expect("write production inbound A2A config");
        let roster = parse_workspace_a2a_config(&config_path).expect("parse inbound sender roster");
        let config = parse_workspace_a2a_server_config(&config_path)
            .expect("parse inbound A2A server config")
            .expect("inbound A2A server config exists");
        let auth = resolve_server_auth(&config, &roster, |name| credentials.get(name).cloned())
            .expect("resolve configured credentials to pinned senders");
        let tls_config = config.tls.as_ref().expect("TLS config");
        let tls = rustain::adapters::a2a::tls::load_tls_material(
            &workspace.join(&tls_config.cert),
            &workspace.join(&tls_config.key),
        )
        .expect("load production inbound TLS material");
        let security = A2aServerSecurity {
            tls: Some(tls),
            auth,
            thread_reads: config.thread_reads.unwrap_or_default(),
        };
        let signer_dir = tempfile::tempdir().expect("inbound daemon identity directory");
        let signer = IdentityKeyStore::new(signer_dir.path())
            .load_or_generate()
            .expect("inbound daemon signing identity");
        let cancel = CancellationToken::new();
        let task = tokio::spawn(serve(
            listener,
            ServeConfig {
                registry: Arc::new(CapabilityRegistry::new(None)),
                signer,
                security,
                runtime: Some(
                    self.server.clone() as Arc<dyn rustain::domain::ports::InboundPeerRuntime>
                ),
                transparency: Arc::new(
                    TransparencySink::new(self.room_journal.clone())
                        .with_reader(self.journal.node.clone()),
                ),
                policy: config.admission,
                workspace: workspace.to_path_buf(),
                advertised_host: config.advertised_host.clone(),
                cards: Arc::new(SignedCardCache::new()),
            },
            cancel.child_token(),
        ));
        InboundHttpHost {
            clients,
            cancel,
            task,
            _signer_dir: signer_dir,
            _senders: senders,
        }
    }
    pub async fn stop(self) {
        self.shutdown.cancel();
        self.task.await.expect("daemon socket task");
    }
}

/// A distinct pinned API-key sender for the attached daemon's real inbound
/// A2A endpoint. The key is resolved only by the written server config.
pub struct InboundSenderIdentity {
    pub alias: String,
    pub key_env: String,
    pub secret: String,
    pub identity: PeerIdentity,
    _keys: tempfile::TempDir,
}
impl InboundSenderIdentity {
    pub fn new(alias: impl Into<String>) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let alias = alias.into();
        let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let key_env = format!(
            "RUSTAIN_ATTACHED_INBOUND_{}_KEY_{serial}",
            alias.to_ascii_uppercase()
        );
        let secret = format!("attached-inbound-secret-{serial}-{alias}");
        let keys = tempfile::tempdir().expect("inbound sender key directory");
        let signer: AgentSigner = IdentityKeyStore::new(keys.path())
            .load_or_generate()
            .expect("generate pinned inbound sender identity");
        Self {
            alias,
            key_env,
            secret,
            identity: signer.identity().clone(),
            _keys: keys,
        }
    }
}
/// A roster entry whose pin is exactly the key authenticated by the inbound
/// host's `peerKeys` binding. Use its alias in a daemon's composed peers/policy.
pub fn pinned_peer_for_inbound_sender(
    sender: &InboundSenderIdentity,
) -> rustain::domain::models::A2aPeerSpec {
    rustain::domain::models::A2aPeerSpec::new(
        sender.alias.clone(),
        rustain::domain::models::RedactedUrl::from("http://127.0.0.1:9/"),
        rustain::domain::models::A2aPeerSource::Workspace,
    )
    .with_pinned_key(Some(rustain::domain::models::PinnedKey::new(
        rustain::domain::models::PinnedKeyAlgorithm::EdDsa,
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&sender.identity.public_key),
        None,
    )))
}

pub struct InboundHttpHost {
    clients: HashMap<String, InboundHttpSender>,
    cancel: CancellationToken,
    task: JoinHandle<anyhow::Result<()>>,
    _signer_dir: tempfile::TempDir,
    _senders: Vec<InboundSenderIdentity>,
}
impl InboundHttpHost {
    pub fn sender(&self, alias: &str) -> InboundHttpSender {
        self.clients
            .get(alias)
            .unwrap_or_else(|| panic!("unknown attached inbound sender {alias:?}"))
            .clone()
    }
    pub async fn stop(self) {
        self.cancel.cancel();
        self.task
            .await
            .expect("attached inbound A2A task")
            .expect("A2A shutdown");
    }
}

#[derive(Clone)]
pub struct InboundHttpSender {
    endpoint: String,
    client: reqwest::Client,
    secret: String,
}
impl InboundHttpSender {
    fn new(secret: String, endpoint: String, cert_path: PathBuf) -> Self {
        let pem = std::fs::read(cert_path).expect("read daemon TLS anchor");
        let anchor = reqwest::Certificate::from_pem(&pem).expect("parse daemon TLS anchor");
        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .tls_built_in_root_certs(false)
            .add_root_certificate(anchor)
            .timeout(FIXTURE_TIMEOUT)
            .build()
            .expect("build anchored inbound sender client");
        Self {
            endpoint,
            client,
            secret,
        }
    }
    pub async fn message_send(&self, task: &str, text: &str) -> InboundSendResult {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "message/send",
            "params": { "message": {
                "kind": "message",
                "messageId": task,
                "role": "user",
                "parts": [{ "kind": "text", "text": text }],
                "metadata": {},
            }},
        });
        let response = tokio::time::timeout(
            FIXTURE_TIMEOUT,
            self.client
                .post(&self.endpoint)
                .header(API_KEY_HEADER, &self.secret)
                .json(&body)
                .send(),
        )
        .await
        .expect("inbound message/send deadline")
        .expect("inbound message/send HTTP response");
        let status = response.status();
        let body_text = tokio::time::timeout(FIXTURE_TIMEOUT, response.text())
            .await
            .expect("inbound response body deadline")
            .expect("read inbound response body");
        let json = serde_json::from_str::<serde_json::Value>(&body_text).unwrap_or_default();
        let task_state = json["result"]["status"]["state"]
            .as_str()
            .map(str::to_owned);
        let outcome = if !status.is_success() || !json["error"].is_null() {
            InboundAdmissionOutcome::Refused
        } else if task_state.as_deref() == Some("auth-required") {
            InboundAdmissionOutcome::PendingConsent
        } else if task_state.is_some() {
            InboundAdmissionOutcome::Admitted
        } else {
            InboundAdmissionOutcome::Refused
        };
        InboundSendResult {
            outcome,
            task_state,
            json,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InboundAdmissionOutcome {
    Admitted,
    PendingConsent,
    Refused,
}
#[derive(Clone, Debug)]
pub struct InboundSendResult {
    pub outcome: InboundAdmissionOutcome,
    pub task_state: Option<String>,
    pub json: serde_json::Value,
}

/// Real HTTP recipient; optional held journal gate pauses a received-item append
/// inside the production A2A admission path. `listener` enables same-address
/// down-at-boot fresh-egress tests. Policy can be `story_assignment`-specific;
/// admission can be Allow or Deny.
pub struct RecipientHost {
    pub journal: Journal,
    pub endpoint: String,
    pub registry: Arc<CapabilityRegistry>,
    cancel: CancellationToken,
    task: JoinHandle<anyhow::Result<()>>,
    _keys: tempfile::TempDir,
}
impl RecipientHost {
    pub async fn start(admission: A2aAdmissionPolicy) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind recipient");
        Self::start_on(listener, admission, None).await
    }
    pub async fn start_on(
        listener: TcpListener,
        admission: A2aAdmissionPolicy,
        wrapped: Option<Arc<dyn RoomJournal>>,
    ) -> Self {
        let journal = Journal::open().await;
        Self::start_on_with_journal(listener, admission, journal, wrapped).await
    }
    pub async fn start_with_gate(
        admission: A2aAdmissionPolicy,
        matches: fn(&RoomEvent) -> bool,
    ) -> (Self, Arc<GateRoomJournal>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind recipient");
        let journal = Journal::open().await;
        let gate = Arc::new(GateRoomJournal::new(journal.room.clone(), matches, true));
        let host =
            Self::start_on_with_journal(listener, admission, journal, Some(gate.clone())).await;
        (host, gate)
    }
    pub async fn start_on_with_journal(
        listener: TcpListener,
        admission: A2aAdmissionPolicy,
        journal: Journal,
        wrapped: Option<Arc<dyn RoomJournal>>,
    ) -> Self {
        let sink = wrapped.unwrap_or_else(|| journal.room.clone());
        let workspace = journal.workspace.path().to_path_buf();
        let keys = tempfile::tempdir().expect("recipient signing keys");
        let signer = IdentityKeyStore::new(keys.path())
            .load_or_generate()
            .expect("recipient identity");
        let (domain_tx, _domain_rx) = mpsc::unbounded_channel();
        let tree = NodeTree::with_event_tx(
            domain_tx.clone(),
            Arc::new(|| chrono::Utc::now().timestamp_millis()),
        )
        .with_journal(journal.node.clone());
        let storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
            sessions_dir(&workspace),
            workspace.clone(),
        ));
        let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let turns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let runtime_workspace = workspace.clone();
        let runtime_storage = storage.clone();
        let runtime_requests = requests.clone();
        let runtime_turns = turns.clone();
        let core = Arc::new(DaemonCore::new(
            workspace.clone(),
            Arc::new(ArcSwap::from_pointee(AppConfig::default())),
            Arc::new(NoOpMemory),
            storage,
            Arc::new(NoOpSecurity),
            Arc::new(NoOpPersona),
            Arc::new(rustain::adapters::rap::PeerTopicStore::new()),
            Box::new(move || {
                Ok(daemon_runtime(
                    Arc::new(ObservedProvider {
                        requests: runtime_requests.clone(),
                        turns: runtime_turns.clone(),
                        script: Arc::new(ToolScript::default()),
                    }),
                    runtime_storage.clone(),
                    &runtime_workspace,
                    None,
                    Arc::new(rustain::adapters::daemon::team_proposal::TeamProposalRegistry::new()),
                    Arc::new(ArcSwap::from_pointee(AppConfig::default())),
                    Arc::new(NoOpSecurity),
                ))
            }),
        ));
        let delivery = story_assignment_delivery_policy();
        let bus =
            rustain::adapters::daemon::server::peer_bus_slot_with_policy(&tree, delivery.clone());
        let attach = AttachServer::new_with_node_tree_bus_policy_and_journal(
            core,
            Arc::new(tokio::sync::Mutex::new(Conversation::default())),
            domain_tx,
            tree,
            bus,
            delivery,
            sink.clone(),
            journal.node.clone(),
            None,
        );
        let endpoint = format!(
            "http://{}/",
            listener.local_addr().expect("recipient address")
        );
        let cancel = CancellationToken::new();
        let registry = Arc::new(CapabilityRegistry::new(None));
        let task = tokio::spawn(serve(
            listener,
            ServeConfig {
                registry: registry.clone(),
                signer,
                security: A2aServerSecurity::default(),
                runtime: Some(attach as Arc<dyn rustain::domain::ports::InboundPeerRuntime>),
                transparency: Arc::new(
                    TransparencySink::new(sink).with_reader(journal.node.clone()),
                ),
                policy: admission,
                workspace,
                advertised_host: None,
                cards: Arc::new(SignedCardCache::new()),
            },
            cancel.child_token(),
        ));
        Self {
            journal,
            endpoint,
            registry,
            cancel,
            task,
            _keys: keys,
        }
    }
    pub fn peer(&self, alias: &str) -> rustain::domain::models::A2aPeerSpec {
        rustain::domain::models::A2aPeerSpec::new(
            alias,
            rustain::domain::models::RedactedUrl::from(self.endpoint.clone()),
            rustain::domain::models::A2aPeerSource::Workspace,
        )
    }
    pub async fn stop(self) {
        self.cancel.cancel();
        self.task
            .await
            .expect("recipient task")
            .expect("recipient shutdown");
    }
}
fn story_assignment_delivery_policy() -> Arc<dyn DeliveryPolicy> {
    use rustain::domain::models::{
        IndividualDefaults, IndividualPolicy, NotificationUrgency, TeamPolicy, TeamTypeOverride,
    };
    let individual = IndividualPolicy {
        defaults: IndividualDefaults {
            response_mode: Some(ResponseMode::NotifyAndAuto),
            notification: Some(NotificationUrgency::Immediate),
            ..Default::default()
        },
        ..Default::default()
    };
    let mut team = TeamPolicy::default();
    team.overrides.per_type.insert(
        "story_assignment".into(),
        toml::Value::try_from(TeamTypeOverride {
            response_mode: Some(ResponseMode::NotifyAndAuto),
            notification: None,
        })
        .expect("policy"),
    );
    let policy = rustain::domain::services::team_policy::resolve_effective_policy(
        &individual,
        Some(&team),
        &[],
    );
    Arc::new(EffectiveDeliveryPolicy::new(Arc::new(policy)))
}

/// Signed real attach client. The connection owns the only frame reader; every
/// received frame is retained in both the ordered stream and request/kind index.
pub struct AttachedClient {
    pub stream: UnixStream,
    pub granted: AttachMode,
    pub received: Vec<DaemonFrame>,
    consumed_frames: HashSet<usize>,
}
impl AttachedClient {
    pub async fn attach(path: &Path, tier: ConnectionTier, read_only_ok: bool) -> Self {
        let mut client = Self::attach_without_proposals(path, tier, read_only_ok).await;
        if tier == ConnectionTier::TrustedLocal && client.granted == AttachMode::ReadWrite {
            client.enable_team_proposals().await;
        }
        client
    }
    /// Authenticated line-client connection: ordinary turns, no decision surface.
    pub async fn attach_without_proposals(
        path: &Path,
        tier: ConnectionTier,
        read_only_ok: bool,
    ) -> Self {
        let mut stream = tokio::time::timeout(FIXTURE_TIMEOUT, UnixStream::connect(path))
            .await
            .expect("connect deadline")
            .expect("connect daemon");
        let nonce = match read_socket_frame(&mut stream).await {
            DaemonFrame::AttachChallenge { nonce } => nonce,
            other => panic!("challenge expected, got {other:?}"),
        };
        let keys = tempfile::tempdir().expect("attachment key store");
        let signer = IdentityKeyStore::new(keys.path())
            .load_or_generate()
            .expect("attachment identity");
        let proof = signer.attach_proof(&nonce, PROTOCOL_VERSION, tier.proof_tag(), read_only_ok);
        write_frame(
            &mut stream,
            &ClientFrame::Attach {
                protocol_version: PROTOCOL_VERSION,
                read_only_ok,
                tier,
                challenge_nonce: nonce,
                identity: signer.identity().clone(),
                proof,
            },
        )
        .await
        .expect("signed attach");
        let ack = read_socket_frame(&mut stream).await;
        let granted = match &ack {
            DaemonFrame::AttachAck { granted_mode, .. } => *granted_mode,
            other => panic!("attach ack expected, got {other:?}"),
        };
        Self {
            stream,
            granted,
            received: vec![ack],
            consumed_frames: HashSet::new(),
        }
    }
    pub async fn enable_team_proposals(&mut self) {
        self.send_frame(ClientFrame::EnableTeamProposals).await;
        // History shares the operator FIFO: return only after the grant and any
        // retained snapshot publication, including for direct runtime callers.
        self.send_frame(ClientFrame::HistoryRequest {
            before_index: Some(0),
            count: 1,
        })
        .await;
        self.wait_for(|frame| matches!(frame, DaemonFrame::History { .. }))
            .await;
    }
    /// EOF is the cleanup fence: server removes this signed writer before
    /// dropping its outbound half, so replacement tests never race `drop()`.
    pub async fn detach(mut self) {
        use tokio::io::AsyncWriteExt;
        self.stream
            .shutdown()
            .await
            .expect("half-close attached writer");
        tokio::time::timeout(FIXTURE_TIMEOUT, async {
            while read_frame::<_, DaemonFrame>(&mut self.stream)
                .await
                .expect("detach drain")
                .is_some()
            {}
        })
        .await
        .expect("daemon detach cleanup fence");
    }
    pub async fn send_frame(&mut self, frame: ClientFrame) {
        write_frame(&mut self.stream, &frame)
            .await
            .expect("attached request write");
    }
    pub async fn send_team(
        &mut self,
        request: impl Into<String>,
        recipients: Vec<String>,
        text: impl Into<String>,
        message_type: Option<rustain::domain::models::SemanticMessageType>,
        subject: Option<String>,
    ) {
        self.send_frame(ClientFrame::TeamSend {
            request: request.into(),
            recipients,
            text: text.into(),
            message_type,
            subject,
        })
        .await;
    }
    pub async fn team_board(&mut self, request: impl Into<String>, selector: TeamBoardRequest) {
        self.send_frame(ClientFrame::TeamBoard {
            request: request.into(),
            selector,
        })
        .await;
    }
    pub async fn team_status(&mut self, request: impl Into<String>) {
        self.send_frame(ClientFrame::TeamStatus {
            request: request.into(),
        })
        .await;
    }
    pub async fn revoke_sender_consent(
        &mut self,
        request: impl Into<String>,
        alias: impl Into<String>,
    ) {
        self.send_frame(ClientFrame::RevokeSenderConsent {
            request: request.into(),
            alias: alias.into(),
        })
        .await;
    }
    pub async fn collect_one(&mut self) -> DaemonFrame {
        let frame = read_socket_frame(&mut self.stream).await;
        self.received.push(frame.clone());
        frame
    }
    pub async fn wait_for<F>(&mut self, mut matches: F) -> DaemonFrame
    where
        F: FnMut(&DaemonFrame) -> bool,
    {
        tokio::time::timeout(FIXTURE_TIMEOUT, async {
            loop {
                let ready = self.received.iter().enumerate().find_map(|(index, frame)| {
                    (!self.consumed_frames.contains(&index) && matches(frame))
                        .then(|| (index, frame.clone()))
                });
                if let Some((index, frame)) = ready {
                    self.consumed_frames.insert(index);
                    return frame;
                }
                self.collect_one().await;
            }
        })
        .await
        .expect("correlated frame deadline")
    }
    pub fn frames_for_request(&self, request: &str) -> Vec<&DaemonFrame> {
        self.received
            .iter()
            .filter(|frame| match frame {
                DaemonFrame::TeamSendStarted { request: key, .. }
                | DaemonFrame::TeamSendSettled { request: key, .. }
                | DaemonFrame::TeamBoardChunk { request: key, .. }
                | DaemonFrame::TeamBoardDone { request: key, .. }
                | DaemonFrame::TeamStatusResult { request: key, .. }
                | DaemonFrame::SenderConsentRevoked { request: key, .. } => key == request,
                DaemonFrame::RecipientActionResult {
                    target: RecipientActionTarget::Request { request: key },
                    ..
                } => key == request,
                _ => false,
            })
            .collect()
    }
    /// Snapshot retaining every team frame, partitioned by request and kind.
    pub fn frames_by_request_and_kind(
        &self,
    ) -> BTreeMap<String, BTreeMap<&'static str, Vec<&DaemonFrame>>> {
        correlated_frames(&self.received)
    }
    pub async fn wait_send_started(&mut self, key: &str) -> DaemonFrame {
        self.wait_for(|f| {
            matches!(f, DaemonFrame::TeamSendStarted { request, .. } if request == key)
                || is_request_refusal(f, key)
        })
        .await
    }
    pub async fn wait_send_settled(&mut self, key: &str, index: usize) -> DaemonFrame {
        self.wait_for(|f| matches!(f, DaemonFrame::TeamSendSettled { request, index: i, .. } if request == key && *i == index) || is_request_refusal(f,key)).await
    }
    pub async fn wait_board_result(&mut self, key: &str) -> Result<String, Box<DaemonFrame>> {
        let mut next_sequence = 0u32;
        let mut view = String::new();
        loop {
            let frame = self
                .wait_for(|f| {
                    matches!(
                        f,
                        DaemonFrame::TeamBoardChunk { request, .. }
                            | DaemonFrame::TeamBoardDone { request, .. }
                            if request == key
                    ) || is_request_refusal(f, key)
                })
                .await;
            match frame {
                DaemonFrame::TeamBoardChunk {
                    sequence,
                    view: chunk,
                    ..
                } if sequence == next_sequence => {
                    view.push_str(&chunk);
                    let Some(next) = next_sequence.checked_add(1) else {
                        return Err(Box::new(DaemonFrame::Error(
                            rustain::adapters::daemon::protocol::ProtocolError::Malformed(
                                "team board sequence overflow".into(),
                            ),
                        )));
                    };
                    next_sequence = next;
                }
                DaemonFrame::TeamBoardDone { chunks, .. } if chunks == next_sequence => {
                    return Ok(view);
                }
                refusal @ DaemonFrame::RecipientActionResult { .. } => {
                    return Err(Box::new(refusal));
                }
                other => {
                    return Err(Box::new(DaemonFrame::Error(
                        rustain::adapters::daemon::protocol::ProtocolError::Malformed(format!(
                            "invalid board chunk sequence: {other:?}"
                        )),
                    )));
                }
            }
        }
    }
    pub async fn wait_status_result(&mut self, key: &str) -> DaemonFrame {
        self.wait_for(|f| {
            matches!(f,DaemonFrame::TeamStatusResult{request,..} if request==key)
                || is_request_refusal(f, key)
        })
        .await
    }
    pub async fn wait_revoked(&mut self, key: &str) -> DaemonFrame {
        self.wait_for(|f| {
            matches!(f,DaemonFrame::SenderConsentRevoked{request,..} if request==key)
                || is_request_refusal(f, key)
        })
        .await
    }
    /// Wait for the real daemon-owned sender-consent approval card.
    pub async fn wait_sender_consent_request(&mut self) -> (RequestId, String) {
        match self
            .wait_for(|frame| matches!(frame, DaemonFrame::ApprovalRequest { tool, .. } if tool == "a2a/sender-consent"))
            .await
        {
            DaemonFrame::ApprovalRequest {
                request_id,
                summary,
                ..
            } => (request_id, summary),
            other => panic!("sender-consent approval request expected, got {other:?}"),
        }
    }
    pub async fn answer_sender_consent_always_and_save(&mut self, request_id: RequestId) {
        self.send_frame(ClientFrame::ApprovalResponse {
            request_id,
            outcome: ApprovalOutcome::AlwaysAndSave {
                scope: ApprovalScope::Tool("a2a/sender-consent".to_owned()),
            },
        })
        .await;
    }
    pub async fn answer_sender_consent_reject(
        &mut self,
        request_id: RequestId,
        feedback: impl Into<String>,
    ) {
        let outcome = ApprovalOutcome::Reject {
            feedback: Some(feedback.into()),
        };
        self.send_frame(ClientFrame::ApprovalResponse {
            request_id,
            outcome,
        })
        .await;
    }
}
fn is_request_refusal(frame: &DaemonFrame, key: &str) -> bool {
    matches!(frame,DaemonFrame::RecipientActionResult{target:RecipientActionTarget::Request{request},outcome:RecipientActionOutcome::Refused{..},..} if request==key)
}
async fn read_socket_frame(stream: &mut UnixStream) -> DaemonFrame {
    tokio::time::timeout(FIXTURE_TIMEOUT, read_frame::<_, DaemonFrame>(stream))
        .await
        .expect("fixture socket frame deadline")
        .expect("decode daemon frame")
        .expect("socket frame present")
}
pub fn mint_request_key() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    format!(
        "attached-{}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

/// Shared board reader wrapper. Callers supply strictly increasing `Instant`s
/// for repeated reads on one runtime; the production 500ms refresh floor remains
/// intact and tests must not sleep or lower it.
pub async fn render_board(
    runtime: &rustain::adapters::a2a::driver::A2aDelegationRuntime,
    selector: &rustain::adapters::tui::handlers::team_command::TeamBoardSelector,
    now: Instant,
) -> String {
    use rustain::adapters::a2a::board::{self, ActionSelector};
    use rustain::adapters::tui::handlers::team_command::TeamBoardSelector;
    let wall = 1_800_000_000;
    match selector {
        TeamBoardSelector::Inventory => match board::collect_board(runtime, now).await {
            Ok(view) => board::render_board(&view),
            Err(refusal) => board::render_refresh_refusal(refusal),
        },
        TeamBoardSelector::Peer(peer) => {
            match board::collect_peer_board(runtime, peer, now).await {
                Ok(view) => board::render_board(&view),
                Err(refusal) => board::render_peer_board_refusal(&refusal),
            }
        }
        TeamBoardSelector::Last => {
            match board::collect_action_board(runtime, &ActionSelector::Last, now).await {
                Ok(view) => board::render_action_board(&view, wall),
                Err(refusal) => board::render_action_board_refusal(&refusal),
            }
        }
        TeamBoardSelector::Action(id) => {
            match board::collect_action_board(runtime, &ActionSelector::Id(id.clone()), now).await {
                Ok(view) => board::render_action_board(&view, wall),
                Err(refusal) => board::render_action_board_refusal(&refusal),
            }
        }
        TeamBoardSelector::Actions => match board::collect_action_list(runtime).await {
            Ok(actions) => board::render_action_list(&actions, wall),
            Err(refusal) => board::render_action_board_refusal(&refusal),
        },
    }
}

/// A frame-kind/request index preserving all siblings and arrival order.
pub fn correlated_frames(
    frames: &[DaemonFrame],
) -> BTreeMap<String, BTreeMap<&'static str, Vec<&DaemonFrame>>> {
    let mut result = BTreeMap::new();
    for frame in frames {
        let (key, kind) = match frame {
            DaemonFrame::TeamSendStarted { request, .. } => (request.as_str(), "started"),
            DaemonFrame::TeamSendSettled { request, .. } => (request.as_str(), "settled"),
            DaemonFrame::TeamBoardChunk { request, .. } => (request.as_str(), "board-chunk"),
            DaemonFrame::TeamBoardDone { request, .. } => (request.as_str(), "board"),
            DaemonFrame::TeamStatusResult { request, .. } => (request.as_str(), "status"),
            DaemonFrame::SenderConsentRevoked { request, .. } => (request.as_str(), "revoked"),
            DaemonFrame::RecipientActionResult {
                action,
                target: RecipientActionTarget::Request { request },
                ..
            } => {
                let kind = match action {
                    rustain::adapters::daemon::protocol::RecipientAction::TeamSend => {
                        "send-refusal"
                    }
                    rustain::adapters::daemon::protocol::RecipientAction::TeamBoard => {
                        "board-refusal"
                    }
                    rustain::adapters::daemon::protocol::RecipientAction::TeamStatus => {
                        "status-refusal"
                    }
                    rustain::adapters::daemon::protocol::RecipientAction::RevokeSenderConsent => {
                        "untrust-refusal"
                    }
                    _ => "other-refusal",
                };
                (request.as_str(), kind)
            }
            _ => continue,
        };
        result
            .entry(key.to_owned())
            .or_insert_with(BTreeMap::new)
            .entry(kind)
            .or_insert_with(Vec::new)
            .push(frame);
    }
    result
}

/// Normalized parity evidence and independently hosted equivalent recipients
/// returned by [`standalone_attached_parity`].
pub struct StandaloneAttachedParity {
    pub standalone_facts: Vec<String>,
    pub attached_facts: Vec<String>,
    pub standalone_rows: String,
    pub attached_rows: String,
    pub standalone_hosts: Vec<RecipientHost>,
    pub attached_hosts: Vec<RecipientHost>,
}

/// Drive one labelled typed send through the existing standalone Sender and a
/// separate attached daemon journal. Each rail gets distinct real recipient
/// hosts with equivalent admission behavior, so generated arrival ordinals
/// are comparable. Action ids are normalized only for journal/board comparison.
pub async fn standalone_attached_parity(
    aliases: Vec<String>,
    text: &str,
    subject: &str,
) -> StandaloneAttachedParity {
    let standalone_hosts = futures::future::join_all(
        (0..aliases.len()).map(|_| RecipientHost::start(A2aAdmissionPolicy::Allow)),
    )
    .await;
    let standalone_peers = standalone_hosts
        .iter()
        .zip(&aliases)
        .map(|(host, alias)| host.peer(alias))
        .collect::<Vec<_>>();
    let attached_hosts = futures::future::join_all(
        (0..aliases.len()).map(|_| RecipientHost::start(A2aAdmissionPolicy::Allow)),
    )
    .await;
    let attached_peers = attached_hosts
        .iter()
        .zip(&aliases)
        .map(|(host, alias)| host.peer(alias))
        .collect::<Vec<_>>();
    let clock = Arc::new(rustain::domain::clock::MockClock::at_wall_ms(
        1_800_000_000_000,
    ));
    let mut standalone =
        crate::harness::Sender::start_with_clock(standalone_peers.clone(), clock.clone()).await;
    let mut state = rustain::adapters::tui::state::TuiState::new(100, 40);
    let command = format!(
        "/team send --type story_assignment --subject {subject} {} {text}",
        aliases.join(",")
    );
    standalone.command(&mut state, &command).await;
    standalone.settle(&mut state, aliases.len()).await;
    let standalone_events = crate::harness::read_room_events(&standalone.journal).await;
    let standalone_id = send_action_id(&standalone_events);
    let standalone_facts = normalized_send_facts(&standalone_events, &standalone_id);
    let selector = rustain::adapters::tui::handlers::team_command::TeamBoardSelector::Action(
        standalone_id.clone(),
    );
    let standalone_rows = render_board(standalone.runtime(), &selector, Instant::now())
        .await
        .replace(&standalone_id, "<action>");

    let attached_journal = Journal::open_with_clock(clock.clone()).await;
    let attached_sink = attached_journal.room.clone();
    let daemon = AttachedDaemon::compose_with_clock(
        attached_journal.clone(),
        attached_peers.clone(),
        attached_sink,
        clock,
    )
    .await;
    let mut client =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    let request = mint_request_key();
    client
        .send_team(
            request.clone(),
            aliases,
            text,
            Some(rustain::domain::models::SemanticMessageType::StoryAssignment),
            Some(subject.to_owned()),
        )
        .await;
    let started = client.wait_send_started(&request).await;
    let attached_id = match started {
        DaemonFrame::TeamSendStarted { action, .. } => action.as_str().to_owned(),
        other => panic!("attached parity send refused: {other:?}"),
    };
    for index in 0..standalone_events
        .iter()
        .find(|(_, event)| matches!(event, RoomEvent::TeamSendActionRecorded { .. }))
        .and_then(|(_, event)| match event {
            RoomEvent::TeamSendActionRecorded { recipients, .. } => Some(recipients.len()),
            _ => None,
        })
        .unwrap_or(0)
    {
        let frame = client.wait_send_settled(&request, index).await;
        assert!(
            matches!(frame, DaemonFrame::TeamSendSettled { .. }),
            "attached parity settlement: {frame:?}"
        );
    }
    let attached_events = crate::harness::read_room_events(&daemon.journal).await;
    let attached_facts = normalized_send_facts(&attached_events, &attached_id);
    let selector = rustain::adapters::tui::handlers::team_command::TeamBoardSelector::Action(
        attached_id.clone(),
    );
    let attached_rows = render_board(
        daemon.egress.runtime(),
        &selector,
        Instant::now() + Duration::from_secs(1),
    )
    .await
    .replace(&attached_id, "<action>");
    daemon.stop().await;
    StandaloneAttachedParity {
        standalone_facts,
        attached_facts,
        standalone_rows,
        attached_rows,
        standalone_hosts,
        attached_hosts,
    }
}

fn send_action_id(events: &[(u64, RoomEvent)]) -> String {
    events
        .iter()
        .find_map(|(_, event)| match event {
            RoomEvent::TeamSendActionRecorded { action, .. } => Some(action.as_str().to_owned()),
            _ => None,
        })
        .expect("typed send action recorded")
}

fn normalized_send_facts(events: &[(u64, RoomEvent)], action_id: &str) -> Vec<String> {
    events
        .iter()
        .filter_map(|(_, event)| {
            let belongs = match event {
                RoomEvent::TeamSendActionRecorded { action, .. }
                | RoomEvent::TeamSendRecipientSettled { action, .. } => {
                    action.as_str() == action_id
                }
                RoomEvent::RemoteEnvelopeDispatched {
                    action: Some(reference),
                    ..
                } => reference.action.as_str() == action_id,
                _ => false,
            };
            belongs.then(|| format!("{event:?}").replace(action_id, "<action>"))
        })
        .collect()
}

#[tokio::test]
async fn attached_harness_smoke_typed_send_reaches_two_real_http_hosts() {
    let a = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let b = RecipientHost::start(A2aAdmissionPolicy::Allow).await;
    let daemon = AttachedDaemon::start(vec![a.peer("a"), b.peer("b")]).await;
    let mut client =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    assert_eq!(client.granted, AttachMode::ReadWrite);
    client
        .send_team(
            "smoke-send",
            vec!["a".into(), "b".into()],
            "typed attached fixture",
            Some(rustain::domain::models::SemanticMessageType::StoryAssignment),
            Some("smoke".into()),
        )
        .await;
    assert!(matches!(
        client.wait_send_started("smoke-send").await,
        DaemonFrame::TeamSendStarted { .. }
    ));
    assert!(matches!(
        client.wait_send_settled("smoke-send", 0).await,
        DaemonFrame::TeamSendSettled { .. }
    ));
    assert!(matches!(
        client.wait_send_settled("smoke-send", 1).await,
        DaemonFrame::TeamSendSettled { .. }
    ));
    daemon.stop().await;
    a.stop().await;
    b.stop().await;
}

#[tokio::test]
async fn attached_harness_smoke_read_only_board_is_served() {
    let daemon = AttachedDaemon::start(Vec::new()).await;
    let mut client = AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, true).await;
    assert_eq!(client.granted, AttachMode::ReadOnly);
    client
        .team_board("smoke-board", TeamBoardRequest::Inventory)
        .await;
    assert!(client.wait_board_result("smoke-board").await.is_ok());
    daemon.stop().await;
}

#[tokio::test]
async fn attached_harness_smoke_user_message_is_provider_positive_control() {
    let daemon = AttachedDaemon::start(Vec::new()).await;
    let mut client =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    client
        .send_frame(ClientFrame::UserMessage {
            text: "positive provider control".into(),
            images: Vec::new(),
        })
        .await;
    tokio::time::timeout(FIXTURE_TIMEOUT, async {
        while daemon.requests.lock().len() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("provider request control deadline");
    assert_eq!(daemon.requests.lock().len(), 1);
    assert_eq!(
        daemon
            .model_turns
            .load(std::sync::atomic::Ordering::Acquire),
        1
    );
    daemon.stop().await;
}

#[tokio::test]
async fn attached_harness_smoke_boot_down_peer_keeps_card_not_cached_refusal() {
    // Bound but not listening: warm-up fails without releasing this test's port.
    let reserved = tokio::net::TcpSocket::new_v4().expect("create reserved socket");
    reserved
        .bind(([127, 0, 0, 1], 0).into())
        .expect("reserve recipient address");
    let addr = reserved.local_addr().unwrap();
    let peer = rustain::domain::models::A2aPeerSpec::new(
        "down",
        rustain::domain::models::RedactedUrl::from(format!("http://{addr}/")),
        rustain::domain::models::A2aPeerSource::Workspace,
    );
    let daemon = AttachedDaemon::start(vec![peer]).await;
    let listener = reserved
        .listen(1024)
        .expect("bring peer up on same address");
    let host = RecipientHost::start_on(listener, A2aAdmissionPolicy::Allow, None).await;
    let mut client =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;
    client
        .send_team(
            "boot-down",
            vec!["down".into()],
            "must retain boot slot",
            None,
            None,
        )
        .await;
    let refused = client.wait_send_settled("boot-down", 0).await;
    let refusal_text = format!("{refused:?}");
    assert!(
        refusal_text.contains("CardNotCached")
            || refusal_text.to_ascii_lowercase().contains("not cached"),
        "expected CardNotCached-style refusal, got {refused:?}"
    );
    daemon.stop().await;
    host.stop().await;
}

#[tokio::test]
async fn attached_harness_smoke_real_inbound_sender_is_prompted_without_consent() {
    let journal = Journal::open().await;
    let gate = Arc::new(AppendGateJournal::new(
        journal.room.clone(),
        is_consent_event,
        false,
    ));
    let identity = InboundSenderIdentity::new("prompted");
    let peer_id = identity.identity.peer_id.clone();
    let peers = vec![pinned_peer_for_inbound_sender(&identity)];
    let daemon = AttachedDaemon::start_with_journal(journal, peers, Some(gate.clone())).await;
    let host = daemon.serve_http(vec![identity]).await;
    let sender = host.sender("prompted");
    let mut attached =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;

    let send = sender
        .message_send("prompted-task", "request the real sender capability")
        .await;
    assert_eq!(
        send.outcome,
        InboundAdmissionOutcome::PendingConsent,
        "{send:?}"
    );
    let prompts = daemon.sender_consent_prompts_for(&peer_id).await;
    assert_eq!(
        prompts.len(),
        1,
        "the real sender receives one consent prompt"
    );
    let (request_id, _) = attached.wait_sender_consent_request().await;
    assert_eq!(prompts[0], request_id);
    attached
        .answer_sender_consent_reject(request_id, "smoke control")
        .await;

    host.stop().await;
    daemon.stop().await;
}

#[tokio::test]
async fn attached_harness_smoke_saved_sender_consent_applies_only_to_pinned_sender() {
    let journal = Journal::open().await;
    let gate = Arc::new(AppendGateJournal::new(
        journal.room.clone(),
        is_consent_event,
        false,
    ));
    let trusted_identity = InboundSenderIdentity::new("saved");
    let other_identity = InboundSenderIdentity::new("other");
    assert_ne!(
        trusted_identity.identity.peer_id, other_identity.identity.peer_id,
        "fixture senders need distinct pinned identities"
    );
    let trusted_peer_id = trusted_identity.identity.peer_id.clone();
    let other_peer_id = other_identity.identity.peer_id.clone();
    let peers = vec![
        pinned_peer_for_inbound_sender(&trusted_identity),
        pinned_peer_for_inbound_sender(&other_identity),
    ];
    let daemon = AttachedDaemon::start_with_journal(journal, peers, Some(gate.clone())).await;
    let host = daemon
        .serve_http(vec![trusted_identity, other_identity])
        .await;
    let trusted = host.sender("saved");
    let other = host.sender("other");
    let mut attached =
        AttachedClient::attach(&daemon.path, ConnectionTier::TrustedLocal, false).await;

    let first = trusted
        .message_send("saved-task-1", "request saved capability")
        .await;
    assert_eq!(
        first.outcome,
        InboundAdmissionOutcome::PendingConsent,
        "{first:?}"
    );
    let first_prompt_ids = daemon.sender_consent_prompts_for(&trusted_peer_id).await;
    assert_eq!(
        first_prompt_ids.len(),
        1,
        "the saved sender is prompted once"
    );
    let (request_id, _) = attached.wait_sender_consent_request().await;
    assert_eq!(first_prompt_ids[0], request_id);
    attached
        .answer_sender_consent_always_and_save(request_id)
        .await;
    gate.wait_durable_for(1).await;
    assert!(
        gate.durable_seen().iter().any(|event| matches!(
            event,
            RoomEvent::ConsentGranted {
                sender: Some(sender),
                principal: Some(PrincipalKind::Pinned),
                ..
            } if sender == &trusted_peer_id
        )),
        "the real durable consent grant must name the authenticated pinned sender: {:?}",
        gate.durable_seen()
    );
    daemon
        .wait_for_pinned_sender_consent(&trusted_peer_id)
        .await;
    assert_eq!(
        daemon
            .consent_projection
            .consent_for(&PolicyPrincipal::bound_a2a(trusted_peer_id.clone())),
        ConsentState::Trusted,
        "the same BoundA2a principal/slot used by message/send must see the grant"
    );
    daemon.wait_for_received_task("saved-task-1").await;

    let _second = trusted
        .message_send("saved-task-2", "saved task marker second")
        .await;
    assert!(
        daemon
            .sender_consent_prompts_for(&trusted_peer_id)
            .await
            .is_empty(),
        "a saved pinned sender must not produce a second sender-consent card"
    );
    daemon.wait_for_received_task("saved-task-2").await;
    let _third = other
        .message_send("other-task", "different pinned sender marker")
        .await;
    let third_prompt_ids = daemon.sender_consent_prompts_for(&other_peer_id).await;
    assert_eq!(
        third_prompt_ids.len(),
        1,
        "a different pinned sender must still raise a sender-consent card"
    );
    let (request_id, _) = attached.wait_sender_consent_request().await;
    assert_eq!(third_prompt_ids[0], request_id);
    attached
        .answer_sender_consent_reject(request_id, "distinct identity control")
        .await;

    host.stop().await;
    daemon.stop().await;
}
