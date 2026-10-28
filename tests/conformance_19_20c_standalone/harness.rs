use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use futures::stream::BoxStream;
use rustain::adapters::filesystem::FileSystemStorage;
use rustain::adapters::noop::{
    NoOpApprovalPersistence, NoOpContext, NoOpPersona, NoOpStorage, NoOpUsageLedger,
};
use rustain::adapters::security_adapter::SecurityAdapter;
use rustain::adapters::skill_activation::SkillActivator;
use rustain::adapters::tui::app::{self, InputAction};
use rustain::adapters::tui::state::TuiState;
use rustain::adapters::tui::widgets::permission_prompt;
use rustain::domain::errors::{ProviderError, ToolError};
use rustain::domain::events::{AppEvent, ToolProgressEvent};
use rustain::domain::models::tool_call::{RequestId, ToolCall, ToolCallTransition};
use rustain::domain::models::{
    AppConfig, CompletionOptions, ModelDescriptor, PermissionMode, PermissionResolution,
    SessionManager, SessionState, StopReason, StreamChunk, StreamingState, ToolDefinition,
    ToolInvocation, ToolResult,
};
use rustain::domain::ports::{
    ContextPort, ProbeOutcome, SecurityPort, StreamingProvider, ToolSetPort,
};
use rustain::domain::services::approval_runtime::{
    ApprovalRuntime, ApprovalRuntimeEvent, PendingReview,
};
use rustain::domain::services::tool_scheduler::ToolScheduler;
use rustain::infrastructure::runtime::approval_bridge;
use rustain::infrastructure::runtime::event_bus::EventBus;
use rustain::infrastructure::runtime::turn_driver::{
    LocalTurnDriver, TurnViewState, UserSubmission,
};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

const SETTLE: Duration = Duration::from_secs(45);
pub(crate) const VIEWPORTS: [(u16, u16); 3] = [(60, 16), (80, 24), (130, 30)];

/// Only the provider and executor are fixtures. No fixture constructs a pending
/// permission, invokes ApprovalRuntime::request/resolve, or schedules a bypass.
struct ScriptedProvider {
    calls: Vec<(&'static str, Value)>,
    completions: AtomicUsize,
}

#[async_trait::async_trait]
impl StreamingProvider for ScriptedProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<rustain::domain::models::Message>,
        _options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        let mut chunks = Vec::new();
        let first = self.completions.fetch_add(1, Ordering::SeqCst) == 0;
        if first {
            for (index, (name, input)) in self.calls.iter().enumerate() {
                chunks.push(StreamChunk::ToolUse {
                    id: format!("exact-{index}"),
                    name: (*name).to_owned(),
                    input: input.clone(),
                });
            }
        }
        chunks.push(StreamChunk::TurnComplete {
            stop_reason: if first {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            },
        });
        Ok(Box::pin(futures::stream::iter(chunks)))
    }
    async fn abort(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    fn provider_id(&self) -> String {
        "exact-approval-conformance".into()
    }
    fn list_models(&self) -> Vec<ModelDescriptor> {
        vec![]
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

#[derive(Default)]
pub(crate) struct CapturedTools {
    pub(crate) executed: parking_lot::Mutex<Vec<(String, Value)>>,
    pub(crate) permission_resolutions: parking_lot::Mutex<Vec<Option<PermissionResolution>>>,
}

#[async_trait::async_trait]
impl ToolSetPort for CapturedTools {
    fn available_tools(&self) -> Vec<ToolDefinition> {
        [
            "Bash",
            "Write",
            "Edit",
            "a2a__peer__skill",
            "unregistered_exact_tool",
        ]
        .into_iter()
        .map(|name| ToolDefinition {
            name: name.into(),
            description: "Capture the executor boundary without interpreting input".into(),
            input_schema: serde_json::json!({}),
            parallel_safe: true,
        })
        .collect()
    }
    // All fixtures are independent side-effect captures, allowing the real
    // scheduler to raise a batch of pending approvals before any is decided.
    fn is_parallel_safe(&self, _tool: &str) -> bool {
        true
    }
    async fn execute_invocation(
        &self,
        tool_name: &str,
        _tool_use_id: &str,
        input: Value,
        cancel: CancellationToken,
        _progress_tx: Option<mpsc::UnboundedSender<ToolProgressEvent>>,
        invocation: &ToolInvocation<'_>,
    ) -> Result<ToolResult, ToolError> {
        self.permission_resolutions
            .lock()
            .push(invocation.permission_resolution);
        self.execute(tool_name, input, cancel).await
    }
    async fn execute(
        &self,
        tool_name: &str,
        input: Value,
        _cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        self.executed.lock().push((tool_name.into(), input));
        Ok(ToolResult {
            tool_use_id: String::new(),
            content: "captured".into(),
            is_error: false,
            images: Vec::new(),
        })
    }
}

pub(crate) struct Fixture {
    pub(crate) state: TuiState,
    pub(crate) runtime: Arc<ApprovalRuntime>,
    pub(crate) tools: Arc<CapturedTools>,
    pub(crate) closed: HashSet<RequestId>,
    pub(crate) requested: HashSet<RequestId>,
    pub(crate) errors: Vec<String>,
    bus: Arc<EventBus>,
    events: mpsc::UnboundedReceiver<AppEvent>,
    transitions: broadcast::Receiver<ToolCallTransition>,
    scheduler: Arc<ToolScheduler>,
    security: Arc<SecurityAdapter>,
    skill: Arc<SkillActivator>,
    config: Arc<ArcSwap<AppConfig>>,
    workspace: tempfile::TempDir,
    turns: Vec<tokio::task::JoinHandle<()>>,
    bridge: tokio::task::JoinHandle<()>,
}

impl Fixture {
    pub(crate) async fn new(capacity: usize) -> Self {
        let workspace = tempfile::tempdir().expect("workspace");
        let security = Arc::new(SecurityAdapter::new(workspace.path().to_path_buf()));
        security.set_mode(PermissionMode::Normal);
        let tools = Arc::new(CapturedTools::default());
        let runtime = ApprovalRuntime::new(capacity, Arc::new(NoOpApprovalPersistence));
        let scheduler = ToolScheduler::new(security.clone(), tools.clone(), runtime.clone(), 512);
        let transitions = scheduler.subscribe();
        let (bus, events) = EventBus::new(512);
        let bus = Arc::new(bus);
        let bridge = approval_bridge::spawn(runtime.clone(), bus.clone());
        Self {
            state: TuiState::new(80, 24),
            runtime,
            tools,
            closed: HashSet::new(),
            requested: HashSet::new(),
            errors: vec![],
            bus,
            events,
            transitions,
            scheduler,
            security,
            skill: Arc::new(SkillActivator::new()),
            config: Arc::new(ArcSwap::from_pointee(AppConfig::default())),
            workspace,
            turns: vec![],
            bridge,
        }
    }
    #[cfg(feature = "test-instrumentation")]
    pub(crate) async fn change_approval_resume(
        &self,
        change: rustain::domain::services::tool_scheduler::ApprovalResumeChange,
    ) {
        self.scheduler
            .change_next_approval_resume_for_test(change)
            .await;
    }

    pub(crate) async fn submit(&mut self, calls: Vec<(&'static str, Value)>) {
        let root = self.workspace.path().to_path_buf();
        let provider = Arc::new(ScriptedProvider {
            calls,
            completions: AtomicUsize::new(0),
        });
        let context: Arc<dyn ContextPort> = Arc::new(NoOpContext);
        let driver = LocalTurnDriver::new(
            provider,
            self.config.clone(),
            self.bus.domain_tx.clone(),
            self.security.clone(),
            self.tools.clone(),
            self.scheduler.clone(),
            self.skill.clone(),
            Arc::new(NoOpPersona),
            Arc::new(ArcSwap::from_pointee(context)),
            Arc::new(ArcSwap::from_pointee(None)),
            root.clone(),
            Arc::new(FileSystemStorage::with_workspace_root(
                rustain::infrastructure::paths::sessions_dir(&root),
                root,
            )),
            Arc::new(NoOpStorage),
            Arc::new(rustain::domain::services::plan_mode_injector::DefaultPlanInjector::new()),
            Arc::new(NoOpUsageLedger),
            rustain::infrastructure::telemetry::ActiveRatioWindow::new_in_memory(),
        );
        let tabs = rustain::domain::models::tab::TabManager::new(CancellationToken::new());
        let mut conversation = tabs.active_tab().conversation.clone();
        let mut streaming = StreamingState::default();
        let mut session = SessionManager::new(SessionState::Empty);
        let mut active_turn = None;
        driver
            .submit(
                UserSubmission {
                    text: "Inspect the exact requested operation".into(),
                    images: vec![],
                    synthetic: false,
                    activation_set: None,
                    agent_snapshot: None,
                    turn_cancel: CancellationToken::new(),
                },
                TurnViewState {
                    conversation: &mut conversation,
                    streaming: &mut streaming,
                    state: &mut self.state,
                    active_turn: &mut active_turn,
                    session_manager: &mut session,
                },
            )
            .await;
        self.turns
            .push(active_turn.expect("real LocalTurnDriver launches run_turn"));
    }

    fn apply_event(
        &mut self,
        event: AppEvent,
    ) -> Option<(ApprovalRuntimeEvent, Option<PendingReview>)> {
        match event {
            AppEvent::ApprovalRuntimeEventBridged { event, review } => {
                match &event {
                    ApprovalRuntimeEvent::Requested { id, .. } => {
                        self.requested.insert(id.clone());
                    }
                    ApprovalRuntimeEvent::Resolved { id, .. }
                    | ApprovalRuntimeEvent::Cancelled { id, .. } => {
                        self.closed.insert(id.clone());
                    }
                }
                approval_bridge::apply(&mut self.state, event.clone(), review.clone());
                Some((event, review))
            }
            AppEvent::ProviderChunk {
                chunk:
                    StreamChunk::ToolResult {
                        content,
                        is_error: true,
                        ..
                    },
                ..
            } => {
                self.errors.push(content);
                None
            }
            _ => None,
        }
    }

    pub(crate) async fn next_approval(&mut self) -> (ApprovalRuntimeEvent, Option<PendingReview>) {
        tokio::time::timeout(SETTLE, async {
            loop {
                let event = self
                    .events
                    .recv()
                    .await
                    .expect("live production bridge bus");
                if let Some(approval) = self.apply_event(event) {
                    return approval;
                }
            }
        })
        .await
        .expect("production approval bridge must deliver an event")
    }

    pub(crate) async fn wait_requested(&mut self) -> (ApprovalRuntimeEvent, Option<PendingReview>) {
        loop {
            let approval = self.next_approval().await;
            if matches!(approval.0, ApprovalRuntimeEvent::Requested { .. }) {
                return approval;
            }
        }
    }

    pub(crate) async fn wait_closed(&mut self, id: &RequestId) {
        while !self.closed.contains(id) {
            self.next_approval().await;
        }
    }

    pub(crate) async fn await_batch_pending(&mut self, count: usize) -> Vec<RequestId> {
        tokio::time::timeout(SETTLE, async {
            let mut ids = Vec::new();
            while ids.len() < count {
                let transition = self
                    .transitions
                    .recv()
                    .await
                    .expect("scheduler transition receiver");
                if let ToolCall::AwaitingApproval { approval_id, .. } = transition.call {
                    ids.push(approval_id);
                }
            }
            ids
        })
        .await
        .expect("real scheduler reaches every approval boundary")
    }

    pub(crate) fn current_id(&self) -> RequestId {
        self.state
            .pending_permission
            .as_ref()
            .expect("shown approval")
            .id
            .clone()
    }

    pub(crate) fn paint(&mut self, width: u16, height: u16) -> Vec<String> {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                let layout = rustain::adapters::tui::layout::compute_layout(
                    frame.area(),
                    &self.state.theme,
                    "",
                    1,
                    false,
                    rustain::domain::models::DensityMode::Focus,
                )
                .expect("real minimum chat viewport");
                permission_prompt::render_permission_surface(
                    frame,
                    layout.chat_pane,
                    self.state.pending_permission.as_ref(),
                    self.state.pending_feedback_input.as_ref(),
                    self.state.permission_queue.len(),
                    &mut self.state.permission_reader,
                    &self.state.theme,
                );
            })
            .expect("real permission overlay paint");
        terminal
            .backend()
            .buffer()
            .content
            .chunks(usize::from(width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect()
    }

    pub(crate) async fn raw(&mut self, event: Event) -> Option<InputAction> {
        let domain = app::convert_crossterm_event(&event, &Default::default())?;
        let action = app::handle_operator_input(&mut self.state, &event, &domain)?;
        approval_bridge::handle_action(&mut self.state, &action, &self.runtime).await;
        Some(action)
    }

    pub(crate) async fn key(&mut self, code: KeyCode) -> Option<InputAction> {
        self.raw(press(code)).await
    }

    pub(crate) async fn review_pages(&mut self, width: u16, height: u16) -> Vec<Vec<String>> {
        let mut pages = vec![self.paint(width, height)];
        // Overlapping pages via the production PageDown key also exercise
        // footer ownership. Scroll exhaustion, not repeated text or time, ends traversal.
        for _ in 0..1024 {
            let previous_scroll = self.state.permission_reader.scroll;
            self.key(KeyCode::PageDown).await;
            let page = self.paint(width, height);
            if self.state.permission_reader.scroll == previous_scroll {
                return pages;
            }
            pages.push(page);
        }
        panic!("Review must have a finite reachable final page");
    }

    pub(crate) async fn finish(&mut self) {
        for turn in self.turns.drain(..) {
            tokio::time::timeout(SETTLE, turn)
                .await
                .expect("turn finishes after decisions")
                .expect("turn task");
        }
        while let Ok(event) = self.events.try_recv() {
            self.apply_event(event);
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.bridge.abort();
        for turn in &self.turns {
            turn.abort();
        }
    }
}

pub(crate) fn press(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

pub(crate) fn text(pages: &[Vec<String>]) -> String {
    pages
        .iter()
        .flatten()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn request_id(event: &ApprovalRuntimeEvent) -> RequestId {
    match event {
        ApprovalRuntimeEvent::Requested { id, .. } => id.clone(),
        _ => panic!("expected genuine Requested event"),
    }
}
