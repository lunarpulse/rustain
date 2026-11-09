use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use rustain::adapters::approval_persistence_toml::ApprovalPersistenceToml;
use rustain::adapters::composite_toolset_adapter::CompositeToolsetAdapter;
use rustain::adapters::noop::{NoOpPersona, NoOpStorage, NoOpUsageLedger};
use rustain::adapters::security_adapter::SecurityAdapter;
use rustain::adapters::toolset_adapter::ToolSetAdapter;
use rustain::adapters::tui::state::TuiState;
use rustain::domain::events::AppEvent;
use rustain::domain::models::{
    ActiveAgent, AgentToolRestriction, AppConfig, AutoApprovePolicy, Conversation, PermissionMode,
    StreamChunk,
};
use rustain::domain::ports::{SecurityPort, StreamingProvider, ToolSetPort};
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::domain::services::tool_scheduler::ToolScheduler;
use rustain::infrastructure::runtime::turn_driver::{
    LocalTurnDriver, TurnViewState, UserSubmission,
};
use tokio_util::sync::CancellationToken;

use super::inherited::Sender;
use super::tool_provider::tool_provider;

const SETTLE: Duration = Duration::from_secs(45);

pub(crate) struct ProposalFixture {
    pub(crate) sender: Sender,
    pub(crate) state: TuiState,
    pub(crate) clock: Arc<rustain::domain::clock::MockClock>,
    pub(crate) copies: Arc<parking_lot::Mutex<Vec<String>>>,
    pub(crate) conversation: Conversation,
    pub(crate) tabs: rustain::domain::models::tab::TabManager,
    pub(crate) tools: Arc<CompositeToolsetAdapter>,
    pub(crate) proposal_sink: Option<
        Arc<rustain::infrastructure::runtime::team_proposal_bridge::StandaloneTeamProposalSink>,
    >,
    skill_activator: Arc<rustain::adapters::skill_activation::SkillActivator>,
    restriction: Option<AgentToolRestriction>,
    pub(crate) security: Arc<SecurityAdapter>,
    pub(crate) scheduler: Arc<ToolScheduler>,
    pub(crate) approval: Arc<ApprovalRuntime>,
    pub(crate) approval_persistence: Arc<ApprovalPersistenceToml>,
    transitions:
        tokio::sync::broadcast::Receiver<rustain::domain::models::tool_call::ToolCallTransition>,
    pub(crate) last_transitions: Vec<rustain::domain::models::tool_call::ToolCallTransition>,
    pub(crate) last_tool_results: Vec<String>,
    pub(crate) last_tool_errors: Vec<bool>,
    pub(crate) last_turn_completed: bool,
    pub(crate) app_config: Arc<ArcSwap<AppConfig>>,
    pub(crate) context: Arc<ArcSwap<Arc<dyn rustain::domain::ports::ContextPort>>>,
}

impl ProposalFixture {
    pub(crate) async fn start(peers: Vec<rustain::domain::models::A2aPeerSpec>) -> Self {
        Self::start_with_surfaces(peers, true, true, AutoApprovePolicy::Ask).await
    }

    pub(crate) async fn start_with_policy(
        peers: Vec<rustain::domain::models::A2aPeerSpec>,
        policy: AutoApprovePolicy,
    ) -> Self {
        Self::start_with_surfaces(peers, true, true, policy).await
    }

    pub(crate) async fn start_without_surface(
        peers: Vec<rustain::domain::models::A2aPeerSpec>,
    ) -> Self {
        Self::start_with_surfaces(peers, true, false, AutoApprovePolicy::Ask).await
    }

    pub(crate) async fn start_without_egress(
        peers: Vec<rustain::domain::models::A2aPeerSpec>,
    ) -> Self {
        Self::start_with_surfaces(peers, false, false, AutoApprovePolicy::Ask).await
    }

    async fn start_with_surfaces(
        peers: Vec<rustain::domain::models::A2aPeerSpec>,
        install_egress: bool,
        surface: bool,
        policy: AutoApprovePolicy,
    ) -> Self {
        let mut sender = Sender::start(peers).await;
        let tabs = rustain::domain::models::tab::TabManager::new(CancellationToken::new());
        let conversation = tabs.active_tab().conversation.clone();
        let security = Arc::new(SecurityAdapter::new(
            sender.journal.workspace.path().to_path_buf(),
        ));
        let security_port = security.clone() as Arc<dyn SecurityPort>;
        let skill = Arc::new(rustain::adapters::skill_activation::SkillActivator::new());
        let builtin: Arc<dyn ToolSetPort> = Arc::new(ToolSetAdapter::new(
            sender.journal.workspace.path().to_path_buf(),
            Arc::new(NoOpStorage),
            Arc::new(ArcSwap::from_pointee(
                Arc::new(rustain::adapters::sandbox::NoOpSandbox)
                    as Arc<dyn rustain::domain::ports::SandboxManager>,
            )),
            Arc::new(tokio::sync::RwLock::new(
                rustain::domain::models::SandboxPolicy::Permissive,
            )),
        ));
        let tools = Arc::new(CompositeToolsetAdapter::new(
            builtin,
            vec![],
            vec![],
            true,
            None,
            Some(skill.clone()),
            None,
        ));
        let proposal_sink = if surface {
            let sink = Arc::new(rustain::infrastructure::runtime::team_proposal_bridge::StandaloneTeamProposalSink::new(sender.app_state.event_bus.clone()));
            sink.attach_conversation(&conversation.id).await;
            Some(sink)
        } else {
            None
        };
        sender.app_state.team_proposals = proposal_sink.clone();
        sender.app_state.transparency = Some(Arc::new(
            rustain::infrastructure::transparency::TransparencyService::new(
                sender.journal.node.clone(),
                sender.journal.workspace.path().to_path_buf(),
            ),
        ));
        if install_egress {
            sender.egress.install(
                &tools,
                proposal_sink
                    .as_ref()
                    .map(|sink| sink.clone() as Arc<dyn rustain::domain::ports::TeamProposalSink>),
            );
        }
        let tools_port = tools.clone() as Arc<dyn ToolSetPort>;
        let approval_persistence = Arc::new(ApprovalPersistenceToml::new(
            sender
                .journal
                .workspace
                .path()
                .join(".rustain-test-home/config.toml"),
            sender
                .journal
                .workspace
                .path()
                .join(".rustain/permissions.toml"),
        ));
        let approval =
            ApprovalRuntime::new_with_subagent_policy(32, approval_persistence.clone(), policy);
        approval.load_session().await;
        sender.app_state.approval_runtime = approval.clone();
        let mut config = AppConfig::default();
        config.subagents.auto_approve = policy;
        let app_config = sender.app_state.app_config.clone();
        app_config.store(Arc::new(config));
        let scheduler = ToolScheduler::new(security_port, tools_port, approval.clone(), 64);
        let transitions = scheduler.subscribe();
        let mut state = TuiState::new(120, 24);
        let clock = Arc::new(rustain::domain::clock::MockClock::new(Instant::now()));
        let copies = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let copy_sink = copies.clone();
        state.clipboard_copier = Some(Arc::new(move |text: &str| {
            copy_sink.lock().push(text.to_owned());
            rustain::adapters::tui::clipboard::ClipboardResult::Osc52Success
        }));
        state.notifications = rustain::adapters::tui::notifications::NotificationStore::from_config(
            &Default::default(),
            clock.clone(),
        );
        rustain::adapters::tui::decision_queue::prune_closed(
            &mut state,
            &rustain::adapters::tui::notification_log::LogFacts {
                active_conversation_id: Some(conversation.id.clone()),
                ..Default::default()
            },
        );
        Self {
            sender,
            state,
            clock,
            copies,
            conversation,
            tabs,
            proposal_sink,
            skill_activator: skill,
            tools,
            restriction: None,
            security,
            scheduler,
            approval,
            approval_persistence,
            transitions,
            last_transitions: Vec::new(),
            last_tool_results: Vec::new(),
            last_tool_errors: Vec::new(),
            last_turn_completed: false,
            app_config,
            context: Arc::new(ArcSwap::from_pointee(
                Arc::new(rustain::adapters::noop::NoOpContext)
                    as Arc<dyn rustain::domain::ports::ContextPort>,
            )),
        }
    }

    pub(crate) fn proposal(
        &self,
        conversation_id: &str,
    ) -> Option<&Arc<rustain::domain::models::TeamProposal>> {
        use rustain::adapters::tui::decision_queue::DecisionSubject;
        self.state
            .decisions
            .items()
            .find_map(|item| match &item.subject {
                DecisionSubject::TeamProposal(payload)
                    if !item.is_stale()
                        && item.conversation_id.as_deref() == Some(conversation_id) =>
                {
                    Some(&payload.proposal)
                }
                _ => None,
            })
    }

    pub(crate) fn proposal_count(&self) -> usize {
        self.state
            .decisions
            .count(rustain::adapters::tui::decision_queue::DecisionKind::TeamProposal)
    }

    pub(crate) fn paint(&mut self, width: u16, height: u16) -> Vec<String> {
        self.paint_streaming(
            width,
            height,
            &rustain::domain::models::StreamingState::default(),
        )
    }

    pub(crate) fn paint_streaming(
        &mut self,
        width: u16,
        height: u16,
        streaming: &rustain::domain::models::StreamingState,
    ) -> Vec<String> {
        let rows = self.paint_streaming_unready(width, height, streaming);
        self.clock
            .advance(rustain::adapters::tui::decision_queue::INERT_AFTER_PAINT);
        rows
    }

    pub(crate) fn paint_unready(&mut self, width: u16, height: u16) -> Vec<String> {
        self.paint_streaming_unready(
            width,
            height,
            &rustain::domain::models::StreamingState::default(),
        )
    }

    fn paint_streaming_unready(
        &mut self,
        width: u16,
        height: u16,
        streaming: &rustain::domain::models::StreamingState,
    ) -> Vec<String> {
        self.state.terminal_width = width;
        self.state.terminal_height = height;
        let area = ratatui::layout::Rect::new(0, 0, width, height);
        let layout = rustain::adapters::tui::layout::compute_layout(
            area,
            &self.state.theme,
            &self.state.input_buffer,
            self.tabs.tab_count(),
            self.state.sidebar_visible,
            self.state.density_mode,
        )
        .expect("fixture viewport meets the TUI minimum");
        let facts = rustain::adapters::tui::notification_log::LogFacts {
            active_conversation_id: Some(self.conversation.id.clone()),
            decisions_supported: true,
            ..Default::default()
        };
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        let mut painted = false;
        terminal
            .draw(|frame| {
                let render = self
                    .state
                    .tab_render_states
                    .entry(self.state.active_tab_id)
                    .or_default();
                rustain::adapters::tui::widgets::chat_pane::render_with_search(
                    frame,
                    layout.chat_pane,
                    &self.conversation,
                    None,
                    streaming,
                    &rustain::domain::models::ViewState::default(),
                    self.clock.as_ref(),
                    0,
                    true,
                    &self.state.theme,
                    render,
                    &self.state.feedback_blocks,
                    None,
                    None,
                    &[],
                    &[],
                    None,
                    None,
                    None,
                    Default::default(),
                );
                let shell = rustain::adapters::tui::presentation::Shell::new(frame.area(), &layout);
                painted = rustain::adapters::tui::presentation::compose(
                    frame,
                    &self.state,
                    &shell,
                    rustain::adapters::tui::keymap::Host::Local,
                    &facts,
                    &mut rustain::adapters::tui::presentation::occlusion::OpaqueRects::new(
                        shell.chat_pane,
                    ),
                    |_| {},
                );
            })
            .unwrap();
        if painted {
            rustain::adapters::tui::decision_queue::note_painted(&mut self.state);
        }
        let buffer = terminal.backend().buffer();
        buffer
            .content
            .chunks(width as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect()
    }

    pub(crate) async fn key(
        &mut self,
        event: rustain::domain::events::DomainInputEvent,
    ) -> rustain::adapters::tui::app::InputAction {
        let facts = rustain::adapters::tui::notification_log::LogFacts {
            active_conversation_id: Some(self.conversation.id.clone()),
            decisions_supported: true,
            ..Default::default()
        };
        let action = match rustain::adapters::tui::presentation::route(
            &mut self.state,
            &event,
            rustain::adapters::tui::keymap::Host::Local,
            &facts,
        ) {
            Some(rustain::adapters::tui::presentation::RouteEffect::Action(action)) => action,
            Some(_) => rustain::adapters::tui::app::InputAction::Consumed,
            None => rustain::adapters::tui::app::handle_input(&mut self.state, &event),
        };
        self.dispatch_proposal_key(&action).await;
        action
    }

    pub(crate) async fn raw_key(
        &mut self,
        event: crossterm::event::Event,
    ) -> Option<rustain::adapters::tui::app::InputAction> {
        if rustain::adapters::tui::app::recipient_surface_ignores_raw_key(&self.state, &event) {
            return None;
        }
        let domain =
            rustain::adapters::tui::app::convert_crossterm_event(&event, &Default::default())?;
        Some(self.key(domain).await)
    }

    async fn dispatch_proposal_key(&mut self, action: &rustain::adapters::tui::app::InputAction) {
        use rustain::adapters::tui::app::InputAction;
        use rustain::adapters::tui::decision_queue::{DecisionEffect, DecisionSubject};
        use rustain::domain::models::TeamProposalDecision;
        let decision = match action {
            InputAction::Decision(DecisionEffect::TeamProposalSend) => TeamProposalDecision::Send,
            InputAction::Decision(DecisionEffect::TeamProposalDismiss) => {
                TeamProposalDecision::Dismiss
            }
            _ => return,
        };
        let Some(DecisionSubject::TeamProposal(payload)) = self.state.decisions.take_resolved()
        else {
            panic!("proposal decision must carry its resolved snapshot");
        };
        rustain::infrastructure::runtime::team_proposal_bridge::proposal_action(
            &mut self.state,
            &self.sender.app_state,
            &self.conversation.id,
            payload.proposal,
            decision,
        )
        .await;
    }

    pub(crate) async fn typed(&mut self, line: &str) {
        self.state.input_buffer = line.to_owned();
        self.state.cursor_position = line.len();
        let rustain::adapters::tui::app::InputAction::ExecuteCommand { name, args } =
            rustain::adapters::tui::app::submit_message_for_test(&mut self.state)
        else {
            panic!("typed team command must reach command dispatcher")
        };
        assert_eq!(name, "team");
        let board = matches!(
            rustain::adapters::tui::handlers::team_command::parse_team_command(args.as_deref()),
            Ok(rustain::adapters::tui::handlers::team_command::TeamCommandArgs::Board { .. })
        );
        rustain::infrastructure::runtime::transparency_bridge::team_command(
            &mut self.state,
            &self.conversation.id,
            args.as_deref(),
            &self.sender.app_state,
        )
        .await;
        if board {
            tokio::time::timeout(SETTLE, async {
                while let Some(event) = self.sender.domain_rx.recv().await {
                    if let AppEvent::TeamBoardReady {
                        conversation_id,
                        message,
                    } = event
                    {
                        assert_eq!(conversation_id, self.conversation.id);
                        rustain::infrastructure::runtime::transparency_bridge::team_board_ready(
                            &self.conversation.id,
                            &mut self.state,
                            &mut self.tabs,
                            &conversation_id,
                            message,
                        );
                        return;
                    }
                }
                panic!("board event channel closed");
            })
            .await
            .expect("actual board projection completes");
        }
    }

    pub(crate) async fn settle(&mut self, expected: usize) {
        for _ in 0..expected {
            let event = tokio::time::timeout(SETTLE, self.sender.domain_rx.recv())
                .await
                .expect("send settlement timeout")
                .expect("send settlement channel");
            let AppEvent::TeamSendSettled {
                conversation_id,
                block_id,
                index,
                outcome,
            } = event
            else {
                panic!("expected send settlement, got {event:?}")
            };
            rustain::infrastructure::runtime::transparency_bridge::team_send_settled(
                &self.conversation.id,
                &mut self.state,
                &mut self.tabs,
                &conversation_id,
                &block_id,
                index,
                outcome,
            );
        }
    }

    pub(crate) fn set_mode(&mut self, mode: PermissionMode) {
        self.security.set_mode(mode);
    }
    pub(crate) fn set_restriction(&mut self, restriction: Option<AgentToolRestriction>) {
        self.restriction = restriction;
    }
    pub(crate) fn available_tools(&self) -> Vec<String> {
        self.tools
            .available_tools()
            .into_iter()
            .map(|tool| tool.name)
            .collect()
    }
    pub(crate) fn describe(&self) -> Vec<rustain::domain::models::tool_descriptor::ToolDescriptor> {
        self.tools.describe()
    }

    pub(crate) async fn activate_policy_skill(&self, allowed_tools: &str) {
        let workspace = self.sender.journal.workspace.path();
        let directory = workspace.join(".rustain/skills/proposal-policy");
        std::fs::create_dir_all(&directory).expect("skill directory");
        std::fs::write(directory.join("SKILL.md"), format!("---\nname: proposal-policy\ndescription: Proposal restriction fixture\nallowed-tools: {allowed_tools}\n---\nApply the declared tool policy.\n")).expect("skill file");
        let registry =
            rustain::adapters::skill_registry::SkillRegistry::discover(workspace, None, &[]);
        let definition = registry
            .find("proposal-policy")
            .expect("discovered skill")
            .clone();
        self.skill_activator.set_registry(registry).await;
        self.skill_activator
            .activate(&definition, String::new(), &self.conversation.id, 0)
            .await
            .expect("activate policy skill");
    }
    pub(crate) fn local_turn_driver_with_tools(
        &self,
        provider: Arc<dyn StreamingProvider>,
        event_tx: tokio::sync::mpsc::UnboundedSender<AppEvent>,
        tools_port: Arc<dyn ToolSetPort>,
    ) -> LocalTurnDriver {
        let security_port = self.security.clone() as Arc<dyn SecurityPort>;
        let workspace = self.sender.journal.workspace.path().to_path_buf();
        LocalTurnDriver::new(
            provider,
            self.app_config.clone(),
            event_tx,
            security_port,
            tools_port,
            self.scheduler.clone(),
            self.skill_activator.clone(),
            Arc::new(NoOpPersona),
            self.context.clone(),
            Arc::new(ArcSwap::from_pointee(None)),
            workspace.clone(),
            Arc::new(
                rustain::adapters::filesystem::FileSystemStorage::with_workspace_root(
                    rustain::infrastructure::paths::sessions_dir(&workspace),
                    workspace.clone(),
                ),
            ),
            Arc::new(NoOpStorage),
            Arc::new(rustain::domain::services::plan_mode_injector::DefaultPlanInjector::new()),
            Arc::new(NoOpUsageLedger),
            rustain::infrastructure::telemetry::ActiveRatioWindow::new_in_memory(),
        )
    }

    fn agent_snapshot(&self) -> Option<ActiveAgent> {
        self.restriction.as_ref().map(|restriction| {
            let (allowed_tools, exclude_tools) = match &restriction.policy {
                rustain::domain::models::ToolPolicy::Allowlist { tools } => {
                    (Some(tools.iter().cloned().collect()), None)
                }
                rustain::domain::models::ToolPolicy::Denylist { tools } => {
                    (None, Some(tools.iter().cloned().collect()))
                }
                rustain::domain::models::ToolPolicy::ResolvedAgainstParent { .. }
                | rustain::domain::models::ToolPolicy::InheritFromParent => panic!(
                    "standalone fixture requires an explicit allowlist or denylist; inherited-agent behavior belongs to the held attached/subagent slice"
                ),
            };
            ActiveAgent {
                name: restriction.agent_name.clone(),
                file: PathBuf::new(),
                body: String::new(),
                allowed_tools,
                exclude_tools,
                model: None,
            }
        })
    }

    pub(crate) async fn submit_tab_with_provider(
        &mut self,
        conversation_id: &str,
        text: &str,
        provider: Arc<dyn StreamingProvider>,
        tools_port: Arc<dyn ToolSetPort>,
    ) -> (
        tokio::task::JoinHandle<()>,
        tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
    ) {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let driver = self.local_turn_driver_with_tools(provider, event_tx, tools_port);
        let activation_set = self
            .skill_activator
            .snapshot_for_turn(conversation_id)
            .await;
        let agent_snapshot = self.agent_snapshot();
        let mut lifecycle =
            rustain::infrastructure::runtime::tui_turn_lifecycle::TuiTurnLifecycle::default();
        let (state, tabs) = (&mut self.state, &mut self.tabs);
        let tab = tabs
            .find_by_conversation_mut(conversation_id)
            .expect("submitted tab conversation");
        driver
            .submit(
                UserSubmission {
                    text: text.to_owned(),
                    images: vec![],
                    synthetic: false,
                    activation_set,
                    agent_snapshot,
                    turn_cancel: tab.turn_cancel.clone(),
                },
                TurnViewState {
                    conversation: &mut tab.conversation,
                    streaming: &mut tab.streaming,
                    state,
                    lifecycle: &mut lifecycle,
                    session_manager: &mut tab.session,
                },
            )
            .await;
        let handle = lifecycle
            .take_task(conversation_id)
            .expect("LocalTurnDriver must provide its completion handle");
        let (admitted_tx, admitted_rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = tokio::spawn(async move {
            let mut completion = handle;
            loop {
                tokio::select! {
                    result = &mut completion => {
                        result.expect("turn task");
                        break;
                    }
                    event = event_rx.recv() => {
                        let Some(event) = event else { break; };
                        if let Some((event, _)) = lifecycle.admit(event) {
                            if admitted_tx.send(event).is_err() {
                                return;
                            }
                        }
                    }
                }
            }
            while let Ok(event) = event_rx.try_recv() {
                if let Some((event, _)) = lifecycle.admit(event) {
                    if admitted_tx.send(event).is_err() {
                        break;
                    }
                }
            }
        });
        (handle, admitted_rx)
    }

    pub(crate) async fn submit(&mut self, input: serde_json::Value) -> String {
        self.submit_tool("propose_team_message", input).await
    }

    pub(crate) async fn submit_tool(
        &mut self,
        tool_name: &'static str,
        input: serde_json::Value,
    ) -> String {
        self.submit_provider(tool_provider(tool_name, input)).await
    }

    pub(crate) async fn submit_provider(&mut self, provider: Arc<dyn StreamingProvider>) -> String {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let tools_port = self.tools.clone() as Arc<dyn ToolSetPort>;
        let driver = self.local_turn_driver_with_tools(provider, event_tx, tools_port);
        let mut streaming = rustain::domain::models::StreamingState::default();
        let mut lifecycle =
            rustain::infrastructure::runtime::tui_turn_lifecycle::TuiTurnLifecycle::default();
        let mut sessions = rustain::domain::models::SessionManager::new(
            rustain::domain::models::SessionState::Empty,
        );
        let cancel = CancellationToken::new();
        let agent_snapshot = self.agent_snapshot();
        self.last_turn_completed = false;
        let activation_set = self
            .skill_activator
            .snapshot_for_turn(&self.conversation.id)
            .await;
        driver
            .submit(
                UserSubmission {
                    text: "scripted proposal".into(),
                    images: vec![],
                    synthetic: false,
                    activation_set,
                    agent_snapshot,
                    turn_cancel: cancel,
                },
                TurnViewState {
                    conversation: &mut self.conversation,
                    streaming: &mut streaming,
                    state: &mut self.state,
                    lifecycle: &mut lifecycle,
                    session_manager: &mut sessions,
                },
            )
            .await;
        let handle = lifecycle
            .take_task(&self.conversation.id)
            .expect("LocalTurnDriver must provide its completion handle");
        tokio::time::timeout(SETTLE, handle)
            .await
            .expect("turn completion timeout")
            .expect("turn task");
        self.last_turn_completed = true;
        self.last_tool_results.clear();
        self.last_tool_errors.clear();
        while let Ok(event) = event_rx.try_recv() {
            let Some((event, _)) = lifecycle.admit(event) else {
                continue;
            };
            if let AppEvent::ProviderChunk {
                chunk:
                    StreamChunk::ToolResult {
                        content, is_error, ..
                    },
                ..
            } = event
            {
                self.last_tool_results.push(content);
                self.last_tool_errors.push(is_error);
            }
        }
        self.last_transitions.clear();
        loop {
            match self.transitions.try_recv() {
                Ok(transition) => self.last_transitions.push(transition),
                Err(
                    tokio::sync::broadcast::error::TryRecvError::Empty
                    | tokio::sync::broadcast::error::TryRecvError::Closed,
                ) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(count)) => {
                    panic!("lost {count} scheduler transitions")
                }
            }
        }
        while let Ok(event) = self.sender.domain_rx.try_recv() {
            if let AppEvent::TeamProposalReady { proposal } = event {
                rustain::infrastructure::runtime::team_proposal_bridge::proposal_ready(
                    &mut self.state,
                    &self.sender.app_state,
                    proposal,
                )
                .await;
            }
        }
        self.last_tool_results
            .last()
            .cloned()
            .expect("completed proposal turn must emit a tool result")
    }
}
