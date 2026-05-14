//! # Event Loop Invariant
//!
//! No `.await` inside a `tokio::select!` branch of this loop may depend on
//! a future event produced by the same loop. Every `.await` must resolve
//! via something external: network, file I/O, timer, or a channel whose
//! sender lives in a SPAWNED task (not in the select body).
//!
//! Violations cause a self-deadlock: the loop parks awaiting a signal
//! that can only be produced by a branch that cannot fire while the loop
//! is parked. Symptom: the TUI freezes; only SIGKILL recovers.
//!
//! See `5-2-appendix-trust-deadlock-analysis.md` for the 2026-04-21
//! incident that introduced this invariant.
//!
//! # EventBus Invariant (Story 6-0a)
//!
//! All event production in this loop MUST go through `EventBus::emit_domain`
//! so that the broadcast tail stays in sync with the mpsc stream. Never write
//! to `domain_tx` directly from outside `emit_domain`. See
//! `event_bus.rs` module doc-comment for the full dual-channel contract.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use crossterm::event::EventStream;
use futures::{FutureExt, StreamExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::adapters::command_registry::CommandRegistry;
use crate::adapters::file_scanner;
use crate::adapters::palette_registry::PaletteRegistry;
use crate::adapters::skill_activation::SkillActivator;
use crate::adapters::skill_registry::SkillRegistry;
use crate::adapters::tui::app::{InputAction, convert_crossterm_event, handle_input};
use crate::adapters::tui::color_detect::detect_color_capability;
use crate::adapters::tui::layout;
use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::terminal::Tui;
use crate::adapters::tui::widgets::{
    autocomplete_popup, chat_pane, command_palette as command_palette_widget, help_overlay,
    input_box, model_selector, reverse_search, sidebar, status_bar, which_key_bar,
};
use crate::domain::services::session_index::SessionIndex;

/// Timeout for background tasks (title generation, session save).
/// Separate from shutdown persist timeout (2s) which is more critical.
const BACKGROUND_TASK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
use crate::domain::events::{AppEvent, ChunkAction};
use crate::domain::models::tab::TabManager;
use crate::domain::models::turn::TurnId;
use crate::domain::models::visual::{ConfirmationType, DeleteConfirmTarget, OverlayType};
use crate::domain::models::{
    AppConfig, ApprovalOutcome, ChatMessage, CompletionOptions, ContentBlockType, Conversation,
    FeedbackAction, FeedbackBlock, FeedbackLevel, FocusState, ImageAttachment, MessageRole,
    NoticeLevel, PermissionMode, PlanStatus, PlanTaskStatus, RetryState, SessionManager,
    SessionState, StatusState, StreamChunk, StreamingState, UserMessage, ViewState,
    generate_conversation_id, next_delay,
};
use crate::domain::ports::{
    ClipboardPort, PersonaPort, SecurityPort, StoragePort, StreamingProvider, ToolSetPort,
    UsageLedgerPort,
};
use crate::domain::services::message_builder;
use crate::domain::services::plan_mode_injector::PlanModeInjector;
use crate::domain::services::reducer::{reduce, update_streaming_mirror};
use crate::domain::services::turn_queue::TurnQueue;
use crate::infrastructure::runtime::app_state::AppState;
use crate::infrastructure::runtime::turn;

/// Apply a warning-level notice to TUI state.
/// Creates a FeedbackBlock, sets active_feedback_id, and returns the block ID.
/// Does NOT mutate focus — this is the regression guard for AC6.
fn apply_warning_notice(state: &mut TuiState, msg: String) -> String {
    static WFB_COUNTER: AtomicUsize = AtomicUsize::new(0);
    let fb_id = format!("wfb-{}", WFB_COUNTER.fetch_add(1, Ordering::Relaxed));
    let fb = FeedbackBlock {
        id: fb_id.clone(),
        level: FeedbackLevel::Warning,
        message: msg,
        actions: vec![FeedbackAction::Dismiss],
    };
    state.feedback_blocks.insert(fb_id.clone(), fb);
    state.active_feedback_id = Some(fb_id.clone());
    fb_id
}

/// Story 16.6 helper: pre/post layout + reconcile + mirror for fold mutations.
/// Eliminates ~30-line duplication across the five fold-toggle dispatch arms.
fn reconcile_fold_toggle<F>(
    tab_manager: &mut crate::domain::models::tab::TabManager,
    state: &mut TuiState,
    conversation: &Conversation,
    event_turn_id: TurnId,
    mutate: F,
) where
    F: FnOnce(&mut ViewState, &mut crate::adapters::tui::state::TabRenderState),
{
    let vp_height = state.viewport_height as usize;
    let width = state.terminal_width;
    let theme = state.theme.clone();
    let vs_before = tab_manager.active_tab().view_state.clone();
    let clock = tab_manager.active_tab().clock.clone();
    let tool_block_states = state.tool_block_states.clone();
    let pre_layout = {
        let rs = state.tab_render_state(state.active_tab_id);
        chat_pane::build_layout_metrics(
            conversation,
            &vs_before,
            rs,
            &theme,
            width,
            vp_height,
            &*clock,
            &tool_block_states,
        )
    };
    let prev_focused_turn_top = pre_layout.focused_turn_top;
    let prev_max_offset = pre_layout
        .total_content_height
        .saturating_sub(pre_layout.viewport_height);
    {
        let tab = tab_manager.active_tab_mut();
        let rs = state.tab_render_state(state.active_tab_id);
        mutate(&mut tab.view_state, rs);
    }
    let tool_block_states = state.tool_block_states.clone();
    let post_layout = {
        let rs = state.tab_render_state(state.active_tab_id);
        chat_pane::build_layout_metrics(
            conversation,
            &tab_manager.active_tab().view_state,
            rs,
            &theme,
            width,
            vp_height,
            &*clock,
            &tool_block_states,
        )
    };
    let resolved = tab_manager.active_tab_mut().view_state.reconcile(
        Some(crate::domain::models::ViewEvent::FoldToggle {
            turn_id: event_turn_id,
            prev_focused_turn_top,
            prev_max_offset,
        }),
        &post_layout,
    );
    tab_manager.active_tab_mut().view_state.scroll_offset = resolved;
    state.scroll_snapshot = resolved;
    state.auto_snapshot = matches!(
        tab_manager.active_tab().view_state.mode,
        crate::domain::models::AnchorMode::Following
    );
    state.needs_redraw = true;
}

/// S16.8 AC15: Two-stage anchor-confirmation gate for scroll-intent events.
///
/// When the user is Pinned and emits a scroll-intent (j/k/wheel/page-scroll):
/// first tick shows a toast and no-ops; second tick within 2000ms drops the
/// anchor via `ViewEvent::DropAnchorAndScroll` and applies the scroll.
///
/// Jump-intents (G, gg) and non-scroll inputs (BlockJump) are NOT gated here.
fn apply_scroll_intent(
    tab_manager: &mut crate::domain::models::tab::TabManager,
    state: &mut TuiState,
    conversation: &Conversation,
    app_state: &AppState,
    delta: crate::domain::models::view_state::ScrollDelta,
) {
    use crate::domain::models::AnchorMode;

    let is_pinned = matches!(
        tab_manager.active_tab().view_state.mode,
        AnchorMode::Pinned(_)
    );

    if !is_pinned {
        dispatch_view_scroll(tab_manager, state, conversation, delta);
        return;
    }

    // Pinned: two-stage confirmation (AC15 clauses 1-3).
    let clock = tab_manager.active_tab().clock.clone();
    let now = clock.now();
    let needs_toast = match state.pending_anchor_drop {
        None => true,
        Some(t) => now.duration_since(t) > std::time::Duration::from_millis(2000),
    };

    if needs_toast {
        state.pending_anchor_drop = Some(now);
        app_state.event_bus.emit_domain(AppEvent::SystemNotice {
            conversation_id: Some(conversation.id.clone()),
            level: crate::domain::models::NoticeLevel::Info,
            message: "Anchored to this turn. Scroll again to release, or press ]] .".to_string(),
        });
        state.needs_redraw = true;
    } else {
        // Second tick within 2000ms: drop anchor via explicit ViewEvent.
        state.pending_anchor_drop = None;
        // Pre-flip mode to Reading so apply_scroll sees Reading not Pinned.
        tab_manager.active_tab_mut().view_state.mode = AnchorMode::Reading;
        dispatch_view_scroll(tab_manager, state, conversation, delta);
    }
}

/// Dispatch a scroll delta through `view_state.reconcile()` — the single
/// write-path into ViewState.
///
/// D1 (2026-05-03): Rewritten from direct mutation to the reconcile pathway.
/// Uses a minimal LayoutMetrics built from state.total_content_height (the
/// render-derived height from `chat_pane::render`, which walks `messages`)
/// rather than from `build_layout_metrics` (which walks `conversation.turns`).
/// These can diverge; the renderer's value is what the user sees on screen.
///
/// Scroll math is exclusively in `view_state.rs::apply_scroll`.
fn dispatch_view_scroll(
    tab_manager: &mut crate::domain::models::tab::TabManager,
    state: &mut TuiState,
    _conversation: &Conversation,
    delta: crate::domain::models::view_state::ScrollDelta,
) {
    use crate::domain::models::ViewEvent;
    use crate::domain::models::view_state::LayoutMetrics;

    let layout = LayoutMetrics {
        viewport_height: state.viewport_height as usize,
        total_content_height: state.total_content_height,
        turn_top_offsets: vec![],
        focused_turn_top: None,
    };

    let resolved = tab_manager
        .active_tab_mut()
        .view_state
        .reconcile(Some(ViewEvent::Scroll(delta)), &layout);

    state.scroll_snapshot = resolved;
    state.auto_snapshot = matches!(
        tab_manager.active_tab().view_state.mode,
        crate::domain::models::AnchorMode::Following
    );
    state.needs_redraw = true;
}

/// Run the 4-branch tokio::select! event loop.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    terminal: &mut Tui,
    mut domain_events_rx: mpsc::UnboundedReceiver<AppEvent>,
    app_state: AppState,
    config: &AppConfig,
    provider: Arc<dyn StreamingProvider>,
    router: Arc<crate::adapters::provider::ProviderRouter>,
    security: Arc<dyn SecurityPort>,
    tools: Arc<dyn ToolSetPort>,
    persona: Arc<dyn PersonaPort>,
    storage: Arc<dyn StoragePort>,
    fs_storage: Arc<crate::adapters::filesystem::FileSystemStorage>,
    clipboard: Arc<dyn ClipboardPort>,
    workspace_path: std::path::PathBuf,
    restored_conversation: Option<Conversation>,
    recovery_prompt: Option<(String, u32)>,
    skill_activator: Arc<SkillActivator>,
    agent_activator: Arc<crate::adapters::agent_activation::AgentActivator>,
    approval_runtime: Arc<crate::domain::services::approval_runtime::ApprovalRuntime>,
    progress_tx: Option<mpsc::UnboundedSender<crate::domain::events::ToolProgressEvent>>,
    progress_rx: Option<mpsc::UnboundedReceiver<crate::domain::events::ToolProgressEvent>>,
) -> Result<()> {
    let domain_tx = app_state.event_bus.domain_tx.clone();
    let size = terminal.size()?;
    let capability = detect_color_capability();
    let mut state = TuiState::with_capability(size.width, size.height, capability);
    state.skill_registry = skill_activator.registry_arc();
    state.auto_open_on_task_plan = config.layout.auto_panels.on_task_plan.clone();

    // Cache multiplexer detection once at startup (UX-DR62)
    state.multiplexer_detected = crate::adapters::tui::help_data::is_multiplexer_session();

    // Cache VS Code terminal detection once at startup (Sprint Change Proposal 2026-04-08, AC#4, AC9)
    state.is_vscode = crate::infrastructure::utils::is_vscode_terminal();

    // Load and increment session count for contextual hint fading (UX-DR96)
    state.session_count = crate::adapters::tui::hints::load_and_increment_session_count();
    // Compute initial hint
    state.current_hint = crate::adapters::tui::hints::contextual_hint(
        &state.focus,
        state.session_count,
        state.theme.timing.status_hint_fade_sessions,
        state.is_vscode,
    );

    // Set project context indicator based on persona
    state.has_project_context = !persona.system_prompt(&workspace_path).is_empty();

    let mut terminal_events = EventStream::new();
    let mut tick_interval = tokio::time::interval(std::time::Duration::from_millis(
        state.theme.timing.tick_interval_ms,
    ));

    // Story 7.2: pending async health check for model/provider switcher (AC4).
    // Stored here so the tick branch can poll completion without blocking the event loop.
    type PendingHealthCheck = (
        String,
        String,
        tokio::task::JoinHandle<Result<(), crate::domain::errors::ProviderError>>,
    );
    let mut pending_health_check: Option<PendingHealthCheck> = None;

    // Tab manager — owns all per-tab state; standalone proxies stay in sync with the active tab
    let mut tab_manager = if let Some(conv) = restored_conversation {
        TabManager::with_conversation(conv, app_state.session_cancel.clone())
    } else {
        TabManager::new(app_state.session_cancel.clone())
    };
    // Ensure session_id is set on the active tab's conversation
    if tab_manager.active_tab().conversation.session_id.is_none() {
        let sid = generate_conversation_id();
        tab_manager.active_tab_mut().conversation.session_id = Some(sid);
    }

    // Story 4-4 AC8/AC9: hydrate session_meta for the restored conversation
    // so bookmarks from previous sessions survive reload. Failure is silent —
    // fresh/new conversations have no meta.json yet and that's the correct
    // default (empty bookmarks).
    {
        let conv_id = tab_manager.active_tab().conversation.id.clone();
        if let Ok(Some(meta)) = fs_storage.load_session_meta(&conv_id).await {
            tab_manager.active_tab_mut().session_meta = meta;
        }
    }

    // Story 6-0b: construct ToolScheduler once and spawn bridge task
    let tool_scheduler = crate::domain::services::tool_scheduler::ToolScheduler::new(
        security.clone(),
        tools.clone(),
        approval_runtime.clone(),
        config.runtime.event_bus.raw_capacity,
    );

    // Story 16.9: wire progress channel to ToolScheduler
    let mut progress_rx = progress_rx;
    if let Some(ref tx) = progress_tx {
        tool_scheduler.set_progress_tx(Some(tx.clone())).await;
    }

    // Story 6-0d: warm up plan slug + directory when starting in Plan mode (default_plan_mode)
    if security.current_mode() == PermissionMode::Plan {
        let mut meta = tab_manager.active_tab_mut().session_meta.clone();
        let _ = app_state.plan_manager.ensure_dir().await;
        let plan = app_state.plan_manager.plan_file_for(&mut meta);
        tab_manager.active_tab_mut().session_meta.plan_slug = meta.plan_slug;
        tool_scheduler.set_plan_file(Some(plan.path.clone())).await;
        state.plan_file_path = Some(plan.path);
        app_state.plan_injector.as_ref().reset_reentry();
        state.pending_plan_reminder_at_turn = Some(0);
    }

    // Story 6-2a: construct PlanRuntime for sequential task execution
    let plan_runtime = crate::domain::services::plan_runtime::PlanRuntime::new();

    {
        let bus = app_state.event_bus.clone();
        let mut rx = tool_scheduler.subscribe();
        tokio::spawn(async move {
            use std::time::Duration;
            use tokio::sync::broadcast::error::RecvError;
            loop {
                match tokio::time::timeout(Duration::from_secs(30), rx.recv()).await {
                    Ok(Ok(transition)) => {
                        bus.emit_domain(AppEvent::ToolCallTransitionBridged {
                            conversation_id: transition.conversation_id.clone(),
                            transition,
                        });
                    }
                    Ok(Err(RecvError::Lagged(n))) => {
                        tracing::warn!(missed = n, "tool transition subscriber lagged");
                    }
                    Ok(Err(RecvError::Closed)) => break,
                    Err(_) => continue, // idle timeout — re-poll
                }
            }
        });
    }

    // Story 6-0c: spawn ApprovalRuntime bridge task
    {
        let bus = app_state.event_bus.clone();
        let mut rx = approval_runtime.subscribe();
        tokio::spawn(async move {
            use std::time::Duration;
            use tokio::sync::broadcast::error::RecvError;
            loop {
                match tokio::time::timeout(Duration::from_secs(30), rx.recv()).await {
                    Ok(Ok(event)) => {
                        bus.emit_domain(AppEvent::ApprovalRuntimeEventBridged { event });
                    }
                    Ok(Err(RecvError::Lagged(n))) => {
                        tracing::warn!(missed = n, "approval runtime subscriber lagged");
                    }
                    Ok(Err(RecvError::Closed)) => break,
                    Err(_) => continue,
                }
            }
        });
    }

    // Active-tab proxies — always reflect the current tab; synced on every tab switch
    let mut conversation = tab_manager.active_tab().conversation.clone();
    let mut streaming = tab_manager.active_tab().streaming.clone();
    let mut turn_queue = TurnQueue::default();
    let mut _pending_save: Option<tokio::task::JoinHandle<()>> = None;

    // Lazy-initialized command registry (NFR10: not scanned at startup)
    let mut command_registry = CommandRegistry::new();

    // Shared skill registry: TuiState and SkillActivator share the same
    // Arc<tokio::sync::RwLock<SkillRegistry>> (Story 16-0 AC1, DF-159).
    // Background scan writes directly into the shared Arc.
    let agent_registry_slot: Arc<
        tokio::sync::Mutex<Option<crate::adapters::agent_registry::AgentRegistry>>,
    > = Arc::new(tokio::sync::Mutex::new(None));

    // Lazy-initialized palette registry (populated on first Ctrl+P)
    let mut palette_registry = PaletteRegistry::new();

    // Story 7.2 AC6: Register model entries in the `:` palette scope
    {
        use crate::adapters::tui::widgets::model_selector::humanize_ctx;
        use crate::domain::models::provider::ModelCapability;
        let models = app_state.provider_registry.list_all_models();
        for model in models {
            let mut cap_tags: Vec<&'static str> = Vec::new();
            if model.capabilities.contains(&ModelCapability::Vision) {
                cap_tags.push("vis");
            }
            if model.capabilities.contains(&ModelCapability::ToolUse) {
                cap_tags.push("tool");
            }
            if model.capabilities.contains(&ModelCapability::Thinking) {
                cap_tags.push("think");
            }
            if model
                .capabilities
                .contains(&ModelCapability::ParallelToolCalls)
            {
                cap_tags.push("par");
            }
            let cap_summary = if cap_tags.is_empty() {
                String::new()
            } else {
                format!(" [{}]", cap_tags.join(","))
            };
            palette_registry.register(crate::domain::models::palette::PaletteEntry {
                name: format!("{} ({})", model.display_name, model.provider_id),
                description: format!("{} ctx{}", humanize_ctx(model.context_window), cap_summary),
                shortcut: Some("Ctrl+X, M".to_string()),
                scope: crate::domain::models::palette::PaletteScope::Model,
                action: crate::domain::models::palette::PaletteAction::SwitchModel(
                    model.model_id.clone(),
                ),
            });
        }
    }

    // P2/AC9: Build SessionIndex at startup from persisted session metadata
    let mut session_index = match storage.list_conversations().await {
        Ok(summaries) => SessionIndex::build(summaries),
        Err(e) => {
            tracing::warn!("Failed to build session index at startup: {}", e);
            SessionIndex::new()
        }
    };
    // Mark the active conversation as open and active in the index
    session_index.set_open(&conversation.id, true);
    session_index.set_active(Some(&conversation.id));
    state.sidebar_entry_count = session_index.len();

    // P4: Cache last filter text to avoid re-scanning on every keystroke
    let mut last_autocomplete_filter = String::new();
    // P6: Cache last palette filter text to avoid re-filtering on every keystroke
    let mut last_palette_filter = String::new();

    // Initialize SessionManager from the active tab
    let mut session_manager = tab_manager.active_tab().session.clone();

    // Mark session as in-flight (clean_exit = false) for crash detection
    if !conversation.messages.is_empty() {
        let fs_ref = fs_storage.clone();
        let conv_clone = conversation.clone();
        if let Err(e) = fs_ref.save_conversation_with_exit(&conv_clone, false).await {
            tracing::warn!("Failed to mark session in-flight: {}", e);
        }
    }

    // Track active turn task for cancellation
    let mut _active_turn: Option<tokio::task::JoinHandle<()>> = None;

    // Send recovery prompt if crash detected (before first render)
    if let Some((title, token_count)) = recovery_prompt {
        app_state.event_bus.emit_domain(AppEvent::RecoveryPrompt {
            conversation_id: conversation.id.clone(),
            title,
            token_count,
        });
    }

    // Story 6-2a AC11: flip stale Running tasks back to Pending on reload
    // and emit warning notices for mid-execution plans.
    {
        let mut plans_to_warn: Vec<(String, String, u32)> = Vec::new();
        for plan in conversation.plans.values_mut() {
            if plan.status == PlanStatus::Executing {
                for task in &mut plan.tasks {
                    if task.status == PlanTaskStatus::Running {
                        plans_to_warn.push((plan.id.clone(), plan.title.clone(), task.number));
                        task.status = PlanTaskStatus::Pending;
                        task.started_at_ms = None;
                    }
                }
            }
        }
        for (_pid, ptitle, tnum) in &plans_to_warn {
            app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                conversation_id: Some(conversation.id.clone()),
                level: NoticeLevel::Warning,
                message: format!(
                    "Plan '{}' was mid-execution at exit (task {} was running). Plan execution was halted. Resume from the task panel (6.3) or invoke /plan resume <id> (later) to continue.",
                    ptitle, tnum
                ),
            });
        }
    }

    // Render first frame immediately
    match render(
        terminal,
        &mut state,
        &conversation,
        &streaming,
        &config.model,
        &router.active_delegate_id(),
        security.as_ref(),
        tab_manager.tab_count(),
        tab_manager.active_tab_index(),
        Some(&tab_manager),
        &session_index,
    ) {
        Ok(()) => state.needs_redraw = false,
        Err(e) => {
            handle_render_error(e, &mut _active_turn, &mut streaming, &mut state, terminal);
            if state.should_quit {
                return Ok(());
            }
        }
    }

    // Story 5-1 AC6: Spawn skill discovery as background task after first frame.
    // `SkillRegistry::discover` performs blocking filesystem I/O, so it runs on
    // a `spawn_blocking` thread to avoid stalling the async runtime (P7).
    // Story 16-0 AC1: writes directly into the shared Arc<RwLock<SkillRegistry>>
    // so TuiState and SkillActivator see the same catalog (DF-159).
    tracing::debug!("Dispatching background skill scan");
    {
        let tx = domain_tx.clone();
        let shared_registry = state.skill_registry.clone();
        let ws = workspace_path.clone();
        let disabled = config.skills.disabled.clone();
        let home = dirs::home_dir();
        tokio::spawn(async move {
            let result = tokio::time::timeout(
                BACKGROUND_TASK_TIMEOUT,
                tokio::task::spawn_blocking(move || {
                    SkillRegistry::discover(&ws, home.as_deref(), &disabled)
                }),
            )
            .await;
            let (registry_opt, log_msgs) = handle_scan_result(result);
            for msg in &log_msgs {
                tracing::warn!("{}", msg);
            }
            match registry_opt {
                Some(registry) => {
                    let count = registry.skills().len();
                    let warnings = registry.warnings_count();
                    tracing::debug!(
                        "Background skill scan complete: {count} skills ({warnings} warnings)"
                    );
                    {
                        let mut g = shared_registry.write().await;
                        *g = registry;
                    }
                    let _ = tx.send(AppEvent::SkillsDiscovered { count, warnings });
                }
                None => {
                    tracing::debug!("Background skill scan failed or timed out");
                    let _ = tx.send(AppEvent::SkillsDiscovered {
                        count: 0,
                        warnings: 0,
                    });
                }
            }
        });
    }

    // Story 5.4 AC8: Spawn agent discovery as background task after first frame.
    tracing::debug!("Dispatching background agent scan");
    {
        let tx = domain_tx.clone();
        let ws = workspace_path.clone();
        let activator = agent_activator.clone();
        let agent_slot = agent_registry_slot.clone();
        tokio::spawn(async move {
            let result = tokio::time::timeout(
                BACKGROUND_TASK_TIMEOUT,
                tokio::task::spawn_blocking(move || {
                    crate::adapters::agent_registry::AgentRegistry::discover(&ws)
                }),
            )
            .await;
            match result {
                Ok(Ok(reg)) => {
                    let count = reg.agents().len();
                    let warnings = reg.warnings_count();
                    tracing::debug!(
                        "Background agent scan complete: {count} agents ({warnings} warnings)"
                    );
                    {
                        let mut slot = agent_slot.lock().await;
                        *slot = Some(reg.clone());
                    }
                    activator.set_registry(reg).await;
                    let _ = tx.send(AppEvent::AgentsDiscovered { count, warnings });
                }
                Ok(Err(join_err)) => {
                    tracing::warn!("Agent discovery panicked: {}", join_err);
                    let _ = tx.send(AppEvent::SystemNotice {
                        conversation_id: None,
                        level: NoticeLevel::Warning,
                        message: format!("Custom agent discovery failed: {}", join_err),
                    });
                }
                Err(_) => {
                    tracing::warn!(
                        "Agent discovery timed out after {:?}",
                        BACKGROUND_TASK_TIMEOUT
                    );
                    let _ = tx.send(AppEvent::SystemNotice {
                        conversation_id: None,
                        level: NoticeLevel::Warning,
                        message: format!(
                            "Custom agent discovery timed out after {:?}",
                            BACKGROUND_TASK_TIMEOUT
                        ),
                    });
                }
            }
        });
    }

    // Story 7.3 AC9: one-shot startup provider fallback
    apply_startup_provider_fallback(&mut state, &router, &app_state, &domain_tx).await;

    loop {
        tokio::select! {
            // Branch 1: Terminal input (crossterm event stream)
            Some(event_result) = terminal_events.next() => {
                match event_result {
                    Ok(event) => {
                        if let Some(domain_event) = convert_crossterm_event(&event, &config.mouse) {
                            // Intercept input when recovery prompt is active
                            if state.active_feedback_id.as_deref() == Some("recovery") {
                                use crate::domain::events::DomainInputEvent;
                                match &domain_event {
                                    DomainInputEvent::SpecialKey(crate::domain::events::DomainKey::Enter)
                                    | DomainInputEvent::KeyPress('y') => {
                                        // Continue: dismiss recovery block, keep conversation
                                        state.feedback_blocks.remove("recovery");
                                        state.active_feedback_id = None;
                                        state.focus = FocusState::Input;
                                        state.needs_redraw = true;
                                        continue;
                                    }
                                    DomainInputEvent::SpecialKey(crate::domain::events::DomainKey::CtrlC)
                                    | DomainInputEvent::SpecialKey(crate::domain::events::DomainKey::Esc) => {
                                        // Always allow quit escape hatch
                                        state.feedback_blocks.remove("recovery");
                                        state.active_feedback_id = None;
                                        state.should_quit = true;
                                        continue;
                                    }
                                    DomainInputEvent::KeyPress('n') => {
                                        // New session: dismiss block, reset conversation
                                        state.feedback_blocks.remove("recovery");
                                        state.active_feedback_id = None;
                                        conversation.messages.clear();
                                        conversation.title = String::new();
                                        conversation.id = generate_conversation_id();
                                        conversation.session_id = Some(generate_conversation_id());
                                        conversation.created_at = crate::domain::models::session_meta::now_unix();
                                        conversation.updated_at = crate::domain::models::session_meta::now_unix();
                                        conversation.last_response_at = None;
                                        conversation.usage = None;
                                        state.focus = FocusState::Input;
                                        state.needs_redraw = true;
                                        // Persist the fresh conversation with clean_exit = true
                                        // (avoids phantom crash detection on next startup)
                                        let fs_ref = fs_storage.clone();
                                        let conv_clone = conversation.clone();
                                        if let Some(prev) = _pending_save.take() {
                                            let _ = prev.await;
                                        }
                                        _pending_save = Some(tokio::spawn(async move {
                                            match tokio::time::timeout(
                                                BACKGROUND_TASK_TIMEOUT,
                                                fs_ref.save_conversation_with_exit(&conv_clone, true),
                                            ).await {
                                                Ok(Ok(())) => {}
                                                Ok(Err(e)) => tracing::error!("Failed to persist new session: {}", e),
                                                Err(_) => tracing::warn!("Background task 'new_session_save' timed out after {}s", BACKGROUND_TASK_TIMEOUT.as_secs()),
                                            }
                                        }));
                                        continue;
                                    }
                                    _ => {
                                        // Block all other input while recovery prompt is active
                                        continue;
                                    }
                                }
                            }

                            let action = handle_input(&mut state, &domain_event);

                            // Update contextual hint whenever focus may have changed (UX-DR93)
                            if state.needs_redraw {
                                state.current_hint = crate::adapters::tui::hints::contextual_hint(
                                    &state.focus,
                                    state.session_count,
                                    state.theme.timing.status_hint_fade_sessions,
                                    state.is_vscode,
                                );
                            }

                            // P4: Only re-populate autocomplete when filter text actually changed,
                            // or when suggestions are empty (first open after skill discovery).
                            if state.autocomplete.active
                                && (state.autocomplete.filter_text != last_autocomplete_filter
                                    || state.autocomplete.suggestions.is_empty())
                            {
                                last_autocomplete_filter = state.autocomplete.filter_text.clone();
                                populate_autocomplete_suggestions(
                                    &mut state,
                                    &mut command_registry,
                                    &workspace_path,
                                )
                                .await;
                            } else if !state.autocomplete.active {
                                last_autocomplete_filter.clear();
                            }

                            // Repopulate command palette entries only when filter changes (P6)
                            if state.command_palette.active
                                && state.command_palette.filter_text != last_palette_filter
                            {
                                last_palette_filter = state.command_palette.filter_text.clone();
                                populate_palette_entries(
                                    &mut state,
                                    &mut command_registry,
                                    &mut palette_registry,
                                    &workspace_path,
                                )
                                .await;
                            } else if !state.command_palette.active {
                                last_palette_filter.clear();
                            }

                            match action {
                                InputAction::SubmitMessage(text) => {
                                    // Resolve any file mentions from autocomplete selections
                                    let mut mention_images: Vec<ImageAttachment> = Vec::new();
                                    let send_text = if !state.resolved_mentions.is_empty() {
                                        let (file_contexts, file_images, file_errors) = resolve_file_context(&state.resolved_mentions, &workspace_path, security.as_ref());
                                        mention_images = file_images;
                                        state.resolved_mentions.clear();
                                        // P9: Show FeedbackBlock for each file resolution error
                                        for err in &file_errors {
                                            let fb = FeedbackBlock {
                                                id: generate_conversation_id(),
                                                message: err.reason.clone(),
                                                level: FeedbackLevel::Error,
                                                actions: vec![],
                                            };
                                            state.feedback_blocks.insert(fb.id.clone(), fb);
                                            state.needs_redraw = true;
                                        }
                                        if !file_contexts.is_empty() {
                                            let prefix = message_builder::build_file_context_prefix(&file_contexts);
                                            format!("{}{}", prefix, text)
                                        } else {
                                            text
                                        }
                                    } else {
                                        text
                                    };
                                    // Merge pending images (clipboard paste) with mention images (@ file references)
                                    // Covers: FR112 (AC1, AC2)
                                    let mut all_images: Vec<ImageAttachment> = state.pending_images.drain(..).collect();
                                    all_images.extend(mention_images);
                                    state.image_indicator = None;

                                    if streaming.is_streaming {
                                        // Queue message during streaming
                                        let msg = UserMessage {
                                            content: send_text,
                                            images: all_images,
                                        };
                                        if turn_queue.enqueue(msg).is_err() {
                                            state.status_before_flash = Some(state.status.clone());
                                            state.status = StatusState::Flash {
                                            message: "Message queue full".to_string(),
                                            remaining_ms: state.theme.timing.status_flash_ms,
                                        };
                                            state.needs_redraw = true;
                                        }
                                    } else {
                                         let _skill_snap = skill_activator.snapshot_for_turn(&conversation.id).await;
                                         let _agent_snap = agent_activator.snapshot(&conversation.id).await;
                                          start_turn(
                                              &send_text,
                                              all_images,
                                              &mut conversation,
                                              &mut streaming,
                                              &mut state,
                                              &mut _active_turn,
                                              &provider,
                                              config,
                                              &domain_tx,
                                              &security,
                                              &tools, &tool_scheduler,
                                              &persona,
                                              &workspace_path,
                                              &mut session_manager,
                                              &fs_storage,
                                              &storage,
                                              &app_state.plan_manager, &app_state.plan_injector,
                                              None,
                                              _skill_snap,
                                              _agent_snap,
                                                                                tab_manager.reset_and_clone_turn_cancel(),
                                          app_state.usage_ledger.clone()).await;
                                        // Force immediate render for typing indicator
                                        match render(terminal, &mut state, &conversation, &streaming, &config.model, &router.active_delegate_id(), security.as_ref(), tab_manager.tab_count(), tab_manager.active_tab_index(), Some(&tab_manager), &session_index) {
                                            Ok(()) => state.needs_redraw = false,
                                            Err(e) => handle_render_error(e, &mut _active_turn, &mut streaming, &mut state, terminal),
                                        }
                                    }
                                }
                                InputAction::Quit => {
                                    state.should_quit = true;
                                }
                                InputAction::CancelOrQuit => {
                                    // Cancel active feedback input flow first (AC5)
                                    if let Some(fi) = state.pending_feedback_input.take() {
                                        let pending = fi.pending_permission;
                                        let _ = approval_runtime.resolve(&pending.id, ApprovalOutcome::Cancel).await;
                                        state.focus = FocusState::Input;
                                    }
                                    // Cancel any pending permission before aborting
                                    if let Some(pending) = state.pending_permission.take() {
                                        let _ = approval_runtime.resolve(&pending.id, ApprovalOutcome::Cancel).await;
                                        state.focus = FocusState::Input;
                                    }
                                    // Cancel all queued permission requests
                                    while let Some(queued) = state.permission_queue.pop() {
                                        let _ = approval_runtime.resolve(&queued.id, ApprovalOutcome::Cancel).await;
                                    }
                                    // Decline any pending skill trust prompt
                                    if let Some(pending) = state.pending_skill_trust.take() {
                                        let _ = pending.response_tx.send(crate::domain::models::SkillTrustResponse::Declined);
                                        state.focus = FocusState::Input;
                                    }
                                    while let Some(queued) = state.skill_trust_queue.pop_front() {
                                        let _ = queued.response_tx.send(crate::domain::models::SkillTrustResponse::Declined);
                                    }
                                    // Drain user-driven pending activation (Story 5-2 deadlock fix).
                                    if let Some(pending) = state.pending_activation.take() {
                                        state.pending_activation_inspect_content = None;
                                        app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                            conversation_id: Some(pending.conversation_id),
                                            level: NoticeLevel::Info,
                                            message: format!(
                                                "Skill '{}' activation cancelled",
                                                pending.skill_name
                                            ),
                                        });
                                        if state.pending_skill_trust.is_none() {
                                            state.focus = FocusState::Input;
                                            state.skill_trust_inspect_mode = false;
                                        }
                                        state.needs_redraw = true;
                                    }
                                    if streaming.is_streaming {
                                        // AC12: Finalize active tool calls with [aborted] before clearing
                                        for (_, tc) in streaming.active_tool_calls.iter_mut() {
                                            if tc.result.is_none() {
                                                tc.result = Some(crate::domain::models::ToolResultInfo {
                                                    content: "[aborted]".to_string(),
                                                    is_error: true,
                                                });
                                                tc.completed_at_ms = Some(crate::domain::models::session_meta::now_unix() as u64 * 1000);
                                            }
                                        }

                                        // Abort streaming: preserve partial response
                                        if !streaming.current_text_buffer.is_empty()
                                            || !streaming.active_tool_calls.is_empty()
                                        {
                                            let content = std::mem::take(&mut streaming.current_text_buffer);
                                            conversation.messages.push(ChatMessage {
                                                id: generate_conversation_id(),
                                                role: MessageRole::Assistant,
                                                content,
                                                content_blocks: std::mem::take(&mut streaming.current_blocks),
                                                tool_calls: streaming.active_tool_calls.drain().map(|(_, v)| v).collect(),
                                                created_at: crate::domain::models::session_meta::now_unix(),
                                                token_count: None,
                                                stop_reason: Some(crate::domain::models::StopReason::Cancelled),
                                                synthetic: false,
                                                images: vec![],
                                            });
                                        }
                                        // Abort the active turn task
                                        if let Some(handle) = _active_turn.take() {
                                            handle.abort();
                                        }
                                        // Reset streaming state
                                        streaming.is_streaming = false;
                                        streaming.phase = crate::domain::models::StreamingPhase::Idle;
                                        streaming.current_blocks.clear();
                                        streaming.active_tool_calls.clear();
                                        // Clear TurnQueue entirely
                                        while turn_queue.dequeue().is_some() {}
                                        // Ready for next input
                                        state.focus = FocusState::Input;
                                        state.status = StatusState::Idle;
                                        state.needs_redraw = true;
                                    } else {
                                        // Not streaming → quit
                                        state.should_quit = true;
                                    }
                                }
                                InputAction::PermissionAllow => {
                                    if let Some(pending) = state.pending_permission.take() {
                                        let _ = approval_runtime.resolve(&pending.id, ApprovalOutcome::Once).await;
                                        advance_permission_queue(&mut state);
                                    }
                                }
                                InputAction::PermissionDeny => {
                                    if let Some(pending) = state.pending_permission.take() {
                                        let _ = approval_runtime.resolve(&pending.id, ApprovalOutcome::Reject { feedback: None }).await;
                                        advance_permission_queue(&mut state);
                                    }
                                }
                                InputAction::PermissionAlwaysAllow => {
                                    if let Some(pending) = state.pending_permission.take() {
                                        use crate::domain::models::ApprovalScope;
                                        let _ = approval_runtime.resolve(&pending.id, ApprovalOutcome::AlwaysAndSave { scope: ApprovalScope::Tool(pending.tool_name.clone()) }).await;
                                        advance_permission_queue(&mut state);
                                    }
                                }
                                InputAction::PermissionSessionAllow => {
                                    if let Some(pending) = state.pending_permission.take() {
                                        let _ = approval_runtime.resolve(&pending.id, ApprovalOutcome::AlwaysTool { tool_name: pending.tool_name.clone() }).await;
                                        advance_permission_queue(&mut state);
                                    }
                                }
                                InputAction::PermissionDenyFeedback => {
                                    if let Some(pending) = state.pending_permission.take() {
                                        use crate::adapters::tui::state::FeedbackInputState;
                                        state.pending_feedback_input = Some(FeedbackInputState {
                                            buffer: String::new(),
                                            cursor: 0,
                                            pending_permission: pending,
                                        });
                                        state.focus = FocusState::Overlay(OverlayType::Confirmation(
                                            ConfirmationType::PermissionFeedback,
                                        ));
                                        state.needs_redraw = true;
                                    }
                                }
                                // Plan approval card key handlers (Story 6-0d AC4)
                                InputAction::PlanApproveNormal => {
                                    if let Some(pending) = state.pending_plan_approval.take() {
                                        app_state.event_bus.emit_domain(AppEvent::PlanApprovalResolved {
                                            conversation_id: pending.conversation_id,
                                            outcome: crate::domain::models::PlanApprovalOutcome::ApproveNormal,
                                        });
                                    }
                                }
                                InputAction::PlanApproveAutoEdit => {
                                    if let Some(pending) = state.pending_plan_approval.take() {
                                        app_state.event_bus.emit_domain(AppEvent::PlanApprovalResolved {
                                            conversation_id: pending.conversation_id,
                                            outcome: crate::domain::models::PlanApprovalOutcome::ApproveAutoEdit,
                                        });
                                    }
                                }
                                InputAction::PlanReject => {
                                    if let Some(pending) = state.pending_plan_approval.take() {
                                        app_state.event_bus.emit_domain(AppEvent::PlanApprovalResolved {
                                            conversation_id: pending.conversation_id,
                                            outcome: crate::domain::models::PlanApprovalOutcome::Reject,
                                        });
                                    }
                                }
                                InputAction::PlanRevise => {
                                    if let Some(ref pending) = state.pending_plan_approval {
                                        let plan_path = pending.plan_path.clone();
                                        let _ = crate::adapters::tui::editor_suspend::suspend_terminal(|| {
                                            let editor = crate::infrastructure::utils::env_var_trimmed("EDITOR").unwrap_or_else(|| "vi".to_string());
                                            std::process::Command::new(editor).arg(&plan_path).status()
                                        });
                                        let contents = tokio::fs::read_to_string(&plan_path).await.unwrap_or_default();
                                        if let Some(ref mut pending) = state.pending_plan_approval {
                                            pending.contents = contents;
                                        }
                                        state.needs_redraw = true;
                                    }
                                }
                                // PlanCard key handlers (Story 6-1a AC7)
                                InputAction::PlanCardApprove => {
                                    if let Some(ref pending) = state.pending_plan_card {
                                        let conversation_id = conversation.id.clone();
                                        let plan_id = pending.plan_id.clone();
                                        app_state.event_bus.emit_domain(AppEvent::PlanCardResolved {
                                            conversation_id,
                                            plan_id,
                                            decision: crate::domain::models::plan::PlanDecision::Approve,
                                        });
                                    }
                                }
                                InputAction::PlanCardReject => {
                                    if let Some(ref pending) = state.pending_plan_card {
                                        let conversation_id = conversation.id.clone();
                                        let plan_id = pending.plan_id.clone();
                                        app_state.event_bus.emit_domain(AppEvent::PlanCardResolved {
                                            conversation_id,
                                            plan_id,
                                            decision: crate::domain::models::plan::PlanDecision::Reject,
                                        });
                                    }
                                }
                                InputAction::PlanCardEdit => {
                                    if let Some(ref pending) = state.pending_plan_card {
                                        let conversation_id = conversation.id.clone();
                                        let plan_id = pending.plan_id.clone();
                                        app_state.event_bus.emit_domain(AppEvent::PlanCardResolved {
                                            conversation_id,
                                            plan_id,
                                            decision: crate::domain::models::plan::PlanDecision::Edit,
                                        });
                                    }
                                }
                                InputAction::FeedbackInputChar(c) => {
                                    if let Some(ref mut fi) = state.pending_feedback_input {
                                        // Cap feedback buffer to prevent unbounded paste into
                                        // tool-result message sent to the LLM (AC5).
                                        const MAX_FEEDBACK_LEN: usize = 2048;
                                        if fi.buffer.len() + c.len_utf8() <= MAX_FEEDBACK_LEN {
                                            fi.buffer.push(c);
                                            fi.cursor += 1;
                                            state.needs_redraw = true;
                                        }
                                    }
                                }
                                InputAction::FeedbackInputBackspace => {
                                    if let Some(ref mut fi) = state.pending_feedback_input {
                                        if fi.cursor > 0 {
                                            fi.buffer.pop();
                                            fi.cursor -= 1;
                                            state.needs_redraw = true;
                                        }
                                    }
                                }
                                InputAction::FeedbackInputSubmit => {
                                    if let Some(fi) = state.pending_feedback_input.take() {
                                        let pending = fi.pending_permission;
                                        if fi.buffer.is_empty() {
                                            let _ = approval_runtime.resolve(&pending.id, ApprovalOutcome::Reject { feedback: None }).await;
                                        } else {
                                            let feedback = fi.buffer.clone();
                                            let _ = approval_runtime.resolve(&pending.id, ApprovalOutcome::Reject { feedback: Some(feedback.clone()) }).await;
                                            // Emit FeedbackBlock into chat stream (AC5).
                                            // Escape embedded `"` so the quoted-display format
                                            // stays unambiguous even if the user typed a quote.
                                            let fb_id = generate_conversation_id();
                                            let fb = FeedbackBlock {
                                                id: fb_id.clone(),
                                                level: crate::domain::models::FeedbackLevel::Warning,
                                                message: crate::domain::services::permission_chain::format_feedback_message(&feedback),
                                                actions: vec![],
                                            };
                                            state.feedback_blocks.insert(fb_id.clone(), fb);
                                        }
                                        advance_permission_queue(&mut state);
                                    }
                                }
                                InputAction::FeedbackInputCancel => {
                                    if let Some(fi) = state.pending_feedback_input.take() {
                                        // Restore the permission prompt (AC5 — Esc cancels feedback, permission still pending)
                                        state.pending_permission = Some(fi.pending_permission);
                                        state.focus = FocusState::Overlay(OverlayType::Confirmation(
                                            ConfirmationType::Permission,
                                        ));
                                        state.needs_redraw = true;
                                    }
                                }
                                InputAction::SkillTrustAccept => {
                                    let handled = if let Some(pending) = state.pending_activation.take() {
                                        state.pending_activation_inspect_content = None;
                                        skill_activator
                                            .mark_trusted(&pending.conversation_id, pending.skill_file.clone())
                                            .await;
                                        app_state.event_bus.emit_domain(AppEvent::CompleteSkillActivation {
                                            conversation_id: pending.conversation_id,
                                            skill_name: pending.skill_name,
                                            skill_file: pending.skill_file,
                                            arguments: pending.arguments,
                                            trusted: true,
                                        });
                                        if state.pending_skill_trust.is_none() {
                                            state.focus = FocusState::Input;
                                            state.skill_trust_inspect_mode = false;
                                        }
                                        state.needs_redraw = true;
                                        true
                                    } else if let Some(pending) = state.pending_skill_trust.take() {
                                        let _ = pending.response_tx.send(crate::domain::models::SkillTrustResponse::Accepted);
                                        true
                                    } else {
                                        tracing::error!("SkillTrustAccept received with no pending trust state");
                                        false
                                    };
                                    if handled && state.pending_skill_trust.is_none() {
                                        advance_skill_trust_queue(&mut state);
                                    }
                                }
                                InputAction::SkillTrustDecline => {
                                    let handled = if let Some(pending) = state.pending_activation.take() {
                                        state.pending_activation_inspect_content = None;
                                        app_state.event_bus.emit_domain(AppEvent::CompleteSkillActivation {
                                            conversation_id: pending.conversation_id,
                                            skill_name: pending.skill_name,
                                            skill_file: pending.skill_file,
                                            arguments: pending.arguments,
                                            trusted: false,
                                        });
                                        if state.pending_skill_trust.is_none() {
                                            state.focus = FocusState::Input;
                                            state.skill_trust_inspect_mode = false;
                                        }
                                        state.needs_redraw = true;
                                        true
                                    } else if let Some(pending) = state.pending_skill_trust.take() {
                                        let _ = pending.response_tx.send(crate::domain::models::SkillTrustResponse::Declined);
                                        true
                                    } else {
                                        tracing::error!("SkillTrustDecline received with no pending trust state");
                                        false
                                    };
                                    if handled && state.pending_skill_trust.is_none() {
                                        advance_skill_trust_queue(&mut state);
                                    }
                                }
                                InputAction::SkillTrustInspect => {
                                    if let Some(pending) = &state.pending_activation {
                                        if state.pending_activation_inspect_content.is_none() {
                                            let path = pending.skill_file.clone();
                                            let content = tokio::task::spawn_blocking(move || {
                                                std::fs::read_to_string(&path)
                                                    .unwrap_or_else(|_| "(file read error)".to_string())
                                            })
                                            .await
                                            .unwrap_or_else(|_| "(file read error)".to_string());
                                            state.pending_activation_inspect_content = Some(content);
                                        }
                                    } else if let Some(ref mut pending) = state.pending_skill_trust {
                                        if pending.inspect_content.is_none() {
                                            let path = pending.skill_file.clone();
                                            let content = tokio::task::spawn_blocking(move || {
                                                std::fs::read_to_string(&path)
                                                    .unwrap_or_else(|_| "(file read error)".to_string())
                                            })
                                            .await
                                            .unwrap_or_else(|_| "(file read error)".to_string());
                                            pending.inspect_content = Some(content);
                                        }
                                    } else {
                                        continue;
                                    }
                                    state.skill_trust_inspect_mode = true;
                                    state.focus = FocusState::Overlay(OverlayType::Confirmation(
                                        ConfirmationType::SkillTrustInspect,
                                    ));
                                    state.needs_redraw = true;
                                }
                                InputAction::FeedbackRetry => {
                                    // Clear the feedback block and retry the last user message
                                    if let Some(fb_id) = state.active_feedback_id.take() {
                                        state.feedback_blocks.remove(&fb_id);
                                    }
                                    // Get retry attempt count
                                    let attempt = state.retry_state.as_ref().map_or(0, |r| r.attempt);
                                    let max_attempts: u8 = 5;
                                    if attempt >= max_attempts {
                                        state.status_before_flash = Some(state.status.clone());
                                        state.status = StatusState::Flash {
                                            message: "Max retries reached".to_string(),
                                            remaining_ms: state.theme.timing.status_flash_ms,
                                        };
                                        state.retry_state = None;
                                        state.needs_redraw = true;
                                    } else {
                                        let delay = next_delay(attempt);
                                        state.retry_state = Some(RetryState {
                                            attempt: attempt + 1,
                                            max_attempts,
                                            delay_ms: delay,
                                        });
                                        state.status = StatusState::Retrying {
                                            attempt: attempt + 1,
                                            max: max_attempts,
                                            next_in_ms: delay,
                                        };
                                        state.needs_redraw = true;

                                        // Find the last user message to retry
                                        let last_user_text = conversation
                                            .messages
                                            .iter()
                                            .rev()
                                            .find(|m| m.role == MessageRole::User)
                                            .map(|m| m.content.clone());

                                        if let Some(text) = last_user_text {
                                            // Remove failed assistant response after the last user message
                                            if let Some(pos) = conversation.messages.iter().rposition(|m| m.role == MessageRole::User) {
                                                conversation.messages.truncate(pos);
                                            }
                                            // Schedule retry after actual backoff delay
                                            let delay_duration = std::time::Duration::from_millis(delay);
                                            let tx = domain_tx.clone();
                                            let retry_text = text;
                                            let retry_conv_id = conversation.id.clone();
                                            tokio::spawn(async move {
                                                tokio::time::sleep(delay_duration).await;
                                                let _ = tx.send(AppEvent::RetryMessage {
                                                    conversation_id: retry_conv_id,
                                                    content: retry_text,
                                                });
                                            });
                                        }
                                    }
                                }
                                InputAction::FeedbackCompact => {
                                    if let Some(fb_id) = state.active_feedback_id.take() {
                                        state.feedback_blocks.remove(&fb_id);
                                    }
                                    state.needs_redraw = true;
                                }
                                InputAction::FeedbackDismiss => {
                                    if let Some(fb_id) = state.active_feedback_id.take() {
                                        state.feedback_blocks.remove(&fb_id);
                                    }
                                    state.needs_redraw = true;
                                }
                                InputAction::FeedbackStartFresh => {
                                    if let Some(fb_id) = state.active_feedback_id.take() {
                                        state.feedback_blocks.remove(&fb_id);
                                    }
                                    conversation.messages.clear();
                                    conversation.title = String::new();
                                    conversation.id = generate_conversation_id();
                                    conversation.session_id = Some(generate_conversation_id());
                                    conversation.created_at = crate::domain::models::session_meta::now_unix();
                                    conversation.updated_at = crate::domain::models::session_meta::now_unix();
                                    conversation.last_response_at = None;
                                    conversation.usage = None;
                                    state.focus = FocusState::Input;
                                    state.needs_redraw = true;
                                    let fs_ref = fs_storage.clone();
                                    let conv_clone = conversation.clone();
                                    if let Some(prev) = _pending_save.take() {
                                        let _ = prev.await;
                                    }
                                    _pending_save = Some(tokio::spawn(async move {
                                        match tokio::time::timeout(
                                            BACKGROUND_TASK_TIMEOUT,
                                            fs_ref.save_conversation_with_exit(&conv_clone, true),
                                        ).await {
                                            Ok(Ok(())) => {}
                                            Ok(Err(e)) => tracing::error!("Failed to persist new session: {}", e),
                                            Err(_) => tracing::warn!("Background task 'new_session_save' timed out after {}s", BACKGROUND_TASK_TIMEOUT.as_secs()),
                                        }
                                    }));
                                }
                                InputAction::SubmitQuestionAnswer(answer) => {
                                    // Send answer back via oneshot channel
                                    if let Some(tx) = state.question_response_tx.take() {
                                        let _ = tx.send(answer);
                                    }
                                    state.focus = FocusState::Input;
                                    state.needs_redraw = true;
                                }
                                InputAction::ExecuteCommand { name: cmd_name, args: cmd_args } => {
                                    let cmd_arg: Option<&str> = cmd_args.as_deref();
                                    let cmd_name: &str = cmd_name.as_str();
                                    if cmd_name == "ml" {
                                        // /ml command: toggle multi-line mode (AC#3)
                                        // Covers: Sprint Change Proposal 2026-04-08
                                        state.multiline_mode = !state.multiline_mode;
                                        state.needs_redraw = true;
                                    } else if cmd_name == "export" {
                                        apply_export_command(
                                            cmd_arg,
                                            &conversation,
                                            &tab_manager.active_tab().session_meta.clone(),
                                            &workspace_path,
                                            &mut state,
                                        )
                                        .await;
                                    } else if cmd_name == "mode" {
                                        // /mode plan|normal|autoedit|yolo — AC9
                                        // Bare `/mode` shows the current mode instead of silently resetting.
                                        match cmd_arg.map(|s| s.trim().to_ascii_lowercase()) {
                                            None => {
                                                let current = match security.current_mode() {
                                                    PermissionMode::Plan => "Plan",
                                                    PermissionMode::Normal => "Normal",
                                                    PermissionMode::AutoEdit => "AutoEdit",
                                                    PermissionMode::Yolo => "YOLO",
                                                };
                                                if !matches!(state.status, StatusState::Flash { .. }) {
                                                    state.status_before_flash = Some(state.status.clone());
                                                }
                                                state.status = StatusState::Flash {
                                                    message: format!("Current mode: {} — use /mode <plan|normal|autoedit|yolo> to switch", current),
                                                    remaining_ms: state.theme.timing.status_flash_ms,
                                                };
                                                state.needs_redraw = true;
                                            }
                                            Some(arg) => {
                                                let mode = crate::domain::services::permission_chain::parse_mode_arg(
                                                    Some(arg.as_str()),
                                                );
                                                if let Some(m) = mode {
                                                    app_state.event_bus.emit_domain(AppEvent::SetPermissionMode(m));
                                                } else {
                                                    if !matches!(state.status, StatusState::Flash { .. }) {
                                                        state.status_before_flash = Some(state.status.clone());
                                                    }
                                                    state.status = StatusState::Flash {
                                                        message: format!("Unknown mode: {}. Use plan, normal, autoedit, or yolo", arg),
                                                        remaining_ms: state.theme.timing.status_flash_ms,
                                                    };
                                                    state.needs_redraw = true;
                                                }
                                            }
                                        }
                                    } else if cmd_name == "plan" {
                                        // /plan on|off|toggle — Story 6-0d AC5
                                        match cmd_arg.map(|s| s.trim().to_ascii_lowercase()) {
                                            None => {
                                                let current = match security.current_mode() {
                                                    PermissionMode::Plan => "Plan",
                                                    _ => "not Plan",
                                                };
                                                if !matches!(state.status, StatusState::Flash { .. }) {
                                                    state.status_before_flash = Some(state.status.clone());
                                                }
                                                state.status = StatusState::Flash {
                                                    message: format!("Plan mode is {} — use /plan on|off|toggle to switch", current),
                                                    remaining_ms: state.theme.timing.status_flash_ms,
                                                };
                                                state.needs_redraw = true;
                                            }
                                            Some(arg) => {
                                                let target = match arg.as_str() {
                                                    "on" => Some(PermissionMode::Plan),
                                                    "off" => Some(PermissionMode::Normal),
                                                    "toggle" => {
                                                        if security.current_mode() == PermissionMode::Plan {
                                                            Some(PermissionMode::Normal)
                                                        } else {
                                                            Some(PermissionMode::Plan)
                                                        }
                                                    }
                                                    _ => None,
                                                };
                                                if let Some(m) = target {
                                                    app_state.event_bus.emit_domain(AppEvent::SetPermissionMode(m));
                                                } else {
                                                    if !matches!(state.status, StatusState::Flash { .. }) {
                                                        state.status_before_flash = Some(state.status.clone());
                                                    }
                                                    state.status = StatusState::Flash {
                                                        message: format!("Unknown /plan argument: {}. Use on, off, or toggle", arg),
                                                        remaining_ms: state.theme.timing.status_flash_ms,
                                                    };
                                                    state.needs_redraw = true;
                                                }
                                            }
                                        }
                                    } else if cmd_name == "new" {
                                        // /new command: save current, create fresh session
                                        // AC7: save current conversation if it has messages
                                        if !conversation.messages.is_empty() {
                                            conversation.updated_at = crate::domain::models::session_meta::now_unix();
                                            if let Some(prev) = _pending_save.take() {
                                                let _ = prev.await;
                                            }
                                            let fs_ref = fs_storage.clone();
                                            let conv_clone = conversation.clone();
                                            _pending_save = Some(tokio::spawn(async move {
                                                match tokio::time::timeout(
                                                    BACKGROUND_TASK_TIMEOUT,
                                                    fs_ref.save_conversation_with_exit(&conv_clone, true),
                                                ).await {
                                                    Ok(Ok(())) => {}
                                                    Ok(Err(e)) => tracing::error!("Failed to save session before /new: {}", e),
                                                    Err(_) => tracing::warn!("Session save timed out before /new"),
                                                }
                                            }));
                                        }
                                        // Create fresh conversation
                                        conversation = Conversation {
                                            id: generate_conversation_id(),
                                            title: String::new(),
                                            messages: Vec::new(),
                                            turns: Vec::new(),
                                            created_at: crate::domain::models::session_meta::now_unix(),
                                            updated_at: crate::domain::models::session_meta::now_unix(),
                                            last_response_at: None,
                                            session_id: Some(generate_conversation_id()),
                                            usage: None,
                                            plans: std::collections::HashMap::new(),
                                            fork_source: None,
                                        };
                                        skill_activator.on_new_conversation(&conversation.id).await;
                                        agent_activator.on_new_conversation(&conversation.id).await;
                                        state.active_agent_name = None;
                                        // Reset TUI state
                                        state.input_buffer.clear();
                                        state.cursor_position = 0;
                                        state.input_scroll_offset = 0;
                                        state.scroll_snapshot = 0;
                                        state.auto_snapshot = true;
                                        state.total_content_height = 0;
                                        state.block_boundaries.clear();
                                        state.message_boundaries.clear();
                                        state.user_message_boundaries.clear();
                                        state.tab_render_state(state.active_tab_id).height_cache.invalidate_all();
                                        state.tab_render_state(state.active_tab_id).tool_block_states_version = 0;
                                        state.tool_block_states.clear();
                                        state.focused_tool_id = None;
                                        state.feedback_blocks.clear();
                                        state.active_feedback_id = None;
                                        state.autocomplete.dismiss();
                                        state.resolved_mentions.clear();
                                        state.token_usage = None;
                                        state.focus = FocusState::Input;
                                        state.status = StatusState::Idle;
                                        state.needs_redraw = true;
                                        // Reset session manager
                                        session_manager = SessionManager::new(SessionState::Active {
                                            id: conversation.session_id.clone().unwrap_or_default(),
                                        });
                                        // Reset streaming state
                                        streaming = StreamingState::default();
                                        while turn_queue.dequeue().is_some() {}
                                        if let Some(handle) = _active_turn.take() {
                                            handle.abort();
                                        }
                                    } else if cmd_name == "deactivate" {
                                        // /deactivate [name] — Story 5-2 AC5
                                        let conv_id = conversation.id.clone();
                                        if let Some(ref target) = cmd_arg {
                                            app_state.event_bus.emit_domain(AppEvent::AskActivateSkill {
                                                conversation_id: conv_id,
                                                name: format!("__deactivate__{}", target),
                                                arguments: String::new(),
                                            });
                                        } else {
                                            app_state.event_bus.emit_domain(AppEvent::AskActivateSkill {
                                                conversation_id: conv_id,
                                                name: "__deactivate_all__".to_string(),
                                                arguments: String::new(),
                                            });
                                        }
                                    } else {
                                        // Check if command matches a discovered skill name (Story 5-2 AC8)
                                        if state.skill_registry.read().await.find(cmd_name).is_some() {
                                            let conv_id = conversation.id.clone();
                                            let args_text = cmd_arg
                                                .map(|s| s.trim().to_string())
                                                .unwrap_or_default();
                                            app_state.event_bus.emit_domain(AppEvent::AskActivateSkill {
                                                conversation_id: conv_id,
                                                name: cmd_name.to_string(),
                                                arguments: args_text,
                                            });
                                        }
                                    }
                                }
                                InputAction::SubmitWithContext { text, command, command_args } => {
                                    // Build context-enriched message
                                    let mut context_prefix = String::new();

                                    // Resolve command context if present
                                    if let Some(ref cmd_name) = command {
                                        let (cmd_ctx, cmd_errors) = resolve_command_context(
                                            cmd_name,
                                            command_args.as_deref(),
                                            &workspace_path,
                                            security.as_ref(),
                                            &command_registry,
                                        );
                                        if let Some(ctx) = cmd_ctx {
                                            context_prefix.push_str(&message_builder::build_command_context_prefix(&ctx));
                                        }
                                        for err in &cmd_errors {
                                            let fb = FeedbackBlock {
                                                id: generate_conversation_id(),
                                                message: format!(
                                                    "Command '/{}' references missing file: {}",
                                                    cmd_name, err.raw_path
                                                ),
                                                level: FeedbackLevel::Warning,
                                                actions: vec![],
                                            };
                                            state.feedback_blocks.insert(fb.id.clone(), fb);
                                            state.needs_redraw = true;
                                        }
                                    }

                                    // Resolve file mentions
                                    let (file_contexts, mention_images, file_errors) = resolve_file_context(&state.resolved_mentions, &workspace_path, security.as_ref());
                                    if !file_contexts.is_empty() {
                                        context_prefix.push_str(&message_builder::build_file_context_prefix(&file_contexts));
                                    }
                                    // P9: Show FeedbackBlock for each file resolution error
                                    for err in &file_errors {
                                        let fb = FeedbackBlock {
                                            id: generate_conversation_id(),
                                            message: err.reason.clone(),
                                            level: FeedbackLevel::Error,
                                            actions: vec![],
                                        };
                                        state.feedback_blocks.insert(fb.id.clone(), fb);
                                        state.needs_redraw = true;
                                    }
                                    state.resolved_mentions.clear();

                                    let full_text = if context_prefix.is_empty() {
                                        text
                                    } else {
                                        format!("{}{}", context_prefix, text)
                                    };

                                    // Merge pending images with mention images
                                    // Covers: FR112 (AC1, AC2)
                                    let mut all_images: Vec<ImageAttachment> = state.pending_images.drain(..).collect();
                                    all_images.extend(mention_images);
                                    state.image_indicator = None;

                                    if streaming.is_streaming {
                                        let msg = UserMessage {
                                            content: full_text,
                                            images: all_images,
                                        };
                                        if turn_queue.enqueue(msg).is_err() {
                                            state.status_before_flash = Some(state.status.clone());
                                            state.status = StatusState::Flash {
                                                message: "Message queue full".to_string(),
                                                remaining_ms: state.theme.timing.status_flash_ms,
                                            };
                                            state.needs_redraw = true;
                                        }
                                    } else {
                                        let _skill_snap = skill_activator.snapshot_for_turn(&conversation.id).await;
                                        let _agent_snap = agent_activator.snapshot(&conversation.id).await;
                                        start_turn(
                                            &full_text,
                                            all_images,
                                            &mut conversation,
                                            &mut streaming,
                                            &mut state,
                                            &mut _active_turn,
                                            &provider,
                                            config,
                                            &domain_tx,
                                            &security,
                                            &tools, &tool_scheduler,
                                            &persona,
                                            &workspace_path,
                                            &mut session_manager,
                                            &fs_storage,
                                            &storage,
                                              &app_state.plan_manager, &app_state.plan_injector,
                                            None,
                                            _skill_snap,
                                            _agent_snap,
                                                                              tab_manager.reset_and_clone_turn_cancel(),
                                        app_state.usage_ledger.clone()).await;
                                        match render(terminal, &mut state, &conversation, &streaming, &config.model, &router.active_delegate_id(), security.as_ref(), tab_manager.tab_count(), tab_manager.active_tab_index(), Some(&tab_manager), &session_index) {
                                            Ok(()) => state.needs_redraw = false,
                                            Err(e) => handle_render_error(e, &mut _active_turn, &mut streaming, &mut state, terminal),
                                        }
                                    }
                                }
                                InputAction::CopyToClipboard(mut content) => {
                                    use crate::adapters::tui::clipboard;

                                    // Resolve content if empty (Chat focus copy)
                                    // Covers: FR116 (AC6, AC8, AC9)
                                    if content.is_empty() {
                                        content = resolve_copy_content(&state, &conversation);
                                    }
                                    if content.is_empty() {
                                        state.status_before_flash = Some(state.status.clone());
                                        state.status = StatusState::Flash {
                                            message: "Nothing to copy".to_string(),
                                            remaining_ms: state.theme.timing.status_flash_ms,
                                        };
                                        state.needs_redraw = true;
                                    } else {

                                    let result = clipboard::copy_to_clipboard(&content);
                                    let flash_msg = match result {
                                        clipboard::ClipboardResult::Osc52Success => {
                                            "Copied to clipboard".to_string()
                                        }
                                        clipboard::ClipboardResult::FallbackSuccess(ref path) => {
                                            format!("Copied to {}", path.display())
                                        }
                                        clipboard::ClipboardResult::Failed(ref err) => {
                                            format!("Copy failed: {}", err)
                                        }
                                    };
                                    state.status_before_flash = Some(state.status.clone());
                                    state.status = StatusState::Flash {
                                        message: flash_msg,
                                        remaining_ms: state.theme.timing.status_flash_ms,
                                    };
                                    state.needs_redraw = true;
                                    } // end else (content not empty)
                                }
                                InputAction::CopyTaskResult { plan_id, task_number } => {
                                    use crate::adapters::tui::clipboard;
                                    let content = plan_id.as_deref()
                                        .and_then(|id| conversation.plans.get(id))
                                        .and_then(|plan| plan.tasks.get((task_number - 1) as usize))
                                        .map(|task| {
                                            match task.status {
                                                PlanTaskStatus::Completed => {
                                                    task.result.as_ref().map(|r| r.text.clone()).unwrap_or_default()
                                                }
                                                PlanTaskStatus::Failed | PlanTaskStatus::Skipped | PlanTaskStatus::Cancelled => {
                                                    task.error.clone().unwrap_or_default()
                                                }
                                                _ => format!("Task {}: {} — {:?}", task.number, task.title, task.status),
                                            }
                                        })
                                        .unwrap_or_default();
                                    if content.is_empty() {
                                        state.status_before_flash = Some(state.status.clone());
                                        state.status = StatusState::Flash {
                                            message: "Nothing to copy".to_string(),
                                            remaining_ms: state.theme.timing.status_flash_ms,
                                        };
                                    } else {
                                        let result = clipboard::copy_to_clipboard(&content);
                                        let flash_msg = match result {
                                            clipboard::ClipboardResult::Osc52Success => {
                                                "Copied to clipboard".to_string()
                                            }
                                            clipboard::ClipboardResult::FallbackSuccess(ref path) => {
                                                format!("Copied to {}", path.display())
                                            }
                                            clipboard::ClipboardResult::Failed(ref err) => {
                                                format!("Copy failed: {}", err)
                                            }
                                        };
                                        state.status_before_flash = Some(state.status.clone());
                                        state.status = StatusState::Flash {
                                            message: flash_msg,
                                            remaining_ms: state.theme.timing.status_flash_ms,
                                        };
                                    }
                                    state.needs_redraw = true;
                                }
                                // ─── Story 6.4: Task Control & Plan Deviation ───

                                InputAction::TaskPause(n) => {
                                    let task_number = if state.task_panel_state.drill_down_task.is_some() {
                                        n
                                    } else {
                                        match crate::adapters::tui::task_panel_handlers::resolve_panel_task_number(&state, &conversation, n) {
                                            Some(tn) => tn,
                                            None => {
                                                app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                                    conversation_id: Some(conversation.id.clone()),
                                                    level: NoticeLevel::Info,
                                                    message: "No task selected.".to_string(),
                                                });
                                                continue;
                                            }
                                        }
                                    };
                                    let outcome = crate::adapters::tui::task_panel_handlers::handle_task_pause(
                                        &mut state,
                                        &mut conversation,
                                        task_number,
                                    );
                                    for notice in &outcome.notices {
                                        app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                            conversation_id: Some(notice.conversation_id.clone()),
                                            level: notice.level,
                                            message: notice.message.clone(),
                                        });
                                    }
                                    // Emit PlanTaskStatusChanged: scoped to release immutable borrow
                                    let plan_id = state.task_panel_state.last_executed_plan_id.clone().unwrap_or_default();
                                    let current_status = {
                                        conversation.plans.get(&plan_id)
                                            .and_then(|plan| plan.tasks.get((task_number.saturating_sub(1)) as usize))
                                            .map(|t| t.status)
                                    };
                                    if let Some(status) = current_status {
                                        app_state.event_bus.emit_domain(AppEvent::PlanTaskStatusChanged {
                                            conversation_id: conversation.id.clone(),
                                            plan_id: plan_id.clone(),
                                            task_number,
                                            status,
                                        });
                                    }
                                    let running_to_cancel = outcome.running_task_paused;
                                    let should_advance = outcome.should_resume_advance;
                                    drop(outcome);
                                    if let Some(running_n) = running_to_cancel {
                                        plan_runtime.mark_pause_pending(&plan_id, running_n).await;
                                        if let Some(snapshot) = plan_runtime.snapshot(&plan_id).await {
                                            if let Some(token) = snapshot.task_cancels.get(&running_n) {
                                                token.cancel();
                                            }
                                        }
                                    }
                                    if should_advance {
                                        let conv_id = conversation.id.clone();
                                        plan_runtime.resume_advance(
                                            &conv_id,
                                            &plan_id,
                                            &mut conversation,
                                            app_state.event_bus.as_ref(),
                                        ).await;
                                    }
                                    state.needs_redraw = true;
                                }

                                InputAction::TaskSkip(n) => {
                                    let task_number = if state.task_panel_state.drill_down_task.is_some() {
                                        n
                                    } else {
                                        match crate::adapters::tui::task_panel_handlers::resolve_panel_task_number(&state, &conversation, n) {
                                            Some(tn) => tn,
                                            None => {
                                                app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                                    conversation_id: Some(conversation.id.clone()),
                                                    level: NoticeLevel::Info,
                                                    message: "No task selected.".to_string(),
                                                });
                                                continue;
                                            }
                                        }
                                    };
                                    let notices = crate::adapters::tui::task_panel_handlers::handle_task_skip(
                                        &mut state,
                                        &mut conversation,
                                        task_number,
                                    );
                                    for notice in &notices {
                                        app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                            conversation_id: Some(notice.conversation_id.clone()),
                                            level: notice.level,
                                            message: notice.message.clone(),
                                        });
                                    }
                                    let plan_id = state.task_panel_state.last_executed_plan_id.clone().unwrap_or_default();
                                    let current_status = {
                                        conversation.plans.get(&plan_id)
                                            .and_then(|plan| plan.tasks.get((task_number.saturating_sub(1)) as usize))
                                            .map(|t| t.status)
                                    };
                                    if let Some(status) = current_status {
                                        app_state.event_bus.emit_domain(AppEvent::PlanTaskStatusChanged {
                                            conversation_id: conversation.id.clone(),
                                            plan_id: plan_id.clone(),
                                            task_number,
                                            status,
                                        });
                                    }
                                    let should_advance = state.task_panel_state.skip_cascade_pending.is_none();
                                    if should_advance {
                                        let conv_id = conversation.id.clone();
                                        plan_runtime.resume_advance(
                                            &conv_id,
                                            &plan_id,
                                            &mut conversation,
                                            app_state.event_bus.as_ref(),
                                        ).await;
                                    }
                                    state.needs_redraw = true;
                                }

                                InputAction::TaskRetry(n) => {
                                    let plan_id = match state.task_panel_state.last_executed_plan_id.as_ref() {
                                        Some(id) => id.clone(),
                                        None => {
                                            app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                                conversation_id: Some(conversation.id.clone()),
                                                level: NoticeLevel::Info,
                                                message: "No active plan.".to_string(),
                                            });
                                            continue;
                                        }
                                    };
                                    let should_retry = match conversation.plans.get_mut(&plan_id) {
                                        Some(plan) => {
                                            let idx = (n.saturating_sub(1)) as usize;
                                            if idx < plan.tasks.len() && plan.tasks[idx].number == n && plan.tasks[idx].status == PlanTaskStatus::Failed {
                                                plan.tasks[idx].status = PlanTaskStatus::Pending;
                                                plan.tasks[idx].started_at_ms = None;
                                                plan.tasks[idx].completed_at_ms = None;
                                                plan.tasks[idx].error = None;
                                                plan.tasks[idx].result = None;
                                                true
                                            } else {
                                                false
                                            }
                                        }
                                        None => false,
                                    };
                                    if should_retry {
                                        app_state.event_bus.emit_domain(AppEvent::PlanTaskStatusChanged {
                                            conversation_id: conversation.id.clone(),
                                            plan_id: plan_id.clone(),
                                            task_number: n,
                                            status: PlanTaskStatus::Pending,
                                        });
                                        app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                            conversation_id: Some(conversation.id.clone()),
                                            level: NoticeLevel::Info,
                                            message: format!("Retrying Task {}.", n),
                                        });
                                        let conv_id = conversation.id.clone();
                                        plan_runtime.resume_advance(
                                            &conv_id,
                                            &plan_id,
                                            &mut conversation,
                                            app_state.event_bus.as_ref(),
                                        ).await;
                                    }
                                    state.needs_redraw = true;
                                }

                                InputAction::TaskEdit(n) => {
                                    let plan_id = match state.task_panel_state.last_executed_plan_id.as_ref() {
                                        Some(id) => id.clone(),
                                        None => continue,
                                    };
                                    let has_failed_task = conversation.plans.get(&plan_id)
                                        .and_then(|p| p.tasks.get((n.saturating_sub(1)) as usize))
                                        .map(|t| t.number == n && t.status == PlanTaskStatus::Failed)
                                        .unwrap_or(false);
                                    if !has_failed_task {
                                        continue;
                                    }
                                    let (orig_title, orig_description) = {
                                        let plan = conversation.plans.get(&plan_id).unwrap();
                                        let task = &plan.tasks[(n.saturating_sub(1)) as usize];
                                        (task.title.clone(), task.description.clone())
                                    };
                                    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
                                    let edits_dir = home.join(".rustain").join("edits");
                                    let _ = std::fs::create_dir_all(&edits_dir);
                                    let path = edits_dir.join(format!("task-{}-{}.txt", plan_id, n));
                                    let safe_title = orig_title.replace('\n', " ");
                                    let template: String = format!(
                                        "# Edit task title and description below.\n\
                                         # Save and exit to apply; close without changes to cancel.\n\
                                         # Lines starting with # are ignored.\n\
                                         title: {}\n\
                                         description: |\n{}",
                                        safe_title,
                                        {
                                            let indented: Vec<String> = orig_description
                                                .lines()
                                                .map(|l| format!("  {}", l))
                                                .collect();
                                            indented.join("\n")
                                        }
                                    );
                                    let _ = std::fs::write(&path, &template);
                                    // editor_suspend handles terminal suspend/restore internally
                                    let exit_result = crate::adapters::tui::editor_suspend::run_editor_on_path(&path);
                                    match exit_result {
                                        Ok(_exit_status) => {
                                            match std::fs::read_to_string(&path) {
                                                Ok(content) => {
                                                    if content == template {
                                                        app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                                            conversation_id: Some(conversation.id.clone()),
                                                            level: NoticeLevel::Info,
                                                            message: "Task edit cancelled.".to_string(),
                                                        });
                                                    } else {
                                                        let mut new_title = String::new();
                                                        let mut new_desc_lines: Vec<String> = Vec::new();
                                                        let mut reading_desc = false;
                                                        for line in content.lines() {
                                                            // Only skip comment lines before the description block
                                                            if line.starts_with('#') && !reading_desc {
                                                                continue;
                                                            }
                                                            if line == "description: |" {
                                                                reading_desc = true;
                                                            } else if reading_desc {
                                                                if let Some(rest) = line.strip_prefix("  ") {
                                                                    new_desc_lines.push(rest.to_string());
                                                                } else if line.is_empty() {
                                                                    // preserve empty lines within description body
                                                                    new_desc_lines.push(String::new());
                                                                } else {
                                                                    reading_desc = false;
                                                                }
                                                            } else if let Some(rest) = line.strip_prefix("title: ") {
                                                                new_title = rest.to_string();
                                                            } else if !line.trim().is_empty() && new_title.is_empty() {
                                                                // fallback: first non-comment non-empty line is title
                                                                new_title = line.to_string();
                                                            }
                                                        }
                                                        if new_title.is_empty() {
                                                            app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                                                conversation_id: Some(conversation.id.clone()),
                                                                level: NoticeLevel::Warning,
                                                                message: "Failed to parse edited task — missing title. Task unchanged.".to_string(),
                                                            });
                                                        } else {
                                                            let new_desc = new_desc_lines.join("\n");
                                                            {
                                                                let plan = conversation.plans.get_mut(&plan_id).unwrap();
                                                                let idx = (n.saturating_sub(1)) as usize;
                                                                plan.tasks[idx].title = new_title;
                                                                plan.tasks[idx].description = new_desc;
                                                                plan.tasks[idx].status = PlanTaskStatus::Pending;
                                                                plan.tasks[idx].started_at_ms = None;
                                                                plan.tasks[idx].completed_at_ms = None;
                                                                plan.tasks[idx].error = None;
                                                                plan.tasks[idx].result = None;
                                                            }
                                                            app_state.event_bus.emit_domain(AppEvent::PlanTaskStatusChanged {
                                                                conversation_id: conversation.id.clone(),
                                                                plan_id: plan_id.clone(),
                                                                task_number: n,
                                                                status: PlanTaskStatus::Pending,
                                                            });
                                                            app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                                                conversation_id: Some(conversation.id.clone()),
                                                                level: NoticeLevel::Info,
                                                                message: format!("Task {} edited and queued for retry.", n),
                                                            });
                                                            state.task_panel_state.drill_down_task = None;
                                                            let conv_id = conversation.id.clone();
                                                            plan_runtime.resume_advance(
                                                                &conv_id,
                                                                &plan_id,
                                                                &mut conversation,
                                                                app_state.event_bus.as_ref(),
                                                            ).await;
                                                        }
                                                    }
                                                }
                                                Err(_) => {
                                                    app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                                        conversation_id: Some(conversation.id.clone()),
                                                        level: NoticeLevel::Warning,
                                                        message: "Failed to read edited file. Task unchanged.".to_string(),
                                                    });
                                                }
                                            }
                                        }
                                        Err(_e) => {
                                            app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                                conversation_id: Some(conversation.id.clone()),
                                                level: NoticeLevel::Info,
                                                message: "Task edit cancelled (editor exited with error).".to_string(),
                                            });
                                        }
                                    }
                                    state.needs_redraw = true;
                                }

                                InputAction::TaskCancelPlan => {
                                    let executing_plan = conversation.plans.values()
                                        .find(|p| p.status == PlanStatus::Executing);
                                    if let Some(plan) = executing_plan {
                                        state.task_panel_state.cancel_plan_confirm = Some(plan.id.clone());
                                    } else {
                                        app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                            conversation_id: Some(conversation.id.clone()),
                                            level: NoticeLevel::Info,
                                            message: "No active plan to cancel.".to_string(),
                                        });
                                    }
                                    state.needs_redraw = true;
                                }

                                InputAction::TaskResumeAll => {
                                    let plan_id = match state.task_panel_state.last_executed_plan_id.as_ref() {
                                        Some(id) => id.clone(),
                                        None => {
                                            app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                                conversation_id: Some(conversation.id.clone()),
                                                level: NoticeLevel::Info,
                                                message: "No paused tasks.".to_string(),
                                            });
                                            continue;
                                        }
                                    };
                                    let mut count = 0u32;
                                    {
                                        let plan = match conversation.plans.get_mut(&plan_id) {
                                            Some(p) => p,
                                            None => {
                                                app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                                    conversation_id: Some(conversation.id.clone()),
                                                    level: NoticeLevel::Info,
                                                    message: "No paused tasks.".to_string(),
                                                });
                                                continue;
                                            }
                                        };
                                        for task in &mut plan.tasks {
                                            if task.status == PlanTaskStatus::Paused {
                                                let was_running = task.started_at_ms.is_some()
                                                    && task.completed_at_ms.is_none();
                                                task.status = PlanTaskStatus::Pending;
                                                if was_running {
                                                    task.started_at_ms = None;
                                                }
                                                count += 1;
                                                app_state.event_bus.emit_domain(AppEvent::PlanTaskStatusChanged {
                                                    conversation_id: conversation.id.clone(),
                                                    plan_id: plan_id.clone(),
                                                    task_number: task.number,
                                                    status: PlanTaskStatus::Pending,
                                                });
                                            }
                                        }
                                    }
                                    if count == 0 {
                                        app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                            conversation_id: Some(conversation.id.clone()),
                                            level: NoticeLevel::Info,
                                            message: "No paused tasks.".to_string(),
                                        });
                                    } else {
                                        app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                            conversation_id: Some(conversation.id.clone()),
                                            level: NoticeLevel::Info,
                                            message: format!("Resumed {} task(s).", count),
                                        });
                                        let conv_id = conversation.id.clone();
                                        plan_runtime.resume_advance(
                                            &conv_id,
                                            &plan_id,
                                            &mut conversation,
                                            app_state.event_bus.as_ref(),
                                        ).await;
                                    }
                                    state.needs_redraw = true;
                                }

                                InputAction::TaskReorderEnter(n) => {
                                    let plan_id = match state.task_panel_state.last_executed_plan_id.as_ref() {
                                        Some(id) => id.clone(),
                                        None => {
                                            app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                                conversation_id: Some(conversation.id.clone()),
                                                level: NoticeLevel::Warning,
                                                message: "No active plan.".to_string(),
                                            });
                                            continue;
                                        }
                                    };
                                    if let Some(plan) = conversation.plans.get(&plan_id) {
                                        let idx = (n.saturating_sub(1)) as usize;
                                        if idx < plan.tasks.len() && plan.tasks[idx].status == PlanTaskStatus::Pending {
                                            let orig_order: Vec<u32> = plan.tasks.iter().map(|t| t.number).collect();
                                            state.task_panel_state.reorder_mode_for = Some(n);
                                            state.task_panel_state.reorder_original_order = Some(orig_order);
                                        } else {
                                            app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                                conversation_id: Some(conversation.id.clone()),
                                                level: NoticeLevel::Warning,
                                                message: "Reorder requires a pending task selected.".to_string(),
                                            });
                                        }
                                    }
                                    state.needs_redraw = true;
                                }

                                InputAction::TaskReorderMove(dir) => {
                                    let plan_id = match state.task_panel_state.last_executed_plan_id.as_ref() {
                                        Some(id) => id.clone(),
                                        None => continue,
                                    };
                                    let task_n = match state.task_panel_state.reorder_mode_for {
                                        Some(tn) => tn,
                                        None => continue,
                                    };
                                    if let Some(plan) = conversation.plans.get_mut(&plan_id) {
                                        let current_idx = plan.tasks.iter().position(|t| t.number == task_n);
                                        if let Some(i) = current_idx {
                                            let new_i = match dir {
                                                crate::adapters::tui::state::Direction::Up => {
                                                    if i > 0 { i - 1 } else { i }
                                                }
                                                crate::adapters::tui::state::Direction::Down => {
                                                    if i + 1 < plan.tasks.len() { i + 1 } else { i }
                                                }
                                                crate::adapters::tui::state::Direction::Left | crate::adapters::tui::state::Direction::Right => i,
                                            };
                                            if i == new_i {
                                                continue;
                                            }
                                            if let Err(reason) = crate::domain::services::plan_runtime::validate_reorder(plan, task_n, new_i) {
                                                app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                                    conversation_id: Some(conversation.id.clone()),
                                                    level: NoticeLevel::Warning,
                                                    message: format!("Reorder violates dependencies: {}.", reason),
                                                });
                                            } else {
                                                let task = plan.tasks.remove(i);
                                                plan.tasks.insert(new_i, task);
                                            }
                                        }
                                    }
                                    state.needs_redraw = true;
                                }

                                InputAction::TaskReorderCommit => {
                                    if let Some(plan_id) = state.task_panel_state.last_executed_plan_id.as_ref() {
                                        if let Some(plan) = conversation.plans.get(plan_id) {
                                            for task in &plan.tasks {
                                                app_state.event_bus.emit_domain(AppEvent::PlanTaskStatusChanged {
                                                    conversation_id: conversation.id.clone(),
                                                    plan_id: plan_id.clone(),
                                                    task_number: task.number,
                                                    status: PlanTaskStatus::Pending,
                                                });
                                            }
                                        }
                                    }
                                    state.task_panel_state.reorder_mode_for = None;
                                    state.task_panel_state.reorder_original_order = None;
                                    app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                        conversation_id: Some(conversation.id.clone()),
                                        level: NoticeLevel::Info,
                                        message: "Plan reordered.".to_string(),
                                    });
                                    state.needs_redraw = true;
                                }

                                InputAction::TaskReorderCancel => {
                                    if let (Some(plan_id), Some(orig)) = (
                                        state.task_panel_state.last_executed_plan_id.as_ref(),
                                        state.task_panel_state.reorder_original_order.take(),
                                    ) {
                                        if let Some(plan) = conversation.plans.get_mut(plan_id) {
                                            let mut restored = Vec::new();
                                            for num in &orig {
                                                if let Some(idx) = plan.tasks.iter().position(|t| &t.number == num) {
                                                    restored.push(plan.tasks.remove(idx));
                                                }
                                            }
                                            plan.tasks = restored;
                                        }
                                    }
                                    state.task_panel_state.reorder_mode_for = None;
                                    state.task_panel_state.reorder_original_order = None;
                                    state.needs_redraw = true;
                                }

                                InputAction::SkipCascadeAck(choice) => {
                                    let skip_pending = match state.task_panel_state.skip_cascade_pending.take() {
                                        Some(s) => s,
                                        None => continue,
                                    };
                                    let plan_id = skip_pending.plan_id.clone();
                                    let conv_id = conversation.id.clone();
                                    match choice {
                                        crate::adapters::tui::state::SkipCascadeChoice::CascadeSkip => {
                                            {
                                                let plan = match conversation.plans.get_mut(&plan_id) {
                                                    Some(p) => p,
                                                    None => continue,
                                                };
                                                let now_ms = chrono::Utc::now().timestamp_millis();
                                                for num in &skip_pending.downstream {
                                                    let idx = (num.saturating_sub(1)) as usize;
                                                    if idx < plan.tasks.len() {
                                                        plan.tasks[idx].status = PlanTaskStatus::Skipped;
                                                        plan.tasks[idx].completed_at_ms = Some(now_ms);
                                                        plan.tasks[idx].error = Some(format!(
                                                            "Skipped — upstream Task {} skipped",
                                                            skip_pending.source_task
                                                        ));
                                                        app_state.event_bus.emit_domain(AppEvent::PlanTaskStatusChanged {
                                                            conversation_id: conv_id.clone(),
                                                            plan_id: plan_id.clone(),
                                                            task_number: *num,
                                                            status: PlanTaskStatus::Skipped,
                                                        });
                                                    }
                                                }
                                            }
                                            plan_runtime.resume_advance(
                                                &conv_id,
                                                &plan_id,
                                                &mut conversation,
                                                app_state.event_bus.as_ref(),
                                            ).await;
                                        }
                                        crate::adapters::tui::state::SkipCascadeChoice::ContinueAnyway => {
                                            plan_runtime.resume_advance(
                                                &conv_id,
                                                &plan_id,
                                                &mut conversation,
                                                app_state.event_bus.as_ref(),
                                            ).await;
                                        }
                                        crate::adapters::tui::state::SkipCascadeChoice::CancelSkip => {
                                            {
                                                let plan = match conversation.plans.get_mut(&plan_id) {
                                                    Some(p) => p,
                                                    None => continue,
                                                };
                                                let idx = (skip_pending.source_task.saturating_sub(1)) as usize;
                                                if idx < plan.tasks.len() && plan.tasks[idx].number == skip_pending.source_task {
                                                    plan.tasks[idx].status = skip_pending.source_prior_status;
                                                    plan.tasks[idx].error = skip_pending.source_prior_error.clone();
                                                    plan.tasks[idx].completed_at_ms = None;
                                                }
                                            }
                                            if let Some(plan) = conversation.plans.get(&plan_id) {
                                                let idx = (skip_pending.source_task.saturating_sub(1)) as usize;
                                                if idx < plan.tasks.len() && plan.tasks[idx].number == skip_pending.source_task {
                                                    app_state.event_bus.emit_domain(AppEvent::PlanTaskStatusChanged {
                                                        conversation_id: conv_id.clone(),
                                                        plan_id: plan_id.clone(),
                                                        task_number: skip_pending.source_task,
                                                        status: plan.tasks[idx].status,
                                                    });
                                                }
                                            }
                                        }
                                    }
                                    state.needs_redraw = true;
                                }

                                InputAction::PlanDeviationDecided(plan_id, decision) => {
                                    use crate::domain::models::plan::PlanDecision;
                                    let conv_id = conversation.id.clone();
                                    match decision {
                                        PlanDecision::Approve => {
                                            plan_runtime.clear_deviation_pending(&plan_id).await;
                                            state.task_panel_state.pending_deviation = None;
                                            plan_runtime.resume_advance(
                                                &conv_id,
                                                &plan_id,
                                                &mut conversation,
                                                app_state.event_bus.as_ref(),
                                            ).await;
                                        }
                                        PlanDecision::Edit => {
                                            app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                                conversation_id: Some(conv_id.clone()),
                                                level: NoticeLevel::Warning,
                                                message: "Nothing to edit on auto-skip deviation. Use [n] to reject and stop.".to_string(),
                                            });
                                        }
                                        PlanDecision::Reject => {
                                            plan_runtime.clear_deviation_pending(&plan_id).await;
                                            state.task_panel_state.pending_deviation = None;
                                            {
                                                let plan = match conversation.plans.get_mut(&plan_id) {
                                                    Some(p) => p,
                                                    None => continue,
                                                };
                                                let now_ms = chrono::Utc::now().timestamp_millis();
                                                for task in &mut plan.tasks {
                                                    if !crate::domain::services::plan_runtime::is_terminal_pub(task.status) {
                                                        task.status = PlanTaskStatus::Cancelled;
                                                        task.completed_at_ms = Some(now_ms);
                                                        task.error = Some("Plan cancelled by user".to_string());
                                                        app_state.event_bus.emit_domain(AppEvent::PlanTaskStatusChanged {
                                                            conversation_id: conv_id.clone(),
                                                            plan_id: plan_id.clone(),
                                                            task_number: task.number,
                                                            status: PlanTaskStatus::Cancelled,
                                                        });
                                                    }
                                                }
                                                plan.status = PlanStatus::Cancelled;
                                            }
                                            app_state.event_bus.emit_domain(AppEvent::PlanCancelled {
                                                conversation_id: conv_id.clone(),
                                                plan_id: plan_id.clone(),
                                                cancelled_at_task: None,
                                            });
                                            crate::domain::services::plan_runtime::finish_plan(
                                                &conv_id,
                                                &plan_id,
                                                &mut conversation,
                                                app_state.event_bus.as_ref(),
                                            );
                                        }
                                        _ => {}
                                    }
                                    state.needs_redraw = true;
                                }

                                InputAction::CancelPlanConfirm(confirmed) => {
                                    if confirmed {
                                        let plan_id = match state.task_panel_state.cancel_plan_confirm.take() {
                                            Some(id) => id,
                                            None => continue,
                                        };
                                        // Clear any overlay cards before cancelling
                                        state.task_panel_state.pending_deviation = None;
                                        state.task_panel_state.skip_cascade_pending = None;
                                        let conv_id = conversation.id.clone();
                                        let has_running = conversation.plans.get(&plan_id)
                                            .map(|p| p.tasks.iter().any(|t| t.status == PlanTaskStatus::Running))
                                            .unwrap_or(false);
                                        if has_running {
                                            plan_runtime.mark_whole_plan_cancel_pending(&plan_id).await;
                                            if let Some(snapshot) = plan_runtime.snapshot(&plan_id).await {
                                                for (task_n, token) in &snapshot.task_cancels {
                                                    let is_running = conversation.plans.get(&plan_id)
                                                        .and_then(|p| p.tasks.get((task_n.saturating_sub(1)) as usize))
                                                        .map(|t| t.status == PlanTaskStatus::Running)
                                                        .unwrap_or(false);
                                                    if is_running {
                                                        token.cancel();
                                                        break;
                                                    }
                                                }
                                            }
                                            app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                                conversation_id: Some(conv_id.clone()),
                                                level: NoticeLevel::Info,
                                                message: "Cancelling plan...".to_string(),
                                            });
                                        } else {
                                            let plan = match conversation.plans.get_mut(&plan_id) {
                                                Some(p) => p,
                                                None => continue,
                                            };
                                            // Guard against plan that became terminal while confirm card was visible
                                            if plan.status != PlanStatus::Executing {
                                                app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                                    conversation_id: Some(conv_id.clone()),
                                                    level: NoticeLevel::Info,
                                                    message: "Plan already finished.".to_string(),
                                                });
                                                continue;
                                            }
                                            let now_ms = chrono::Utc::now().timestamp_millis();
                                            for task in &mut plan.tasks {
                                                if !crate::domain::services::plan_runtime::is_terminal_pub(task.status) {
                                                    task.status = PlanTaskStatus::Cancelled;
                                                    task.completed_at_ms = Some(now_ms);
                                                    task.error = Some("Plan cancelled by user".to_string());
                                                    app_state.event_bus.emit_domain(AppEvent::PlanTaskStatusChanged {
                                                        conversation_id: conv_id.clone(),
                                                        plan_id: plan_id.clone(),
                                                        task_number: task.number,
                                                        status: PlanTaskStatus::Cancelled,
                                                    });
                                                }
                                            }
                                            plan.status = PlanStatus::Cancelled;
                                            let _ = plan;
                                            app_state.event_bus.emit_domain(AppEvent::PlanCancelled {
                                                conversation_id: conv_id.clone(),
                                                plan_id: plan_id.clone(),
                                                cancelled_at_task: None,
                                            });
                                            crate::domain::services::plan_runtime::finish_plan(
                                                &conv_id,
                                                &plan_id,
                                                &mut conversation,
                                                app_state.event_bus.as_ref(),
                                            );
                                            plan_runtime.remove_plan(&plan_id).await;
                                        }
                                    } else {
                                        state.task_panel_state.cancel_plan_confirm = None;
                                    }
                                    state.needs_redraw = true;
                                }
                                // ─── End Story 6.4 ───
                                InputAction::ImageFormatError => {
                                    let fb = FeedbackBlock {
                                        id: generate_conversation_id(),
                                        message: "Unsupported image format. Supported: PNG, JPEG, GIF, WebP".to_string(),
                                        level: FeedbackLevel::Error,
                                        actions: vec![],
                                    };
                                    state.feedback_blocks.insert(fb.id.clone(), fb);
                                    state.needs_redraw = true;
                                }
                                InputAction::ImageSizeWarning { media_type, data, warning } => {
                                    // AC4: Show confirm/cancel FeedbackBlock for large images.
                                    // Store the image pending user confirmation.
                                    let attachment = ImageAttachment {
                                        media_type,
                                        data,
                                    };
                                    state.pending_large_image = Some(attachment);
                                    let fb = FeedbackBlock {
                                        id: generate_conversation_id(),
                                        message: warning,
                                        level: FeedbackLevel::Warning,
                                        actions: vec![
                                            FeedbackAction::Custom("[y] Attach anyway".to_string()),
                                            FeedbackAction::Custom("[n] Cancel".to_string()),
                                        ],
                                    };
                                    state.active_feedback_id = Some(fb.id.clone());
                                    state.feedback_blocks.insert(fb.id.clone(), fb);
                                    state.focus = FocusState::Chat;
                                    state.needs_redraw = true;
                                }
                                InputAction::ImageConfirmAttach => {
                                    // AC4: User confirmed attaching the large image
                                    if let Some(attachment) = state.pending_large_image.take() {
                                        use crate::adapters::tui::image;
                                        state.pending_images.push(attachment);
                                        state.image_indicator = Some(image::format_image_indicator(
                                            state.pending_images.len(),
                                            state.pending_images.iter().fold(0usize, |acc, i| acc.saturating_add(i.data.len() / 1024)),
                                        ));
                                    }
                                    // Clear the feedback block
                                    if let Some(fb_id) = state.active_feedback_id.take() {
                                        state.feedback_blocks.remove(&fb_id);
                                    }
                                    state.focus = FocusState::Input;
                                    state.needs_redraw = true;
                                }
                                InputAction::ImageConfirmCancel => {
                                    // AC4: User cancelled the large image attachment
                                    state.pending_large_image = None;
                                    // Clear the feedback block
                                    if let Some(fb_id) = state.active_feedback_id.take() {
                                        state.feedback_blocks.remove(&fb_id);
                                    }
                                    state.status_before_flash = Some(state.status.clone());
                                    state.status = StatusState::Flash {
                                        message: "Image attachment cancelled".to_string(),
                                        remaining_ms: state.theme.timing.status_flash_ms,
                                    };
                                    state.focus = FocusState::Input;
                                    state.needs_redraw = true;
                                }
                                InputAction::RequestClipboardPaste => {
                                    // Async: read image (or text) from the OS clipboard via
                                    // ClipboardPort, then re-enter handle_input so all existing
                                    // image validation / state mutation runs exactly once.
                                    use crate::domain::events::DomainInputEvent;
                                    match clipboard.read_image_png().await {
                                        Ok(Some(png_bytes)) => {
                                            let inner_action = handle_input(
                                                &mut state,
                                                &DomainInputEvent::ImagePaste(png_bytes),
                                            );
                                            // Propagate image-validation outcomes inline
                                            match inner_action {
                                                InputAction::ImageFormatError => {
                                                    let fb = FeedbackBlock {
                                                        id: generate_conversation_id(),
                                                        message: "Unsupported image format. Supported: PNG, JPEG, GIF, WebP".to_string(),
                                                        level: FeedbackLevel::Error,
                                                        actions: vec![],
                                                    };
                                                    state.feedback_blocks.insert(fb.id.clone(), fb);
                                                    state.needs_redraw = true;
                                                }
                                                InputAction::ImageSizeWarning { media_type, data, warning } => {
                                                    let attachment = ImageAttachment { media_type, data };
                                                    state.pending_large_image = Some(attachment);
                                                    let fb = FeedbackBlock {
                                                        id: generate_conversation_id(),
                                                        message: warning,
                                                        level: FeedbackLevel::Warning,
                                                        actions: vec![
                                                            FeedbackAction::Custom("[y] Attach anyway".to_string()),
                                                            FeedbackAction::Custom("[n] Cancel".to_string()),
                                                        ],
                                                    };
                                                    state.active_feedback_id = Some(fb.id.clone());
                                                    state.feedback_blocks.insert(fb.id.clone(), fb);
                                                    state.focus = FocusState::Chat;
                                                    state.needs_redraw = true;
                                                }
                                                _ => {} // Consumed or other — already mutated state
                                            }
                                        }
                                        Ok(None) => {
                                            // No image: try text
                                            match clipboard.read_text().await {
                                                Ok(Some(text)) => {
                                                    handle_input(
                                                        &mut state,
                                                        &DomainInputEvent::Paste(text),
                                                    );
                                                }
                                                Ok(None) => {
                                                    state.status_before_flash = Some(state.status.clone());
                                                    state.status = StatusState::Flash {
                                                        message: "Clipboard is empty".to_string(),
                                                        remaining_ms: state.theme.timing.status_flash_ms,
                                                    };
                                                    state.needs_redraw = true;
                                                }
                                                Err(e) => {
                                                    let fb = FeedbackBlock {
                                                        id: generate_conversation_id(),
                                                        message: format!("Could not read clipboard: {e}"),
                                                        level: FeedbackLevel::Error,
                                                        actions: vec![],
                                                    };
                                                    state.feedback_blocks.insert(fb.id.clone(), fb);
                                                    state.needs_redraw = true;
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            let fb = FeedbackBlock {
                                                id: generate_conversation_id(),
                                                message: format!("Could not read clipboard: {e}"),
                                                level: FeedbackLevel::Error,
                                                actions: vec![],
                                            };
                                            state.feedback_blocks.insert(fb.id.clone(), fb);
                                            state.needs_redraw = true;
                                        }
                                    }
                                }
                                InputAction::SkillSelected { name, arguments } => {
                                    let conv_id = conversation.id.clone();
                                    app_state.event_bus.emit_domain(AppEvent::AskActivateSkill {
                                        conversation_id: conv_id,
                                        name,
                                        arguments,
                                    });
                                }
                                InputAction::NewTab => {
                                    // Abort any active streaming on this tab before saving,
                                    // so the saved state reflects the abort (is_streaming = false,
                                    // partial response finalized). Without this, Tab 1 is saved
                                    // with is_streaming = true but no task producing chunks → stuck.
                                    if streaming.is_streaming {
                                        // Finalize active tool calls with [aborted]
                                        for (_, tc) in streaming.active_tool_calls.iter_mut() {
                                            if tc.result.is_none() {
                                                tc.result = Some(crate::domain::models::ToolResultInfo {
                                                    content: "[aborted]".to_string(),
                                                    is_error: true,
                                                });
                                                tc.completed_at_ms = Some(crate::domain::models::session_meta::now_unix() as u64 * 1000);
                                            }
                                        }
                                        // Preserve partial response as a finalized message
                                        if !streaming.current_text_buffer.is_empty()
                                            || !streaming.active_tool_calls.is_empty()
                                        {
                                            let content = std::mem::take(&mut streaming.current_text_buffer);
                                            conversation.messages.push(ChatMessage {
                                                id: generate_conversation_id(),
                                                role: MessageRole::Assistant,
                                                content,
                                                content_blocks: std::mem::take(&mut streaming.current_blocks),
                                                tool_calls: streaming.active_tool_calls.drain().map(|(_, v)| v).collect(),
                                                created_at: crate::domain::models::session_meta::now_unix(),
                                                token_count: None,
                                                stop_reason: Some(crate::domain::models::StopReason::Cancelled),
                                                synthetic: false,
                                                images: vec![],
                                            });
                                        }
                                        // Abort the streaming task
                                        if let Some(handle) = _active_turn.take() {
                                            handle.abort();
                                        }
                                        // Reset streaming state
                                        streaming.is_streaming = false;
                                        streaming.phase = crate::domain::models::StreamingPhase::Idle;
                                        streaming.current_blocks.clear();
                                        streaming.active_tool_calls.clear();
                                        while turn_queue.dequeue().is_some() {}
                                    }
                                    // Save current tab state back to TabManager (after abort,
                                    // so stored state has is_streaming = false)
                                    save_active_tab(&mut tab_manager, &conversation, &streaming, &session_manager, &state, &turn_queue);
                                    // Persist current conversation if it has messages
                                    if !conversation.messages.is_empty() {
                                        conversation.updated_at = crate::domain::models::session_meta::now_unix();
                                        let fs_ref = fs_storage.clone();
                                        let conv_clone = conversation.clone();
                                        if let Some(prev) = _pending_save.take() {
                                            let _ = prev.await;
                                        }
                                        _pending_save = Some(tokio::spawn(async move {
                                            match tokio::time::timeout(
                                                BACKGROUND_TASK_TIMEOUT,
                                                fs_ref.save_conversation_with_exit(&conv_clone, true),
                                            ).await {
                                                Ok(Ok(())) => {}
                                                Ok(Err(e)) => tracing::error!("Failed to save before new tab: {}", e),
                                                Err(_) => tracing::warn!("Tab save timed out"),
                                            }
                                        }));
                                    }
                                    // Create new tab in TabManager (switches active to the new tab)
                                    tab_manager.create_tab();
                                    // Load new tab state into proxies (fresh tab, never has queued messages)
                                    let _ = load_active_tab(&tab_manager, &mut conversation, &mut streaming, &mut session_manager, &mut state, &mut turn_queue);
                                    state.active_agent_name = None;
                                    // Update sidebar: mark new conversation open/active
                                    session_index.set_open(&conversation.id, true);
                                    session_index.set_active(Some(&conversation.id));
                                    state.sidebar_entry_count = session_index.len();
                                    state.input_buffer.clear();
                                    state.cursor_position = 0;
                                    state.input_scroll_offset = 0;
                                    state.autocomplete.dismiss();
                                    state.resolved_mentions.clear();
                                    state.focus = FocusState::Input;
                                    state.status = StatusState::Idle;
                                    state.needs_redraw = true;
                                }
                                InputAction::CloseTab => {
                                    if tab_manager.tab_count() == 1 {
                                        state.status_before_flash = Some(state.status.clone());
                                        state.status = StatusState::Flash {
                                            message: "Only one tab open".to_string(),
                                            remaining_ms: state.theme.timing.status_flash_ms,
                                        };
                                        state.needs_redraw = true;
                                    } else {
                                        // Save current tab state
                                        save_active_tab(&mut tab_manager, &conversation, &streaming, &session_manager, &state, &turn_queue);
                                        let closing_conv_id = conversation.id.clone();
                                        let tab_id = tab_manager.active_tab_id();
                                        // Abort active streaming on this tab
                                        if streaming.is_streaming {
                                            if let Some(handle) = _active_turn.take() {
                                                handle.abort();
                                            }
                                            while turn_queue.dequeue().is_some() {}
                                        }
                                        // Persist conversation if it has messages
                                        if !conversation.messages.is_empty() {
                                            conversation.updated_at = crate::domain::models::session_meta::now_unix();
                                            let fs_ref = fs_storage.clone();
                                            let conv_clone = conversation.clone();
                                            if let Some(prev) = _pending_save.take() {
                                                let _ = prev.await;
                                            }
                                            _pending_save = Some(tokio::spawn(async move {
                                                match tokio::time::timeout(
                                                    BACKGROUND_TASK_TIMEOUT,
                                                    fs_ref.save_conversation_with_exit(&conv_clone, true),
                                                ).await {
                                                    Ok(Ok(())) => {}
                                                    Ok(Err(e)) => tracing::error!("Failed to save before close tab: {}", e),
                                                    Err(_) => tracing::warn!("Close-tab save timed out"),
                                                }
                                            }));
                                        }
                                        // Close the tab (TabManager adjusts active index)
                                        tab_manager.close_tab(tab_id);
                                        state.tab_render_states.remove(&tab_id);
                                        agent_activator.on_tab_closed(&closing_conv_id).await;
                                        // Load the new active tab into proxies
                                        let should_drain = load_active_tab(&tab_manager, &mut conversation, &mut streaming, &mut session_manager, &mut state, &mut turn_queue);
                                        state.active_agent_name = agent_activator.active_agent_name(&conversation.id).await;
                                        if should_drain {
                                            if let Some(queued_msg) = turn_queue.dequeue() {
                                                { let _snap = skill_activator.snapshot_for_turn(&conversation.id).await; let _agent_snap = agent_activator.snapshot(&conversation.id).await; start_turn(&queued_msg.content, queued_msg.images, &mut conversation, &mut streaming, &mut state, &mut _active_turn, &provider, config, &domain_tx, &security, &tools, &tool_scheduler, &persona, &workspace_path, &mut session_manager, &fs_storage, &storage,
                                              &app_state.plan_manager, &app_state.plan_injector, None, _snap, _agent_snap, tab_manager.reset_and_clone_turn_cancel(), app_state.usage_ledger.clone()).await; }
                                            }
                                        }
                                        // Update sidebar: mark closed tab as no longer open
                                        session_index.set_open(&closing_conv_id, false);
                                        session_index.set_active(Some(&conversation.id));
                                        state.input_buffer.clear();
                                        state.cursor_position = 0;
                                        state.input_scroll_offset = 0;
                                        state.autocomplete.dismiss();
                                        state.resolved_mentions.clear();
                                        state.focus = FocusState::Input;
                                        state.needs_redraw = true;
                                    }
                                }
                                InputAction::SwitchToNextTab => {
                                    if tab_manager.tab_count() > 1 {
                                        save_active_tab(&mut tab_manager, &conversation, &streaming, &session_manager, &state, &turn_queue);
                                        tab_manager.switch_to_next();
                                        let should_drain = load_active_tab(&tab_manager, &mut conversation, &mut streaming, &mut session_manager, &mut state, &mut turn_queue);
                                        state.active_agent_name = agent_activator.active_agent_name(&conversation.id).await;
                                        if should_drain {
                                            if let Some(queued_msg) = turn_queue.dequeue() {
                                                { let _snap = skill_activator.snapshot_for_turn(&conversation.id).await; let _agent_snap = agent_activator.snapshot(&conversation.id).await; start_turn(&queued_msg.content, queued_msg.images, &mut conversation, &mut streaming, &mut state, &mut _active_turn, &provider, config, &domain_tx, &security, &tools, &tool_scheduler, &persona, &workspace_path, &mut session_manager, &fs_storage, &storage,
                                              &app_state.plan_manager, &app_state.plan_injector, None, _snap, _agent_snap, tab_manager.reset_and_clone_turn_cancel(), app_state.usage_ledger.clone()).await; }
                                            }
                                        }
                                        session_index.set_active(Some(&conversation.id));
                                        state.needs_redraw = true;
                                    }
                                }
                                InputAction::SwitchToPrevTab => {
                                    if tab_manager.tab_count() > 1 {
                                        save_active_tab(&mut tab_manager, &conversation, &streaming, &session_manager, &state, &turn_queue);
                                        tab_manager.switch_to_prev();
                                        let should_drain = load_active_tab(&tab_manager, &mut conversation, &mut streaming, &mut session_manager, &mut state, &mut turn_queue);
                                        state.active_agent_name = agent_activator.active_agent_name(&conversation.id).await;
                                        if should_drain {
                                            if let Some(queued_msg) = turn_queue.dequeue() {
                                                { let _snap = skill_activator.snapshot_for_turn(&conversation.id).await; let _agent_snap = agent_activator.snapshot(&conversation.id).await; start_turn(&queued_msg.content, queued_msg.images, &mut conversation, &mut streaming, &mut state, &mut _active_turn, &provider, config, &domain_tx, &security, &tools, &tool_scheduler, &persona, &workspace_path, &mut session_manager, &fs_storage, &storage,
                                              &app_state.plan_manager, &app_state.plan_injector, None, _snap, _agent_snap, tab_manager.reset_and_clone_turn_cancel(), app_state.usage_ledger.clone()).await; }
                                            }
                                        }
                                        session_index.set_active(Some(&conversation.id));
                                        state.needs_redraw = true;
                                    }
                                }
                                InputAction::CycleMode => {
                                    let next = cycle_mode(security.current_mode());
                                    app_state.event_bus.emit_domain(AppEvent::SetPermissionMode(next));
                                }
                                InputAction::SwitchToTab(n) => {
                                    if tab_manager.tab_count() > 1
                                        && n >= 1
                                        && n <= tab_manager.tab_count()
                                        && n - 1 != tab_manager.active_tab_index()
                                    {
                                        save_active_tab(&mut tab_manager, &conversation, &streaming, &session_manager, &state, &turn_queue);
                                        tab_manager.switch_to_index(n);
                                        let should_drain = load_active_tab(&tab_manager, &mut conversation, &mut streaming, &mut session_manager, &mut state, &mut turn_queue);
                                        state.active_agent_name = agent_activator.active_agent_name(&conversation.id).await;
                                        if should_drain {
                                            if let Some(queued_msg) = turn_queue.dequeue() {
                                                { let _snap = skill_activator.snapshot_for_turn(&conversation.id).await; let _agent_snap = agent_activator.snapshot(&conversation.id).await; start_turn(&queued_msg.content, queued_msg.images, &mut conversation, &mut streaming, &mut state, &mut _active_turn, &provider, config, &domain_tx, &security, &tools, &tool_scheduler, &persona, &workspace_path, &mut session_manager, &fs_storage, &storage,
                                              &app_state.plan_manager, &app_state.plan_injector, None, _snap, _agent_snap, tab_manager.reset_and_clone_turn_cancel(), app_state.usage_ledger.clone()).await; }
                                            }
                                        }
                                        session_index.set_active(Some(&conversation.id));
                                        state.needs_redraw = true;
                                    }
                                }
                                InputAction::ToggleSidebar => {
                                    // P10: Don't toggle sidebar when an overlay is active
                                    if matches!(state.focus, FocusState::Overlay(_)) {
                                        // Ignore — overlay has focus priority
                                    } else if state.terminal_width >= layout::SIDEBAR_MIN_WIDTH {
                                        state.sidebar_visible = !state.sidebar_visible;
                                        if state.sidebar_visible {
                                            state.sidebar_panel = Some(crate::domain::models::visual::PanelType::History);
                                            state.focus = FocusState::Sidebar {
                                                panel: crate::domain::models::visual::PanelType::History,
                                                selected: state.sidebar_selected
                                            };
                                        } else {
                                            state.sidebar_panel = None;
                                            state.focus = FocusState::Chat;
                                        }
                                        state.needs_redraw = true;
                                    } else {
                                        // Terminal too narrow - show info message
                                        state.status_before_flash = Some(state.status.clone());
                                        state.status = crate::domain::models::StatusState::Flash {
                                            message: "Terminal too narrow for sidebar. Use Ctrl+P to search history.".to_string(),
                                            remaining_ms: state.theme.timing.status_flash_ms,
                                        };
                                        state.needs_redraw = true;
                                    }
                                }
                                InputAction::OpenPanel(panel_type) => {
                                    use crate::domain::models::visual::PanelType;
                                    if panel_type == PanelType::Tasks {
                                        let term_width = state.terminal_width;
                                        let outcome = crate::adapters::tui::task_panel_handlers::handle_open_panel_tasks(
                                            &mut state,
                                            &conversation,
                                            term_width,
                                            layout::SIDEBAR_MIN_WIDTH,
                                        );
                                        if outcome.opened {
                                            state.focus = FocusState::Sidebar {
                                                panel: panel_type,
                                                selected: state.sidebar_selected,
                                            };
                                        } else if outcome.closed {
                                            state.focus = FocusState::Chat;
                                        }
                                        if let Some(notice) = outcome.notice {
                                            app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                                conversation_id: Some(notice.conversation_id),
                                                level: notice.level,
                                                message: notice.message,
                                            });
                                        }
                                    } else if state.sidebar_visible && state.sidebar_panel == Some(panel_type) {
                                        state.sidebar_visible = false;
                                        state.sidebar_panel = None;
                                        state.focus = FocusState::Chat;
                                        state.needs_redraw = true;
                                    } else if state.terminal_width >= layout::SIDEBAR_MIN_WIDTH {
                                        state.sidebar_visible = true;
                                        state.sidebar_panel = Some(panel_type);
                                        state.focus = FocusState::Sidebar {
                                            panel: panel_type,
                                            selected: state.sidebar_selected,
                                        };
                                        state.needs_redraw = true;
                                    } else {
                                        app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                            conversation_id: Some(conversation.id.clone()),
                                            level: NoticeLevel::Warning,
                                            message: "Panel requires terminal width >= 120 cols.".to_string(),
                                        });
                                    }
                                }
                                InputAction::OpenSidebarConversation => {
                                    // Resolve conversation ID from sidebar selection
                                    let resolved_id = session_index.entries()
                                        .get(state.sidebar_selected)
                                        .map(|e| e.conversation_id.clone());
                                    if let Some(conv_id) = resolved_id {
                                        // Check if already open in a tab
                                        if conv_id == conversation.id {
                                            // Already active — just switch focus and close sidebar
                                            state.sidebar_visible = false;
                                            state.sidebar_panel = None;
                                            state.focus = FocusState::Chat;
                                            state.needs_redraw = true;
                                        } else if tab_manager.find_by_conversation(&conv_id).is_some() {
                                            // Open in another tab — switch to it
                                            save_active_tab(&mut tab_manager, &conversation, &streaming, &session_manager, &state, &turn_queue);
                                            if let Some(idx) = tab_manager.tabs().iter().position(|t| t.conversation.id == conv_id) {
                                                tab_manager.switch_to_index(idx + 1); // 1-based
                                                let should_drain = load_active_tab(&tab_manager, &mut conversation, &mut streaming, &mut session_manager, &mut state, &mut turn_queue);
                                        state.active_agent_name = agent_activator.active_agent_name(&conversation.id).await;
                                                if should_drain {
                                                    if let Some(queued_msg) = turn_queue.dequeue() {
                                                        { let _snap = skill_activator.snapshot_for_turn(&conversation.id).await; let _agent_snap = agent_activator.snapshot(&conversation.id).await; start_turn(&queued_msg.content, queued_msg.images, &mut conversation, &mut streaming, &mut state, &mut _active_turn, &provider, config, &domain_tx, &security, &tools, &tool_scheduler, &persona, &workspace_path, &mut session_manager, &fs_storage, &storage,
                                              &app_state.plan_manager, &app_state.plan_injector, None, _snap, _agent_snap, tab_manager.reset_and_clone_turn_cancel(), app_state.usage_ledger.clone()).await; }
                                                    }
                                                }
                                                session_index.set_active(Some(&conv_id));
                                            }
                                            state.sidebar_visible = false;
                                            state.sidebar_panel = None;
                                            state.focus = FocusState::Chat;
                                            state.needs_redraw = true;
                                        } else {
                                            // Not open — load from storage into a new tab
                                            match storage.load_conversation(&conv_id).await {
                                                Ok(Some(loaded_conv)) => {
                                                    // Save current tab state first
                                                    save_active_tab(&mut tab_manager, &conversation, &streaming, &session_manager, &state, &turn_queue);
                                                    // Create new tab and load the conversation into it
                                    tab_manager.create_tab();
                                    skill_activator.on_new_conversation(&tab_manager.active_tab().conversation.id).await;
                                    agent_activator.on_new_conversation(&tab_manager.active_tab().conversation.id).await;
                                                    let _ = load_active_tab(&tab_manager, &mut conversation, &mut streaming, &mut session_manager, &mut state, &mut turn_queue);
                                                    // Overwrite the fresh conversation with the loaded one
                                                    conversation = loaded_conv;
                                                    // G1-P4: flip any Running tasks back to Pending — they were in-flight
                                                    // when the session was saved and must not re-enter Running on reload.
                                                    for plan in conversation.plans.values_mut() {
                                                        for task in &mut plan.tasks {
                                                            if task.status == PlanTaskStatus::Running {
                                                                task.status = PlanTaskStatus::Pending;
                                                                task.started_at_ms = None;
                                                            }
                                                        }
                                                    }
                                                    // Story 4-4: hydrate session_meta (bookmarks) for the loaded tab
                                                    if let Ok(Some(meta)) = fs_storage.load_session_meta(&conv_id).await {
                                                        tab_manager.active_tab_mut().session_meta = meta;
                                                    }
                                                    // Save loaded conversation back into TabManager so it's not lost on tab switch
                                                    save_active_tab(&mut tab_manager, &conversation, &streaming, &session_manager, &state, &turn_queue);
                                                    // Update session_index
                                                    session_index.set_open(&conv_id, true);
                                                    session_index.set_active(Some(&conv_id));
                                                    state.sidebar_visible = false;
                                                    state.sidebar_panel = None;
                                                    state.focus = FocusState::Chat;
                                                    state.needs_redraw = true;
                                                }
                                                Ok(None) => {
                                                    tracing::warn!("Sidebar: conversation {} not found in storage", conv_id);
                                                    session_index.remove(&conv_id);
                                                    state.sidebar_entry_count = session_index.len();
                                                    // Clamp sidebar selection after removal
                                                    if state.sidebar_entry_count > 0 {
                                                        state.sidebar_selected = state.sidebar_selected.min(state.sidebar_entry_count - 1);
                                                    } else {
                                                        state.sidebar_selected = 0;
                                                    }
                                                    state.needs_redraw = true;
                                                }
                                                Err(e) => {
                                                    tracing::error!("Failed to load conversation {}: {}", conv_id, e);
                                                    state.status_before_flash = Some(state.status.clone());
                                                    state.status = StatusState::Flash {
                                                        message: format!("Failed to load conversation: {}", e),
                                                        remaining_ms: state.theme.timing.status_flash_ms,
                                                    };
                                                    state.needs_redraw = true;
                                                }
                                            }
                                        }
                                    }
                                }
                                InputAction::DeleteSidebarConversation => {
                                    // Resolve conversation ID from sidebar selection
                                    if let Some(entry) = session_index.entries().get(state.sidebar_selected) {
                                        let conv_id = entry.conversation_id.clone();
                                        let title = entry.title.clone();

                                        // Don't delete the active conversation if it's the only tab
                                        if conv_id == conversation.id && tab_manager.tab_count() == 1 {
                                            state.status = StatusState::Flash {
                                                message: "Cannot delete the only open conversation".to_string(),
                                                remaining_ms: state.theme.timing.status_flash_ms,
                                            };
                                        } else {
                                            // Show confirmation prompt (AC5)
                                            let display_title = if title.is_empty() { "Untitled".to_string() } else { title };
                                            state.pending_delete = Some(DeleteConfirmTarget::Single {
                                                id: conv_id.clone(),
                                                title: display_title.clone()
                                            });
                                            state.status = StatusState::Flash {
                                                message: format!("Delete \"{}\"? This cannot be undone. [y/n]", display_title),
                                                remaining_ms: 30_000, // Long timeout for confirmation
                                            };
                                            state.focus = FocusState::Overlay(OverlayType::Confirmation(
                                                ConfirmationType::DeleteConfirmation(DeleteConfirmTarget::Single {
                                                    id: conv_id.clone(),
                                                    title: display_title
                                                })
                                            ));
                                        }
                                        state.needs_redraw = true;
                                    }
                                }
                                InputAction::DeleteAllConversations => {
                                    // Show confirmation prompt (AC6)
                                    let count = session_index.len();
                                    if count == 0 {
                                        state.status = StatusState::Flash {
                                            message: "No conversations to delete".to_string(),
                                            remaining_ms: state.theme.timing.status_flash_ms,
                                        };
                                    } else {
                                        state.pending_delete = Some(DeleteConfirmTarget::Bulk { count });
                                        state.status = StatusState::Flash {
                                            message: format!("Delete {} conversations? [y/n]", count),
                                            remaining_ms: 30_000,
                                        };
                                        state.focus = FocusState::Overlay(OverlayType::Confirmation(
                                            ConfirmationType::DeleteConfirmation(DeleteConfirmTarget::Bulk { count })
                                        ));
                                    }
                                    state.needs_redraw = true;
                                }
                                InputAction::ConfirmDelete => {
                                    // Execute the pending delete
                                    if let Some(target) = state.pending_delete.take() {
                                        match target {
                                            DeleteConfirmTarget::Single { id: conv_id, .. } => {
                                                // Single conversation delete
                                                if conv_id == conversation.id {
                                                    let tab_id = tab_manager.active_tab_id();
                                                    if streaming.is_streaming {
                                                        if let Some(handle) = _active_turn.take() {
                                                            handle.abort();
                                                        }
                                                        while turn_queue.dequeue().is_some() {}
                                                    }
                                                    tab_manager.close_tab(tab_id);
                                                    state.tab_render_states.remove(&tab_id);
                                                    let should_drain = load_active_tab(&tab_manager, &mut conversation, &mut streaming, &mut session_manager, &mut state, &mut turn_queue);
                                        state.active_agent_name = agent_activator.active_agent_name(&conversation.id).await;
                                                    if should_drain {
                                                        if let Some(queued_msg) = turn_queue.dequeue() {
                                                            { let _snap = skill_activator.snapshot_for_turn(&conversation.id).await; let _agent_snap = agent_activator.snapshot(&conversation.id).await; start_turn(&queued_msg.content, queued_msg.images, &mut conversation, &mut streaming, &mut state, &mut _active_turn, &provider, config, &domain_tx, &security, &tools, &tool_scheduler, &persona, &workspace_path, &mut session_manager, &fs_storage, &storage,
                                              &app_state.plan_manager, &app_state.plan_injector, None, _snap, _agent_snap, tab_manager.reset_and_clone_turn_cancel(), app_state.usage_ledger.clone()).await; }
                                                        }
                                                    }
                                                    session_index.set_active(Some(&conversation.id));
                                                } else if let Some(tab) = tab_manager.tabs().iter().find(|t| t.conversation.id == conv_id) {
                                                    let tab_id = tab.id;
                                                    tab_manager.close_tab(tab_id);
                                                    state.tab_render_states.remove(&tab_id);
                                                }
                                                let fs_ref = fs_storage.clone();
                                                let del_id = conv_id.clone();
                                                tokio::spawn(async move {
                                                    if let Err(e) = fs_ref.delete_conversation(&del_id).await {
                                                        tracing::error!("Failed to delete conversation {}: {}", del_id, e);
                                                    }
                                                });
                                                session_index.remove(&conv_id);
                                                state.sidebar_entry_count = session_index.len();
                                                if state.sidebar_entry_count > 0 {
                                                    state.sidebar_selected = state.sidebar_selected.min(state.sidebar_entry_count - 1);
                                                } else {
                                                    state.sidebar_selected = 0;
                                                }
                                                // U1: Clear status bar immediately after successful delete
                                                state.status = StatusState::Idle;
                                            }
                                            DeleteConfirmTarget::Bulk { .. } => {
                                                // Delete all conversations - F16: Fix race by clearing index BEFORE spawning task
                                                if streaming.is_streaming {
                                                    if let Some(handle) = _active_turn.take() {
                                                        handle.abort();
                                                    }
                                                    streaming.is_streaming = false;
                                                    streaming.phase = crate::domain::models::StreamingPhase::Idle;
                                                    streaming.current_blocks.clear();
                                                    streaming.active_tool_calls.clear();
                                                    while turn_queue.dequeue().is_some() {}
                                                }
                                                // F16: Collect IDs and clear index atomically BEFORE spawning background task
                                                let conv_ids: Vec<String> = session_index.entries().iter()
                                                    .map(|e| e.conversation_id.clone())
                                                    .collect();
                                                // Clear the index immediately to prevent race with new conversations
                                                session_index.clear();
                                                state.sidebar_entry_count = 0;
                                                state.sidebar_selected = 0;
                                                // F14: Reset sidebar_scroll_offset during DeleteAll
                                                state.sidebar_scroll_offset = 0;

                                                let fs_ref = fs_storage.clone();
                                                tokio::spawn(async move {
                                                    for id in conv_ids {
                                                        if let Err(e) = fs_ref.delete_conversation(&id).await {
                                                            tracing::error!("Failed to delete conversation {}: {}", id, e);
                                                        }
                                                    }
                                                });

                                                while tab_manager.tab_count() > 1 {
                                                    let tab_id = tab_manager.tabs().last().map(|t| t.id).unwrap();
                                                    tab_manager.close_tab(tab_id);
                                                    state.tab_render_states.remove(&tab_id);
                                                }
                                                conversation.messages.clear();
                                                conversation.title = String::new();
                                                conversation.id = generate_conversation_id();
                                                conversation.session_id = Some(generate_conversation_id());
                                                conversation.created_at = crate::domain::models::session_meta::now_unix();
                                                conversation.updated_at = crate::domain::models::session_meta::now_unix();
                                                conversation.last_response_at = None;
                                                conversation.usage = None;
                                                conversation.fork_source = None;
                                                save_active_tab(&mut tab_manager, &conversation, &streaming, &session_manager, &state, &turn_queue);
                                                // U1: Clear status bar immediately after successful delete
                                                state.status = StatusState::Flash {
                                                    message: "All conversations deleted".to_string(),
                                                    remaining_ms: state.theme.timing.status_flash_ms,
                                                };
                                            }
                                        }
                                    }
                                    state.focus = FocusState::Input;
                                    state.needs_redraw = true;
                                }
                                InputAction::CancelDelete => {
                                    // Cancel the pending delete - U1: Clear status bar immediately
                                    state.pending_delete = None;
                                    // U1: Clear status bar immediately on cancel (don't restore previous status)
                                    state.status = StatusState::Idle;
                                    state.focus = if state.sidebar_visible {
                                        FocusState::Sidebar {
                                            panel: crate::domain::models::visual::PanelType::History,
                                            selected: state.sidebar_selected,
                                        }
                                    } else {
                                        FocusState::Input
                                    };
                                    state.needs_redraw = true;
                                }
                                // ── Fork Conversation (Story 4-3a) ──────────────
                                InputAction::ForkAtMessage => {
                                    // Guard: cannot fork while streaming
                                    if streaming.is_streaming {
                                        state.status = StatusState::Flash {
                                            message: "Cannot fork while streaming — wait for response to complete".to_string(),
                                            remaining_ms: 3000,
                                        };
                                        state.needs_redraw = true;
                                    } else if conversation.messages.is_empty() {
                                        // Guard: no messages to fork
                                        state.status = StatusState::Flash {
                                            message: "Cannot fork: conversation has no messages".to_string(),
                                            remaining_ms: 3000,
                                        };
                                        state.needs_redraw = true;
                                    } else if state.message_boundaries.is_empty() {
                                        // Guard: chat pane hasn't rendered yet
                                        state.status = StatusState::Flash {
                                            message: "Cannot fork: chat pane not ready".to_string(),
                                            remaining_ms: 3000,
                                        };
                                        state.needs_redraw = true;
                                    } else {
                                        // Determine which message is focused
                                        let fork_message_index = if state.auto_snapshot {
                                            conversation.messages.len().saturating_sub(1)
                                        } else {
                                            let vp = state.viewport_height as usize;
                                            let max_off = state.total_content_height.saturating_sub(vp);
                                            let clamped = state.scroll_snapshot.min(max_off);
                                            let top_line = max_off.saturating_sub(clamped);
                                            match state.message_boundaries.binary_search(&top_line) {
                                                Ok(i) => i,
                                                Err(i) => i.saturating_sub(1),
                                            }
                                            .min(conversation.messages.len().saturating_sub(1))
                                        };
                                        state.pending_fork_index = Some(fork_message_index);
                                        state.focus = FocusState::Overlay(
                                            crate::domain::models::visual::OverlayType::Confirmation(
                                                crate::domain::models::visual::ConfirmationType::Fork,
                                            ),
                                        );
                                        state.needs_redraw = true;
                                    }
                                }
                                InputAction::ForkConfirm => {
                                    // DF-096 (assessment): index-based pending_fork_index is
                                    // safe — no message_id re-validation needed. Reasoning:
                                    // (a) The event loop is single-threaded per select! branch;
                                    //     no concurrent mutation of conversation.messages is
                                    //     possible while this branch executes.
                                    // (b) Messages are only ever APPENDED (never prepended or
                                    //     deleted) while the fork overlay is shown. An index
                                    //     captured at 'f' press time still refers to the same
                                    //     message when 'y' is pressed even if streaming appended
                                    //     further messages in between.
                                    // (c) fork_at_checkpoint re-reads the conversation from
                                    //     storage, but only to obtain the current tail; the
                                    //     slice [..=message_index] is unaffected by appends.
                                    //
                                    // P5 dedup invariant (party-mode review 2026-04-12):
                                    // `.take()` drops pending_fork_index to None BEFORE the
                                    // storage await, so a second 'y' press that lands while
                                    // fork_at_checkpoint is in flight will be a no-op. The
                                    // event loop is single-threaded per iteration — crossterm
                                    // events queue behind the current select! branch and are
                                    // processed after we return. Do not move `.take()` below
                                    // the await without re-analyzing this invariant.
                                    if let Some(message_index) = state.pending_fork_index.take() {
                                        use crate::domain::models::checkpoint::CheckpointId;
                                        let checkpoint = CheckpointId(message_index as u64);
                                        let source_id = conversation.id.clone();
                                        let is_last = message_index == conversation.messages.len().saturating_sub(1);

                                        match storage.fork_at_checkpoint(&source_id, checkpoint).await {
                                            Ok(new_id) => {
                                                match storage.load_conversation(&new_id).await {
                                                    Ok(Some(forked_conv)) => {
                                                        // Save current tab state before creating new tab
                                                        save_active_tab(&mut tab_manager, &conversation, &streaming, &session_manager, &state, &turn_queue);
                                                        // Create new tab with the forked conversation
                                                        tab_manager.create_tab_with_conversation(forked_conv);
                                                        // Load the new active tab
                                                        let _ = load_active_tab(&tab_manager, &mut conversation, &mut streaming, &mut session_manager, &mut state, &mut turn_queue);
                                                        // Update session index
                                                        session_index.set_open(&conversation.id, true);
                                                        session_index.set_active(Some(&conversation.id));
                                                        state.sidebar_entry_count = session_index.len();
                                                        // Show fork hint in status bar
                                                        let hint_msg = if is_last {
                                                            "Forked at last message — both copies are now identical".to_string()
                                                        } else {
                                                            // Get the source conversation title from the fork_source.
                                                            // P4 (party-mode 2026-04-12): guard against empty/whitespace
                                                            // titles so the status doesn't render `Forked from "" at ...`.
                                                            let source_title = conversation.fork_source.as_ref()
                                                                .and_then(|fs| tab_manager.find_by_conversation(&fs.conversation_id))
                                                                .map(|t| t.conversation.title.clone())
                                                                .filter(|t| !t.trim().is_empty())
                                                                .unwrap_or_else(|| "(Untitled)".to_string());
                                                            format!(
                                                                "Forked from \"{}\" at message {}",
                                                                source_title,
                                                                message_index + 1
                                                            )
                                                        };
                                                        state.status = StatusState::Flash {
                                                            message: hint_msg,
                                                            remaining_ms: 5000,
                                                        };
                                                        state.focus = FocusState::Chat;
                                                    }
                                                    Ok(None) => {
                                                        tracing::error!("Forked conversation {} not found after creation", new_id);
                                                        state.focus = FocusState::Chat;
                                                    }
                                                    Err(e) => {
                                                        tracing::error!("Failed to load forked conversation: {}", e);
                                                        state.focus = FocusState::Chat;
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                tracing::error!("Fork failed: {}", e);
                                                state.status = StatusState::Flash {
                                                    message: format!("Fork failed: {}", e),
                                                    remaining_ms: 3000,
                                                };
                                                state.focus = FocusState::Chat;
                                            }
                                        }
                                        state.needs_redraw = true;
                                    }
                                }
                                InputAction::ForkCancel => {
                                    state.pending_fork_index = None;
                                    state.focus = FocusState::Chat;
                                    state.needs_redraw = true;
                                }
                                // ── Rewind Conversation (Story 4-3b) ─────────────────
                                InputAction::RewindAtMessage => {
                                    // DF-018 guard: block if permission/question overlay is active
                                    if state.pending_permission.is_some() || state.ask_user_question.is_some() {
                                        state.status = StatusState::Flash {
                                            message: "Cannot rewind: a permission/question is pending \u{2014} answer it first".to_string(),
                                            remaining_ms: 3000,
                                        };
                                        state.needs_redraw = true;
                                    } else if streaming.is_streaming {
                                        state.status = StatusState::Flash {
                                            message: "Cannot rewind while streaming \u{2014} wait for response to complete".to_string(),
                                            remaining_ms: 3000,
                                        };
                                        state.needs_redraw = true;
                                    } else if conversation.messages.is_empty() {
                                        state.status = StatusState::Flash {
                                            message: "Cannot rewind: conversation has no messages".to_string(),
                                            remaining_ms: 3000,
                                        };
                                        state.needs_redraw = true;
                                    } else if state.message_boundaries.is_empty() {
                                        state.status = StatusState::Flash {
                                            message: "Cannot rewind: chat pane not ready".to_string(),
                                            remaining_ms: 3000,
                                        };
                                        state.needs_redraw = true;
                                    } else {
                                        // MIRRORED FROM FORK: same message-targeting algorithm
                                        let target_message_index = if state.auto_snapshot {
                                            conversation.messages.len().saturating_sub(1)
                                        } else {
                                            let vp = state.viewport_height as usize;
                                            let max_off = state.total_content_height.saturating_sub(vp);
                                            let clamped = state.scroll_snapshot.min(max_off);
                                            let top_line = max_off.saturating_sub(clamped);
                                            match state.message_boundaries.binary_search(&top_line) {
                                                Ok(i) => i,
                                                Err(i) => i.saturating_sub(1),
                                            }
                                            .min(conversation.messages.len().saturating_sub(1))
                                        };

                                        let messages_to_remove = conversation.messages.len()
                                            .saturating_sub(target_message_index + 1);
                                        let files = storage
                                            .list_snapshot_files(&conversation.id, target_message_index)
                                            .await
                                            .unwrap_or_default();
                                        let files_to_revert: Vec<crate::adapters::tui::state::RevertPreviewItem> = files
                                            .into_iter()
                                            .map(|(path, is_conflict)| {
                                                let display_path = path
                                                    .strip_prefix(&workspace_path)
                                                    .map(|p| p.to_string_lossy().to_string())
                                                    .unwrap_or_else(|_| path.to_string_lossy().to_string());
                                                crate::adapters::tui::state::RevertPreviewItem {
                                                    display_path,
                                                    conflict: is_conflict,
                                                }
                                            })
                                            .collect();

                                        state.pending_rewind_index = Some(target_message_index);
                                        state.rewind_preview = Some(crate::adapters::tui::state::RewindPreview {
                                            target_message_index,
                                            messages_to_remove,
                                            files_to_revert,
                                        });
                                        state.focus = FocusState::Overlay(
                                            crate::domain::models::visual::OverlayType::Confirmation(
                                                crate::domain::models::visual::ConfirmationType::Rewind,
                                            ),
                                        );
                                        state.needs_redraw = true;
                                    }
                                }
                                InputAction::RewindConfirm => {
                                    if let Some(target_msg_idx) = state.pending_rewind_index.take() {
                                        state.rewind_preview = None;

                                        tracing::debug!(
                                            "rewind: target_msg_idx={}, conv_id={}",
                                            target_msg_idx, conversation.id
                                        );

                                        if let Err(e) = storage.begin_rewind_txn(
                                            &conversation.id,
                                            target_msg_idx,
                                        ).await {
                                            tracing::error!(
                                                "begin_rewind_txn failed — aborting rewind: {}",
                                                e
                                            );
                                            state.status = StatusState::Flash {
                                                message: format!("Rewind aborted: could not create recovery journal ({})", e),
                                                remaining_ms: 5000,
                                            };
                                            state.focus = FocusState::Chat;
                                            state.needs_redraw = true;
                                            continue;
                                        }

                                        // 1. Revert files FIRST — needs the checkpoint log to resolve
                                        //    which snapshots belong to messages above target_msg_idx.
                                        //    Truncation prunes the log, so revert must run before it.
                                        let reverted = storage
                                            .revert_file_snapshots(&conversation.id, target_msg_idx)
                                            .await
                                            .unwrap_or_default();
                                        let restored = reverted
                                            .iter()
                                            .filter(|r| matches!(r.status, crate::domain::models::RevertStatus::Restored))
                                            .count();
                                        let conflicts = reverted
                                            .iter()
                                            .filter(|r| matches!(r.status, crate::domain::models::RevertStatus::Conflict { .. }))
                                            .count();

                                        tracing::debug!(
                                            "rewind: reverted={}, restored={}, conflicts={}, target_msg_idx={}",
                                            reverted.len(), restored, conflicts, target_msg_idx
                                        );
                                        for r in &reverted {
                                            tracing::debug!(
                                                "rewind: file={} → {:?}",
                                                r.path.display(), r.status
                                            );
                                        }

                                        // 2. Truncate conversation to the user's selected message index.
                                        match storage.truncate_conversation(&conversation.id, target_msg_idx).await {
                                            Ok(truncated) => {
                                                // Commit transaction journal — both phases done.
                                                let _ = storage.commit_rewind_txn(&conversation.id)
                                                    .await
                                                    .map_err(|e| {
                                                        tracing::warn!(
                                                            "commit_rewind_txn failed (non-fatal): {}",
                                                            e
                                                        );
                                                    });

                                                // 3. Update active-tab proxy (AC3 step 3)
                                                save_active_tab(&mut tab_manager, &conversation, &streaming, &session_manager, &state, &turn_queue);
                                                tab_manager.active_tab_mut().conversation = truncated;
                                                let _ = load_active_tab(&tab_manager, &mut conversation, &mut streaming, &mut session_manager, &mut state, &mut turn_queue);

                                                // 4. DF-005: height cache eviction for removed turns.
                                                // Per-tab cache survives tab switches; rewind explicitly invalidates
                                                // because the conversation shape changed.
                                                state.tab_render_state(state.active_tab_id).height_cache.invalidate_all();

                                                // 5. Cancel in-flight permission/question + drain queue (AC3 step 5)
                                                // Dropping response_tx closes the channel → turn gets Err
                                                drop(state.pending_permission.take());
                                                drop(state.ask_user_question.take());
                                                drop(state.question_response_tx.take());
                                                while turn_queue.dequeue().is_some() {}
                                                // Abort active turn
                                                if let Some(handle) = _active_turn.take() {
                                                    handle.abort();
                                                }

                                                // 6. Status hint (5000ms — completed action, Status Timeout Rule)
                                                let status_msg = if conflicts > 0 {
                                                    format!(
                                                        "\u{2139} Rewound to message {}. Reverted {} files. \u{26a0} {} files skipped (modified externally).",
                                                        target_msg_idx + 1, restored, conflicts
                                                    )
                                                } else {
                                                    format!(
                                                        "\u{2139} Rewound to message {}. Reverted {} files.",
                                                        target_msg_idx + 1, restored
                                                    )
                                                };
                                                state.status = StatusState::Flash {
                                                    message: status_msg,
                                                    remaining_ms: 5000,
                                                };
                                            }
                                            Err(e) => {
                                                tracing::error!("revert_to_checkpoint failed: {}", e);
                                                state.status = StatusState::Flash {
                                                    message: format!("Rewind failed: {}", e),
                                                    remaining_ms: 3000,
                                                };
                                            }
                                        }
                                        state.focus = FocusState::Chat;
                                        state.needs_redraw = true;
                                    }
                                }
                                InputAction::RewindCancel => {
                                    state.pending_rewind_index = None;
                                    state.rewind_preview = None;
                                    state.focus = FocusState::Chat;
                                    state.needs_redraw = true;
                                }
                                InputAction::RewindForkInstead => {
                                    // AC4: rewind → fork-instead path; mirrors ForkConfirm body.
                                    state.rewind_preview = None;
                                    if let Some(message_index) = state.pending_rewind_index.take() {
                                        use crate::domain::models::checkpoint::CheckpointId;
                                        let checkpoint = CheckpointId(message_index as u64);
                                        let source_id = conversation.id.clone();
                                        let is_last = message_index == conversation.messages.len().saturating_sub(1);

                                        match storage.fork_at_checkpoint(&source_id, checkpoint).await {
                                            Ok(new_id) => {
                                                match storage.load_conversation(&new_id).await {
                                                    Ok(Some(forked_conv)) => {
                                                        save_active_tab(&mut tab_manager, &conversation, &streaming, &session_manager, &state, &turn_queue);
                                                        tab_manager.create_tab_with_conversation(forked_conv);
                                                        let _ = load_active_tab(&tab_manager, &mut conversation, &mut streaming, &mut session_manager, &mut state, &mut turn_queue);
                                                        session_index.set_open(&conversation.id, true);
                                                        session_index.set_active(Some(&conversation.id));
                                                        state.sidebar_entry_count = session_index.len();
                                                        let hint_msg = if is_last {
                                                            "Forked at last message \u{2014} both copies are now identical".to_string()
                                                        } else {
                                                            let source_title = conversation.fork_source.as_ref()
                                                                .and_then(|fs| tab_manager.find_by_conversation(&fs.conversation_id))
                                                                .map(|t| t.conversation.title.clone())
                                                                .filter(|t| !t.trim().is_empty())
                                                                .unwrap_or_else(|| "(Untitled)".to_string());
                                                            format!(
                                                                "Forked from \"{}\" at message {}",
                                                                source_title,
                                                                message_index + 1
                                                            )
                                                        };
                                                        state.status = StatusState::Flash {
                                                            message: hint_msg,
                                                            remaining_ms: 5000,
                                                        };
                                                        state.focus = FocusState::Chat;
                                                    }
                                                    Ok(None) => {
                                                        tracing::error!("Forked conversation {} not found after creation", new_id);
                                                        state.focus = FocusState::Chat;
                                                    }
                                                    Err(e) => {
                                                        tracing::error!("Failed to load forked conversation: {}", e);
                                                        state.focus = FocusState::Chat;
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                tracing::error!("Fork (from rewind) failed: {}", e);
                                                state.status = StatusState::Flash {
                                                    message: format!("Fork failed: {}", e),
                                                    remaining_ms: 3000,
                                                };
                                                state.focus = FocusState::Chat;
                                            }
                                        }
                                        state.needs_redraw = true;
                                    } else {
                                        state.focus = FocusState::Chat;
                                        state.needs_redraw = true;
                                    }
                                }
                                // ── Story 4-4: Within-Conversation Search ──────────────
                                InputAction::OpenSearch => {
                                    // app.rs::handle_special_key already set focus
                                    // and activated search_state. Event loop just
                                    // needs to trigger the initial (empty) find_matches
                                    // so the counter renders 0/0 correctly and mark redraw.
                                    state.needs_redraw = true;
                                }
                                InputAction::CloseSearch => {
                                    let prior = state
                                        .search_state
                                        .prior_focus
                                        .take()
                                        .unwrap_or(FocusState::Chat);
                                    state.search_state =
                                        crate::adapters::tui::state::SearchState::new();
                                    state.focus = prior;
                                    state.needs_redraw = true;
                                }
                                InputAction::SearchQueryChanged
                                | InputAction::SearchClear
                                | InputAction::SearchReturnToTyping => {
                                    apply_search_rescan(&conversation, &mut state);
                                }
                                InputAction::SearchCommit => {
                                    if !state.search_state.matches.is_empty() {
                                        state.search_state.substate =
                                            crate::adapters::tui::state::SearchSubstate::Navigating;
                                    } else {
                                        state.status_before_flash = Some(state.status.clone());
                                        state.status = StatusState::Flash {
                                            message: "No matches found".to_string(),
                                            remaining_ms: 1200,
                                        };
                                    }
                                    state.needs_redraw = true;
                                }
                                InputAction::SearchNext => {
                                    apply_search_navigate(&mut state, 1);
                                }
                                InputAction::SearchPrev => {
                                    apply_search_navigate(&mut state, -1);
                                }
                                // ── Story 4-4: Bookmarks ──────────────────────────────
                                InputAction::ToggleBookmark => {
                                    // Await any in-flight conversation save before mutating
                                    // meta.json to serialize all writes to the same file
                                    // (Dev Notes § Bookmark Persistence — Failure Mode 1).
                                    if let Some(prev) = _pending_save.take() {
                                        let _ = prev.await;
                                    }
                                    apply_bookmark_toggle(
                                        &mut tab_manager,
                                        &mut state,
                                        &conversation,
                                        &fs_storage,
                                    ).await;
                                }
                                InputAction::OpenBookmarkList => {
                                    apply_open_bookmark_list(&tab_manager, &mut state);
                                }
                                InputAction::JumpToBookmark => {
                                    apply_jump_bookmark(&tab_manager, &mut state);
                                }
                                InputAction::DeleteBookmark => {
                                    if let Some(prev) = _pending_save.take() {
                                        let _ = prev.await;
                                    }
                                    apply_delete_bookmark(
                                        &mut tab_manager,
                                        &mut state,
                                        &conversation,
                                        &fs_storage,
                                    ).await;
                                }
                                InputAction::UndoBookmarkDelete => {
                                    if let Some(prev) = _pending_save.take() {
                                        let _ = prev.await;
                                    }
                                    apply_undo_bookmark_delete(
                                        &mut tab_manager,
                                        &mut state,
                                        &conversation,
                                        &fs_storage,
                                    ).await;
                                }
                                InputAction::CloseBookmarkList => {
                                    state.focus = FocusState::Chat;
                                    state.bookmark_list_selected = 0;
                                    state.needs_redraw = true;
                                }
                                // ── Story 4-4: Cross-Conversation Search ──────────────
                                InputAction::OpenCrossSearch => {
                                    state.needs_redraw = true;
                                }
                                InputAction::CrossSearchQueryChanged => {
                                    // Story 4-4 AC5 — delegate the guard to
                                    // `apply_cross_search_query_change` so tests exercise the
                                    // same code path (third-audit Fix R4). If the helper
                                    // returns Spawn, kick off the async scan; Cleared is a
                                    // no-op aside from the needs_redraw below.
                                    match apply_cross_search_query_change(&mut state) {
                                        CrossSearchScanAction::Spawn { query } => {
                                            let storage_ref = storage.clone();
                                            let index_clone = session_index.clone();
                                            let tx = domain_tx.clone();
                                            tokio::spawn(async move {
                                                use crate::domain::services::cross_search::{
                                                    CrossSearchBudget, run_cross_search,
                                                };
                                                let outcome = run_cross_search(
                                                    &*storage_ref,
                                                    &index_clone,
                                                    &query,
                                                    CrossSearchBudget::default(),
                                                )
                                                .await;
                                                let _ = tx.send(AppEvent::CrossSearchResultsReady {
                                                    query,
                                                    results: outcome.results,
                                                    truncated_by_count: outcome.truncated_by_count,
                                                    truncated_by_time: outcome.truncated_by_time,
                                                });
                                            });
                                        }
                                        CrossSearchScanAction::Cleared => {}
                                    }
                                    state.needs_redraw = true;
                                }
                                InputAction::OpenCrossSearchResult => {
                                    apply_open_cross_search_result(
                                        &mut tab_manager,
                                        &mut conversation,
                                        &mut streaming,
                                        &mut session_manager,
                                        &mut state,
                                        &mut turn_queue,
                                        &*storage,
                                        &fs_storage,
                                        &mut session_index,
                                        &domain_tx,
                                    )
                                    .await;
                                }
                                InputAction::CloseCrossSearch => {
                                    state.cross_search =
                                        crate::adapters::tui::state::CrossSearchState::new();
                                    state.focus = FocusState::Sidebar {
                                        panel: crate::domain::models::visual::PanelType::History,
                                        selected: state.sidebar_selected,
                                    };
                                    state.needs_redraw = true;
                                }
                                InputAction::ConfirmExportOverwrite => {
                                    // Third-audit Fix R6 — delegate to the `pub` helper so
                                    // integration tests exercise the same code path as the
                                    // event loop.
                                    apply_confirm_export_overwrite(&mut state).await;
                                }
                                InputAction::CancelExportOverwrite => {
                                    apply_cancel_export_overwrite(&mut state);
                                }
                                InputAction::SetActiveAgent { name, then_submit } => {
                                    let conv_id = conversation.id.clone();
                                    let prior = agent_activator.active_agent_name(&conv_id).await;
                                    match agent_activator.activate(&conv_id, &name).await {
                                        Ok(_active) => {
                                            state.active_agent_name = Some(name.clone());
                                            let notice_msg = if let Some(prior_name) = prior {
                                                format!("Active agent: {} (was {})", name, prior_name)
                                            } else {
                                                format!("Active agent: {}", name)
                                            };
                                            app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                                conversation_id: Some(conv_id.clone()),
                                                level: crate::domain::models::NoticeLevel::Info,
                                                message: notice_msg,
                                            });
                                            if let Some(text) = then_submit {
                                                app_state.event_bus.emit_domain(AppEvent::AgentThenSubmit {
                                                    conversation_id: conv_id,
                                                    text,
                                                    synthetic: false,
                                                });
                                            }
                                        }
                                        Err(e) => {
                                            let level = match e {
                                                crate::adapters::agent_activation::AgentActivationError::FileMissing { .. } | crate::adapters::agent_activation::AgentActivationError::FileTooLarge { .. } => crate::domain::models::NoticeLevel::Error,
                                                _ => crate::domain::models::NoticeLevel::Warning,
                                            };
                                            if matches!(e, crate::adapters::agent_activation::AgentActivationError::OutsideWorkspace(..)) {
                                                state.agent_registry.remove(&name);
                                            }
                                            app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                                conversation_id: Some(conv_id),
                                                level,
                                                message: format!("Agent activation failed: {}", e),
                                            });
                                        }
                                    }
                                    state.needs_redraw = true;
                                }
                                InputAction::ClearActiveAgent { then_submit } => {
                                    let conv_id = conversation.id.clone();
                                    let had_agent = agent_activator.active_agent_name(&conv_id).await.is_some();
                                    agent_activator.deactivate(&conv_id).await;
                                    state.active_agent_name = None;
                                    let notice_msg = if had_agent {
                                        "Active agent cleared — using project-context persona".to_string()
                                    } else {
                                        "No active agent".to_string()
                                    };
                                    app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                        conversation_id: Some(conv_id.clone()),
                                        level: crate::domain::models::NoticeLevel::Info,
                                        message: notice_msg,
                                    });
                                    if let Some(text) = then_submit {
                                        app_state.event_bus.emit_domain(AppEvent::AgentThenSubmit {
                                            conversation_id: conv_id,
                                            text,
                                            synthetic: false,
                                        });
                                    }
                                    state.needs_redraw = true;
                                }
                                InputAction::AgentDiscoveryPending { name, then_submit } => {
                                    state.pending_agent_activation =
                                        Some((name.clone(), then_submit));
                                    app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                        conversation_id: Some(conversation.id.clone()),
                                        level: crate::domain::models::NoticeLevel::Info,
                                        message: format!(
                                            "Agent '{}' will activate when discovery completes — please wait",
                                            name
                                        ),
                                    });
                                    state.needs_redraw = true;
                                }
                                InputAction::UnknownAgent(name) => {
                                    app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                        conversation_id: Some(conversation.id.clone()),
                                        level: crate::domain::models::NoticeLevel::Warning,
                                        message: format!(
                                            "Unknown agent: '{}'. Type @Agents/ to see available agents.",
                                            name
                                        ),
                                    });
                                    state.needs_redraw = true;
                                }
                                // === Story 16.6: Vim keymap dispatchers ===

                                InputAction::FoldToggleAtFocus => {
                                    let turn_id = match tab_manager.active_tab().view_state.focused_turn.clone() {
                                        Some(t) => t,
                                        None => {
                                            tracing::warn!("z-fold key with no focused turn");
                                            state.needs_redraw = true;
                                            continue;
                                        }
                                    };
                                    reconcile_fold_toggle(&mut tab_manager, &mut state, &conversation, turn_id.clone(), |vs, rs| {
                                        chat_pane::toggle_turn_fold(vs, rs, &turn_id);
                                    });
                                }

                                InputAction::CollapseFocus => {
                                    let turn_id = match tab_manager.active_tab().view_state.focused_turn.clone() {
                                        Some(t) => t,
                                        None => {
                                            tracing::warn!("z-fold key with no focused turn");
                                            state.needs_redraw = true;
                                            continue;
                                        }
                                    };
                                    reconcile_fold_toggle(&mut tab_manager, &mut state, &conversation, turn_id.clone(), |vs, rs| {
                                        chat_pane::set_turn_collapsed(vs, rs, &turn_id, true);
                                    });
                                }

                                InputAction::ExpandFocus => {
                                    let turn_id = match tab_manager.active_tab().view_state.focused_turn.clone() {
                                        Some(t) => t,
                                        None => {
                                            tracing::warn!("z-fold key with no focused turn");
                                            state.needs_redraw = true;
                                            continue;
                                        }
                                    };
                                    reconcile_fold_toggle(&mut tab_manager, &mut state, &conversation, turn_id.clone(), |vs, rs| {
                                        chat_pane::set_turn_collapsed(vs, rs, &turn_id, false);
                                    });
                                }

                                InputAction::CollapseAllTurns => {
                                    let turns = conversation.turns.clone();
                                    let focused_turn_id = tab_manager.active_tab().view_state.focused_turn.clone();
                                    let event_turn_id = focused_turn_id.unwrap_or_else(|| turns.first().map(|t| t.id.clone()).unwrap_or_else(|| TurnId(String::new())));
                                    reconcile_fold_toggle(&mut tab_manager, &mut state, &conversation, event_turn_id, |vs, rs| {
                                        chat_pane::collapse_all_turns(vs, rs, &turns);
                                    });
                                }

                                InputAction::ExpandAllTurns => {
                                    let turns = conversation.turns.clone();
                                    let focused_turn_id = tab_manager.active_tab().view_state.focused_turn.clone();
                                    let event_turn_id = focused_turn_id.unwrap_or_else(|| turns.first().map(|t| t.id.clone()).unwrap_or_else(|| TurnId(String::new())));
                                    reconcile_fold_toggle(&mut tab_manager, &mut state, &conversation, event_turn_id, |vs, rs| {
                                        chat_pane::expand_all_turns(vs, rs, &turns);
                                    });
                                }

                                InputAction::JumpProseAnchor(direction) => {
                                    state.pending_anchor_drop = None; // S16.8 AC15: explicit anchor action resets gate
                                    let turns = conversation.turns.clone();
                                    let focused = tab_manager.active_tab().view_state.focused_turn.clone();
                                    let vp_height = state.viewport_height as usize;
                                    let width = state.terminal_width;
                                    let theme = state.theme.clone();
                                    let clock = tab_manager.active_tab().clock.clone();
                                    let vs = tab_manager.active_tab().view_state.clone();
                                    let layout = {
                                        let tool_block_states = state.tool_block_states.clone();
                                        let rs = state.tab_render_state(state.active_tab_id);
                                        chat_pane::build_layout_metrics(&conversation, &vs, rs, &theme, width, vp_height, &*clock, &tool_block_states)
                                    };
                                    let scroll_offset = state.scroll_snapshot;
                                    let total_h = layout.total_content_height;
                                    let vp_h = layout.viewport_height;
                                    let visible_bottom = total_h.saturating_sub(scroll_offset);
                                    let visible_top = visible_bottom.saturating_sub(vp_h);
                                    let topmost_on_screen = layout.turn_top_offsets.iter()
                                        .filter(|(_, off)| *off >= visible_top && *off <= visible_bottom)
                                        .find(|(tid, _)| {
                                            turns.iter().any(|t| &t.id == tid && t.role == crate::domain::models::MessageRole::Assistant)
                                        })
                                        .map(|(tid, _)| tid.clone());
                                    let validated_focused = focused.clone().filter(|ft| turns.iter().any(|t| &t.id == ft));
                                    let start_ref = validated_focused.or(topmost_on_screen.clone());
                                    tracing::debug!(
                                        "JumpProseAnchor({:?}): focused={:?} topmost_on_screen={:?} start_ref={:?}",
                                        direction, focused, topmost_on_screen, start_ref,
                                    );
                                    let target = if let Some(start_id) = start_ref {
                                        let start_idx = match turns.iter().position(|t| t.id == start_id) {
                                            Some(i) => i,
                                            None => {
                                                tracing::debug!("JumpProseAnchor: start_ref turn not found");
                                                state.needs_redraw = true;
                                                continue;
                                            }
                                        };
                                        match direction {
                                            crate::adapters::tui::state::Direction::Down => {
                                                turns.iter().skip(start_idx + 1).find(|t| {
                                                    t.role == crate::domain::models::MessageRole::Assistant
                                                        && t.parts.iter().any(|p| matches!(p, crate::domain::models::TurnPart::Prose { .. }))
                                                })
                                            }
                                            crate::adapters::tui::state::Direction::Up => {
                                                turns.iter().take(start_idx).rev().find(|t| {
                                                    t.role == crate::domain::models::MessageRole::Assistant
                                                        && t.parts.iter().any(|p| matches!(p, crate::domain::models::TurnPart::Prose { .. }))
                                                })
                                            }
                                            crate::adapters::tui::state::Direction::Left | crate::adapters::tui::state::Direction::Right => None,
                                        }
                                    } else {
                                        // No starting reference: pick the first (Down) or last (Up) prose turn
                                        // and treat it as the START (not the target), then find next/prev from there
                                        let fallback_start = match direction {
                                            crate::adapters::tui::state::Direction::Down => {
                                                turns.iter().position(|t| {
                                                    t.role == crate::domain::models::MessageRole::Assistant
                                                        && t.parts.iter().any(|p| matches!(p, crate::domain::models::TurnPart::Prose { .. }))
                                                })
                                            }
                                            crate::adapters::tui::state::Direction::Up => {
                                                turns.iter().rposition(|t| {
                                                    t.role == crate::domain::models::MessageRole::Assistant
                                                        && t.parts.iter().any(|p| matches!(p, crate::domain::models::TurnPart::Prose { .. }))
                                                })
                                            }
                                            crate::adapters::tui::state::Direction::Left | crate::adapters::tui::state::Direction::Right => None,
                                        };
                                        // For Down with no focus: go to first prose turn (idx 0, treat like "start before first")
                                        // For Up with no focus: go to last prose turn
                                        fallback_start.and_then(|start_idx| {
                                            match direction {
                                                crate::adapters::tui::state::Direction::Down => {
                                                    // We're at the first turn, but `]]` means "next" —
                                                    // start_idx - 1 + 1 = start_idx. So just return the first.
                                                    turns.get(start_idx)
                                                }
                                                crate::adapters::tui::state::Direction::Up => {
                                                    // We're at the last turn, but `[[` means "previous" — same.
                                                    turns.get(start_idx)
                                                }
                                                crate::adapters::tui::state::Direction::Left | crate::adapters::tui::state::Direction::Right => None,
                                            }
                                        })
                                    };
                                    if let Some(t) = target {
                                        tab_manager.active_tab_mut().view_state.set_focused_turn(Some(t.id.clone()));
                                        // Set Pinned mode via reconcile (correctly anchors the turn)
                                        let layout = { let tool_block_states = state.tool_block_states.clone(); let rs = state.tab_render_state(state.active_tab_id); chat_pane::build_layout_metrics(&conversation, &tab_manager.active_tab().view_state, rs, &theme, width, vp_height, &*clock, &tool_block_states) };
                                        let _ = tab_manager.active_tab_mut().view_state.reconcile(Some(crate::domain::models::ViewEvent::JumpTurn { turn_id: t.id.clone() }), &layout);
                                        // Compute scroll position using the same render-driven approach
                                        // as {}/{}.  Try the TurnId→MessageId mapping (rebuild_messages_mirror
                                        // guarantees it for assistant turns); fall back to a content-based
                                        // linear scan for conversations loaded from disk where IDs diverge.
                                        let msg_idx = conversation.messages.iter().position(|m| m.id == t.id.0)
                                            .or_else(|| {
                                                // Fallback: match by turn's first prose part text prefix
                                                let first_prose = t.parts.iter().find_map(|p| {
                                                    if let crate::domain::models::TurnPart::Prose { text, .. } = p {
                                                        Some(text.as_str())
                                                    } else { None }
                                                });
                                                first_prose.and_then(|prose| {
                                                    conversation.messages.iter()
                                                        .filter(|m| m.role == crate::domain::models::MessageRole::Assistant)
                                                        .position(|m| m.content.starts_with(prose) || prose.starts_with(&m.content))
                                                })
                                            });
                                        if let Some(idx) = msg_idx {
                                            state.scroll_snapshot = chat_pane::find_scroll_offset_for_message(
                                                idx,
                                                &state.message_boundaries,
                                                state.total_content_height,
                                                state.viewport_height as usize,
                                            );
                                            state.auto_snapshot = state.scroll_snapshot == 0;
                                            tab_manager.active_tab_mut().view_state.scroll_offset = state.scroll_snapshot;
                                        }
                                    } else {
                                        tracing::debug!("JumpProseAnchor: no prose anchor in direction={:?}", direction);
                                        let msg = match direction {
                                            crate::adapters::tui::state::Direction::Down => "No next prose turn",
                                            crate::adapters::tui::state::Direction::Up => "No previous prose turn",
                                            crate::adapters::tui::state::Direction::Left | crate::adapters::tui::state::Direction::Right => "",
                                        };
                                        state.status = StatusState::Flash { message: msg.to_string(), remaining_ms: 2000 };
                                    }
                                    state.needs_redraw = true;
                                }

                                InputAction::JumpToLatestProseAnchor => {
                                    let turns = conversation.turns.clone();
                                    let latest_prose = turns.iter().rev().find(|t| {
                                        t.role == crate::domain::models::MessageRole::Assistant
                                            && t.parts.iter().any(|p| matches!(p, crate::domain::models::TurnPart::Prose { .. }))
                                    });
                                    if let Some(t) = latest_prose {
                                        let vp_height = state.viewport_height as usize;
                                        let width = state.terminal_width;
                                        let theme = state.theme.clone();
                                        tab_manager.active_tab_mut().view_state.set_focused_turn(Some(t.id.clone()));
                                        let layout = { let tool_block_states = state.tool_block_states.clone(); let clock = tab_manager.active_tab().clock.clone(); let rs = state.tab_render_state(state.active_tab_id); chat_pane::build_layout_metrics(&conversation, &tab_manager.active_tab().view_state, rs, &theme, width, vp_height, &*clock, &tool_block_states) };
                                        let _ = tab_manager.active_tab_mut().view_state.reconcile(Some(crate::domain::models::ViewEvent::JumpTurn { turn_id: t.id.clone() }), &layout);
                                        // Use render-driven message_boundaries with ID-match + fallback
                                        let msg_idx = conversation.messages.iter().position(|m| m.id == t.id.0)
                                            .or_else(|| {
                                                let first_prose = t.parts.iter().find_map(|p| {
                                                    if let crate::domain::models::TurnPart::Prose { text, .. } = p {
                                                        Some(text.as_str())
                                                    } else { None }
                                                });
                                                first_prose.and_then(|prose| {
                                                    conversation.messages.iter()
                                                        .filter(|m| m.role == crate::domain::models::MessageRole::Assistant)
                                                        .position(|m| m.content.starts_with(prose) || prose.starts_with(&m.content))
                                                })
                                            });
                                        if let Some(idx) = msg_idx {
                                            state.scroll_snapshot = chat_pane::find_scroll_offset_for_message(
                                                idx,
                                                &state.message_boundaries,
                                                state.total_content_height,
                                                state.viewport_height as usize,
                                            );
                                            state.auto_snapshot = state.scroll_snapshot == 0;
                                            tab_manager.active_tab_mut().view_state.scroll_offset = state.scroll_snapshot;
                                        }
                                    } else {
                                        tab_manager.active_tab_mut().view_state.scroll_offset = 0;
                                        tab_manager.active_tab_mut().view_state.mode = crate::domain::models::AnchorMode::Following;
                                        state.scroll_snapshot = 0;
                                        state.auto_snapshot = true;
                                        tracing::debug!("JumpToLatestProseAnchor: empty transcript fallback");
                                    }
                                    state.needs_redraw = true;
                                }

                                InputAction::RecenterAnchor => {
                                    let focused = tab_manager.active_tab().view_state.focused_turn.clone();
                                    let turns = conversation.turns.clone();
                                    let vp_height = state.viewport_height as usize;
                                    let width = state.terminal_width;
                                    let theme = state.theme.clone();
                                    let clock = tab_manager.active_tab().clock.clone();
                                    let target = if let Some(ft) = focused {
                                        Some(ft)
                                    } else {
                                        let vs = tab_manager.active_tab().view_state.clone();
                                        let tool_block_states = state.tool_block_states.clone();
                                        let layout = {
                                            let rs = state.tab_render_state(state.active_tab_id);
                                            chat_pane::build_layout_metrics(&conversation, &vs, rs, &theme, width, vp_height, &*clock, &tool_block_states)
                                        };
                                        let scroll_offset = state.scroll_snapshot;
                                        let total_h = layout.total_content_height;
                                        let vp_h = layout.viewport_height;
                                        let visible_bottom = total_h.saturating_sub(scroll_offset);
                                        let visible_top = visible_bottom.saturating_sub(vp_h);
                                        layout.turn_top_offsets.iter()
                                            .filter(|(_, off)| *off >= visible_top && *off <= visible_bottom)
                                            .find(|(tid, _)| {
                                                turns.iter().any(|t| &t.id == tid && t.role == crate::domain::models::MessageRole::Assistant)
                                            })
                                            .map(|(tid, _)| tid.clone())
                                    };
                                    if let Some(turn_id) = target {
                                        tab_manager.active_tab_mut().view_state.set_focused_turn(Some(turn_id.clone()));
                                        let layout = { let tool_block_states = state.tool_block_states.clone(); let rs = state.tab_render_state(state.active_tab_id); chat_pane::build_layout_metrics(&conversation, &tab_manager.active_tab().view_state, rs, &theme, width, vp_height, &*clock, &tool_block_states) };
                                        let _ = tab_manager.active_tab_mut().view_state.reconcile(Some(crate::domain::models::ViewEvent::JumpTurn { turn_id: turn_id.clone() }), &layout);
                                        // Use render-driven message_boundaries with ID-match + content fallback
                                        let msg_idx = conversation.messages.iter().position(|m| m.id == turn_id.0)
                                            .or_else(|| {
                                                let target_turn = turns.iter().find(|t| t.id == turn_id);
                                                let first_prose = target_turn.and_then(|t| t.parts.iter().find_map(|p| {
                                                    if let crate::domain::models::TurnPart::Prose { text, .. } = p {
                                                        Some(text.as_str())
                                                    } else { None }
                                                }));
                                                first_prose.and_then(|prose| {
                                                    conversation.messages.iter()
                                                        .filter(|m| m.role == crate::domain::models::MessageRole::Assistant)
                                                        .position(|m| m.content.starts_with(prose) || prose.starts_with(&m.content))
                                                })
                                            });
                                        if let Some(idx) = msg_idx {
                                            state.scroll_snapshot = chat_pane::find_scroll_offset_for_message(
                                                idx,
                                                &state.message_boundaries,
                                                state.total_content_height,
                                                state.viewport_height as usize,
                                            );
                                            state.auto_snapshot = state.scroll_snapshot == 0;
                                            tab_manager.active_tab_mut().view_state.scroll_offset = state.scroll_snapshot;
                                        }
                                        state.status = StatusState::Flash { message: "Recenter: turn snapshot to top".to_string(), remaining_ms: 2000 };
                                    } else {
                                        tracing::debug!("RecenterAnchor: no target turn");
                                        state.status = StatusState::Flash { message: "No assistant turn to recenter".into(), remaining_ms: 2000 };
                                    }
                                    state.needs_redraw = true;
                                }

                                InputAction::ToggleSummaryTier => {
                                    tracing::debug!("ToggleSummaryTier: dispatching");
                                    let next_tier = match tab_manager.active_tab().view_state.summary_tier {
                                        crate::domain::models::SummaryTier::Tier1 => crate::domain::models::SummaryTier::Tier2,
                                        crate::domain::models::SummaryTier::Tier2 => crate::domain::models::SummaryTier::Tier1,
                                    };
                                    {
                                        let rs = state.tab_render_state(state.active_tab_id);
                                        chat_pane::set_summary_tier(&mut tab_manager.active_tab_mut().view_state, rs, next_tier);
                                    }
                                    let tier_name = match next_tier {
                                        crate::domain::models::SummaryTier::Tier1 => "Tier1 (terse)",
                                        crate::domain::models::SummaryTier::Tier2 => "Tier2 (descriptive)",
                                    };
                                    state.status = StatusState::Flash { message: format!("Summary tier: {}", tier_name), remaining_ms: 2000 };
                                    state.needs_redraw = true;
                                }

                                InputAction::CycleInvocationInFocusedTurn => {
                                    tracing::debug!("CycleInvocationInFocusedTurn: dispatching");
                                    let turns = conversation.turns.clone();
                                    let focused = tab_manager.active_tab().view_state.focused_turn.clone();
                                    let can_cycle = focused.as_ref().is_some_and(|ft| {
                                        if let Some(turn) = turns.iter().find(|t| &t.id == ft) {
                                            if tab_manager.active_tab().view_state.is_collapsed(turn) {
                                                return false;
                                            }
                                            turn.parts.iter().filter(|p| matches!(p, crate::domain::models::TurnPart::ToolInvocation { .. })).count() >= 2
                                        } else {
                                            false
                                        }
                                    });
                                    if can_cycle {
                                        let ft = focused.expect("can_cycle ensures focused_turn is Some");
                                        let turn = turns.iter().find(|t| t.id == ft).expect("can_cycle ensures turn exists");
                                        let invocations: Vec<_> = turn.parts.iter()
                                            .filter(|p| matches!(p, crate::domain::models::TurnPart::ToolInvocation { .. }))
                                            .collect();
                                        let current_idx = state.focused_tool_id.as_ref().and_then(|ftid| {
                                            invocations.iter().position(|p| {
                                                if let crate::domain::models::TurnPart::ToolInvocation { id, .. } = p {
                                                    crate::domain::models::turn::tool_call_id_for(&ft, *id) == *ftid
                                                } else { false }
                                            })
                                        });
                                        let next_idx = match current_idx {
                                            Some(idx) => (idx + 1) % invocations.len(),
                                            None => 0, // first Tab press focuses first invocation
                                        };
                                        if let Some(crate::domain::models::TurnPart::ToolInvocation { id, .. }) = invocations.get(next_idx) {
                                            state.focused_tool_id = Some(crate::domain::models::turn::tool_call_id_for(&ft, *id));
                                        }
                                        state.needs_redraw = true;
                                    } else if state.sidebar_visible {
                                        state.focus = FocusState::Sidebar {
                                            panel: crate::domain::models::visual::PanelType::History,
                                            selected: state.sidebar_selected,
                                        };
                                        state.needs_redraw = true;
                                    } else if tab_manager.tab_count() > 1 {
                                        save_active_tab(&mut tab_manager, &conversation, &streaming, &session_manager, &state, &turn_queue);
                                        tab_manager.switch_to_next();
                                        let should_drain = load_active_tab(&tab_manager, &mut conversation, &mut streaming, &mut session_manager, &mut state, &mut turn_queue);
                                        state.active_agent_name = agent_activator.active_agent_name(&conversation.id).await;
                                        if should_drain {
                                            if let Some(queued_msg) = turn_queue.dequeue() {
                                                { let _snap = skill_activator.snapshot_for_turn(&conversation.id).await; let _agent_snap = agent_activator.snapshot(&conversation.id).await; start_turn(&queued_msg.content, queued_msg.images, &mut conversation, &mut streaming, &mut state, &mut _active_turn, &provider, config, &domain_tx, &security, &tools, &tool_scheduler, &persona, &workspace_path, &mut session_manager, &fs_storage, &storage,
                                              &app_state.plan_manager, &app_state.plan_injector, None, _snap, _agent_snap, tab_manager.reset_and_clone_turn_cancel(), app_state.usage_ledger.clone()).await; }
                                            }
                                        }
                                        session_index.set_active(Some(&conversation.id));
                                        state.needs_redraw = true;
                                    }
                                }

                                // === Story 16.8: Fast scroll + mouse dispatchers ===

                                InputAction::ScrollLineDown => {
                                    apply_scroll_intent(&mut tab_manager, &mut state, &conversation, &app_state, crate::domain::models::view_state::ScrollDelta::LineDown);
                                }
                                InputAction::ScrollLineUp => {
                                    apply_scroll_intent(&mut tab_manager, &mut state, &conversation, &app_state, crate::domain::models::view_state::ScrollDelta::LineUp);
                                }
                                InputAction::BlockJump { offset, auto_scroll } => {
                                    // S16.8, AC7: Block-boundary jumps set absolute scroll offset.
                                    tab_manager.active_tab_mut().view_state.scroll_offset = offset;
                                    if auto_scroll {
                                        tab_manager.active_tab_mut().view_state.mode = crate::domain::models::AnchorMode::Following;
                                    } else {
                                        // P3: Jumps away from bottom enter Reading so mode and offset stay consistent.
                                        tab_manager.active_tab_mut().view_state.mode = crate::domain::models::AnchorMode::Reading;
                                    }
                                    state.scroll_snapshot = offset;
                                    state.auto_snapshot = auto_scroll;
                                    state.needs_redraw = true;
                                }
                                InputAction::ScrollHalfPageDown => {
                                    apply_scroll_intent(&mut tab_manager, &mut state, &conversation, &app_state, crate::domain::models::view_state::ScrollDelta::HalfPageDown);
                                }
                                InputAction::ScrollHalfPageUp => {
                                    apply_scroll_intent(&mut tab_manager, &mut state, &conversation, &app_state, crate::domain::models::view_state::ScrollDelta::HalfPageUp);
                                }
                                InputAction::ScrollFullPageDown => {
                                    apply_scroll_intent(&mut tab_manager, &mut state, &conversation, &app_state, crate::domain::models::view_state::ScrollDelta::FullPageDown);
                                }
                                InputAction::ScrollFullPageUp => {
                                    apply_scroll_intent(&mut tab_manager, &mut state, &conversation, &app_state, crate::domain::models::view_state::ScrollDelta::FullPageUp);
                                }
                                InputAction::ScrollToTop => {
                                    dispatch_view_scroll(&mut tab_manager, &mut state, &conversation, crate::domain::models::view_state::ScrollDelta::Top);
                                }
                                InputAction::ScrollToBottom => {
                                    // Mode-aware G handler: Pinned → no-op + teaching toast; else ScrollToBottom.
                                    // AC3 + AC15 (G is jump-intent, not scroll-intent).
                                    let mode = tab_manager.active_tab().view_state.mode.clone();
                                    if matches!(mode, crate::domain::models::AnchorMode::Pinned(_)) {
                                        app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                            conversation_id: Some(conversation.id.clone()),
                                            level: crate::domain::models::NoticeLevel::Info,
                                            message: "Anchored. Press ]] to release, then G to jump.".to_string(),
                                        });
                                        state.needs_redraw = true;
                                    } else {
                                        dispatch_view_scroll(&mut tab_manager, &mut state, &conversation, crate::domain::models::view_state::ScrollDelta::Bottom);
                                    }
                                }
                                InputAction::MouseScroll(delta) => {
                                    apply_scroll_intent(&mut tab_manager, &mut state, &conversation, &app_state, delta);
                                }

                                InputAction::Consumed | InputAction::Ignored => {}
                                InputAction::OpenModelSelector => {
                                    let active_pid = router.active_delegate_id();
                                    let effective_mid = effective_model(&state, config).to_string();
                                    let providers = app_state.provider_registry.list_providers();
                                    let columns: Vec<crate::adapters::tui::state::ProviderColumn> = providers
                                        .into_iter()
                                        .map(|pd| {
                                            let models = app_state
                                                .provider_registry
                                                .list_models_by_provider(&pd.provider_id);
                                            crate::adapters::tui::state::ProviderColumn {
                                                provider_id: pd.provider_id,
                                                display_name: pd.display_name,
                                                healthy: pd.healthy,
                                                models,
                                            }
                                        })
                                        .filter(|c| !c.models.is_empty())
                                        .collect();
                                    if columns.is_empty() {
                                        let fb = crate::domain::models::FeedbackBlock {
                                            id: "model-selector-empty".to_string(),
                                            level: crate::domain::models::FeedbackLevel::Info,
                                            message: "No models available".to_string(),
                                            actions: Vec::new(),
                                        };
                                        state.feedback_blocks.insert(fb.id.clone(), fb);
                                        state.needs_redraw = true;
                                    } else {
                                        state.model_selector.open(
                                            state.focus.clone(),
                                            columns,
                                            &active_pid,
                                            &effective_mid,
                                        );
                                        state.focus = crate::domain::models::FocusState::Overlay(
                                            crate::domain::models::visual::OverlayType::ModelSelector,
                                        );
                                        state.needs_redraw = true;
                                    }
                                }
                                InputAction::SwitchModelProvider { provider_id, model_id } => {
                                    apply_model_switch(
                                        &mut state,
                                        &router,
                                        &app_state,
                                        &streaming,
                                        &domain_tx,
                                        &conversation,
                                        provider_id,
                                        model_id,
                                        &mut pending_health_check,
                                    ).await;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Terminal event stream error: {}", e);
                        break;
                    }
                }
            }

            // Branch 2: Domain events (mpsc unbounded channel)
            Some(event) = domain_events_rx.recv() => {
                match event {
                    AppEvent::Shutdown => break,
                    AppEvent::ToolCallTransitionBridged { conversation_id, transition } => {
                        if conversation_id == conversation.id {
                            if let Some(tc) = streaming.active_tool_calls.get_mut(transition.call.id()) {
                                tc.status = Some(crate::domain::models::tool_call::status_chip(&transition.call).to_string());
                            }
                            state.needs_redraw = true;
                        }
                    }
                    AppEvent::ProviderChunk { conversation_id, chunk } => {
                        if conversation_id == conversation.id {
                            // Active tab — apply chunk via reduce() + mirror sync
                            let clock = tab_manager.active_tab().clock.clone();
                            let action = reduce(
                                &mut tab_manager.active_tab_mut().reducer,
                                chunk,
                                &*clock,
                            );
                            update_streaming_mirror(&tab_manager.active_tab().reducer, &mut streaming);

                            // Propagate pending usage to conversation clone
                            if let Some(usage) = tab_manager.active_tab_mut().reducer.pending_usage.take() {
                                conversation.usage = Some(usage);
                            }

                            // Drain committed turn into the conversation clone (syncs to tab_manager via save_active_tab)
                            if let Some(committed) = tab_manager.active_tab_mut().reducer.committed_turn.take() {
                                conversation.turns.push(committed);
                                conversation.rebuild_messages_mirror();
                            }
                            // Compute title-generation trigger after committed turn is drained
                            let title_trigger = conversation.turns.len() == 2
                                && conversation.title.is_empty();

                            match action {
                                ChunkAction::NeedsRedraw => {
                                    state.needs_redraw = true;
                                }
                                ChunkAction::TurnComplete { persist, .. } => {
                                    state.status = StatusState::Idle;
                                    // Sync token usage from conversation to TUI state
                                    state.token_usage = conversation.usage.clone();
                                    // Clear stale feedback/retry state on successful turn
                                    state.active_feedback_id = None;
                                    state.retry_state = None;
                                    state.needs_redraw = true;
                                    _active_turn = None;

                                    // Dispatch any deferred AgentThenSubmit (Story 6-2a race fix)
                                    if let Some((text, synthetic)) = state.pending_agent_then_submit.take() {
                                        tracing::debug!("Dispatching deferred AgentThenSubmit after stream complete");
                                        let _skill_snap = skill_activator.snapshot_for_turn(&conversation.id).await;
                                        let _agent_snap = agent_activator.snapshot(&conversation.id).await;
                                        start_turn_inner(
                                            &text,
                                            Vec::new(),
                                            synthetic,
                                            &mut conversation,
                                            &mut streaming,
                                            &mut state,
                                            &mut _active_turn,
                                            &provider,
                                            config,
                                            &domain_tx,
                                            &security,
                                            &tools, &tool_scheduler,
                                            &persona,
                                            &workspace_path,
                                            &mut session_manager,
                                            &fs_storage,
                                            &storage,
                                            &app_state.plan_manager, &app_state.plan_injector,
                                            None,
                                            _skill_snap,
                                            _agent_snap,
                                            tab_manager.reset_and_clone_turn_cancel(),
                                        app_state.usage_ledger.clone()).await;
                                    }

                                    // Update sidebar index on turn complete
                                    session_index.touch(
                                        &conversation.id,
                                        if conversation.title.is_empty() { None } else { Some(conversation.title.clone()) },
                                        Some(conversation.messages.len()),
                                    );
                                    state.sidebar_entry_count = session_index.len();

                                    // Persist conversation on turn complete
                                    if persist {
                                        let now = crate::domain::models::session_meta::now_unix();
                                        conversation.updated_at = now;
                                        conversation.last_response_at = Some(now);
                                        if let Some(prev) = _pending_save.take() {
                                            let _ = prev.await;
                                        }
                                        let storage_ref = storage.clone();
                                        let conv_clone = conversation.clone();
                                        _pending_save = Some(tokio::spawn(async move {
                                            match tokio::time::timeout(
                                                BACKGROUND_TASK_TIMEOUT,
                                                storage_ref.save_conversation(&conv_clone),
                                            ).await {
                                                Ok(Ok(())) => {}
                                                Ok(Err(e)) => {
                                                    tracing::error!("Failed to persist session: {}", e);
                                                }
                                                Err(_) => {
                                                    tracing::warn!("Background task 'session_save' timed out after {}s", BACKGROUND_TASK_TIMEOUT.as_secs());
                                                }
                                            }
                                        }));
                                    }

                                    // Spawn background title generation (AC1, AC3, AC5)
                                    if title_trigger
                                    {
                                        let provider_ref = provider.clone();
                                        let event_tx_ref = domain_tx.clone();
                                        let model = config.model.clone();
                                        let user_msg = conversation.messages[0].content.clone();
                                        let assistant_msg = truncate(&conversation.messages[1].content, 500);
                                        let title_conv_id = conversation.id.clone();
                                        tokio::spawn(async move {
                                            match tokio::time::timeout(
                                                BACKGROUND_TASK_TIMEOUT,
                                                generate_title(&*provider_ref, &model, &user_msg, &assistant_msg),
                                            ).await {
                                                Ok(Ok(title)) => {
                                                    let _ = event_tx_ref.send(AppEvent::TitleGenerated {
                                                        conversation_id: title_conv_id,
                                                        title,
                                                    });
                                                }
                                                Ok(Err(e)) => {
                                                    tracing::warn!("Title generation failed: {}", e);
                                                }
                                                Err(_) => {
                                                    tracing::warn!("Background task 'title_generation' timed out after {}s", BACKGROUND_TASK_TIMEOUT.as_secs());
                                                }
                                            }
                                        });
                                    }

                                    // Auto-send queued messages
                                    if let Some(queued_msg) = turn_queue.dequeue() {
                                        let _skill_snap = skill_activator.snapshot_for_turn(&conversation.id).await;
                                        let _agent_snap = agent_activator.snapshot(&conversation.id).await;
                                        start_turn(
                                            &queued_msg.content,
                                            queued_msg.images,
                                            &mut conversation,
                                            &mut streaming,
                                            &mut state,
                                            &mut _active_turn,
                                            &provider,
                                            config,
                                            &domain_tx,
                                            &security,
                                            &tools, &tool_scheduler,
                                            &persona,
                                            &workspace_path,
                                            &mut session_manager,
                                            &fs_storage,
                                            &storage,
                                              &app_state.plan_manager, &app_state.plan_injector,
                                            None,
                                            _skill_snap,
                                            _agent_snap,
                                                                              tab_manager.reset_and_clone_turn_cancel(),
                                        app_state.usage_ledger.clone()).await;
                                        match render(terminal, &mut state, &conversation, &streaming, &config.model, &router.active_delegate_id(), security.as_ref(), tab_manager.tab_count(), tab_manager.active_tab_index(), Some(&tab_manager), &session_index) {
                                            Ok(()) => state.needs_redraw = false,
                                            Err(e) => handle_render_error(e, &mut _active_turn, &mut streaming, &mut state, terminal),
                                        }
                                    }
                                }
                                ChunkAction::TurnContinuing => {
                                    state.status = StatusState::Executing {
                                        tool_name: "tools".to_string(),
                                        elapsed_ms: 0,
                                    };
                                    state.needs_redraw = true;
                                }
                                ChunkAction::None => {}
                            }
                        } else if let Some(tab) = tab_manager.find_by_conversation_mut(&conversation_id) {
                            // Background tab — route chunk via reduce() + mirror sync
                            let action = reduce(
                                &mut tab.reducer,
                                chunk,
                                &*tab.clock.clone(),
                            );
                            update_streaming_mirror(&tab.reducer, &mut tab.streaming);

                            // Propagate pending usage to conversation
                            if let Some(usage) = tab.reducer.pending_usage.take() {
                                tab.conversation.usage = Some(usage);
                            }

                            // Drain committed turn into Conversation.turns
                            if let Some(committed) = tab.reducer.committed_turn.take() {
                                tab.conversation.turns.push(committed);
                                tab.conversation.rebuild_messages_mirror();
                            }

                            // Compute title-generation trigger after committed turn is drained
                            let bg_title_trigger = tab.conversation.turns.len() == 2
                                && tab.conversation.title.is_empty();

                            if let ChunkAction::TurnComplete { persist, .. } = action {
                                // reduce() committed the turn; tab.streaming.is_streaming synced via mirror
                                if persist {
                                    let now = crate::domain::models::session_meta::now_unix();
                                    tab.conversation.updated_at = now;
                                    tab.conversation.last_response_at = Some(now);
                                    if let Some(prev) = _pending_save.take() {
                                        let _ = prev.await;
                                    }
                                    let storage_ref = storage.clone();
                                    let conv_clone = tab.conversation.clone();
                                    _pending_save = Some(tokio::spawn(async move {
                                        match tokio::time::timeout(
                                            BACKGROUND_TASK_TIMEOUT,
                                            storage_ref.save_conversation(&conv_clone),
                                        ).await {
                                            Ok(Ok(())) => {}
                                            Ok(Err(e)) => tracing::error!("Failed to persist background tab: {}", e),
                                            Err(_) => tracing::warn!("Background tab save timed out"),
                                        }
                                    }));
                                }
                                if bg_title_trigger
                                {
                                    let provider_ref = provider.clone();
                                    let event_tx_ref = domain_tx.clone();
                                    let model = config.model.clone();
                                    let user_msg = tab.conversation.messages[0].content.clone();
                                    let assistant_msg = truncate(&tab.conversation.messages[1].content, 500);
                                    let title_conv_id = tab.conversation.id.clone();
                                    tokio::spawn(async move {
                                        match tokio::time::timeout(
                                            BACKGROUND_TASK_TIMEOUT,
                                            generate_title(&*provider_ref, &model, &user_msg, &assistant_msg),
                                        ).await {
                                            Ok(Ok(title)) => {
                                                let _ = event_tx_ref.send(AppEvent::TitleGenerated {
                                                    conversation_id: title_conv_id,
                                                    title,
                                                });
                                            }
                                            Ok(Err(e)) => tracing::warn!("Background title gen failed: {}", e),
                                            Err(_) => tracing::warn!("Background title gen timed out"),
                                        }
                                    });
                                }
                                // Redraw tab bar — background tab may have gained a title
                                state.needs_redraw = true;
                            }
                            // NeedsRedraw/TurnContinuing/None for background tab:
                            // user isn't viewing it, no UI update needed
                        }
                    }
                    AppEvent::SystemNotice { conversation_id: notice_conv_id, level, message: msg } => {
                        // Route by conversation_id: None = global (always active tab),
                        // Some(id) = only affects that conversation's tab.
                        let is_active_tab = notice_conv_id
                            .as_deref()
                            .is_none_or(|id| id == conversation.id);

                        if is_active_tab {
                            // Detect session expiry from provider authentication errors.
                            if matches!(level, crate::domain::models::NoticeLevel::Error)
                                && (msg.contains("Authentication failed")
                                    || msg.contains("HTTP 401")
                                    || msg.contains("status: 401")
                                    || msg.to_lowercase().contains("session expired"))
                            {
                                session_manager.mark_invalidated(
                                    conversation.session_id.clone(),
                                );
                                tracing::info!("Session invalidated due to auth error, will rebuild on next message");
                            }

                            // Patch #23: one-shot model fallback when provider rejects agent model (AC6).
                            if matches!(level, crate::domain::models::NoticeLevel::Error)
                                && state.active_agent_name.is_some()
                            {
                                let lower = msg.to_lowercase();
                                let looks_like_model_error = lower.contains("model")
                                    && (lower.contains("not found")
                                        || lower.contains("does not exist")
                                        || lower.contains("model_not_found")
                                        || lower.contains("invalid_request_error"));
                                if looks_like_model_error {
                                    let agent_snap = agent_activator.snapshot(&conversation.id).await;
                                    let agent_model = agent_snap
                                        .as_ref()
                                        .and_then(|a| a.model.as_ref())
                                        .cloned()
                                        .unwrap_or_default();
                                    if !agent_model.is_empty() && agent_model != config.model {
                                        // Find and remove the last user message so start_turn re-adds it
                                        let last_user_text = conversation
                                            .messages
                                            .iter()
                                            .rev()
                                            .find(|m| m.role == MessageRole::User)
                                            .map(|m| m.content.clone());
                                        if let (Some(text), Some(mut fallback_agent)) = (last_user_text, agent_snap) {
                                            if let Some(pos) = conversation.messages.iter().rposition(|m| m.role == MessageRole::User) {
                                                conversation.messages.truncate(pos);
                                            }
                                            fallback_agent.model = None;
                                            app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                                conversation_id: Some(conversation.id.clone()),
                                                level: NoticeLevel::Warning,
                                                message: format!(
                                                    "Agent '{}' specifies unknown model '{}' — falling back to default '{}'",
                                                    fallback_agent.name, agent_model, config.model
                                                ),
                                            });
                                            let _skill_snap = skill_activator.snapshot_for_turn(&conversation.id).await;
                                            start_turn(
                                                &text,
                                                vec![],
                                                &mut conversation,
                                                &mut streaming,
                                                &mut state,
                                                &mut _active_turn,
                                                &provider,
                                                config,
                                                &domain_tx,
                                                &security,
                                                &tools, &tool_scheduler,
                                                &persona,
                                                &workspace_path,
                                                &mut session_manager,
                                                &fs_storage,
                                                &storage,
                                              &app_state.plan_manager, &app_state.plan_injector,
                                                None,
                                                _skill_snap,
                                                Some(fallback_agent),
                                                                                  tab_manager.reset_and_clone_turn_cancel(),
                                            app_state.usage_ledger.clone()).await;
                                        }
                                    }
                                }
                            }

                            // Only reset streaming state for Error/Warning notices.
                            if !matches!(level, crate::domain::models::NoticeLevel::Info) {
                                streaming.is_streaming = false;
                                streaming.phase = crate::domain::models::StreamingPhase::Idle;
                                streaming.current_blocks.clear();
                                streaming.active_tool_calls.clear();
                                // Only abort _active_turn when it belongs to this tab
                                if let Some(handle) = _active_turn.take() {
                                    handle.abort();
                                }
                            }

                            // Create FeedbackBlock for errors and warnings; flash for info
                            match level {
                                crate::domain::models::NoticeLevel::Error => {
                                    static FB_COUNTER: AtomicUsize = AtomicUsize::new(0);
                                    let fb_id = format!("fb-{}", FB_COUNTER.fetch_add(1, Ordering::Relaxed));
                                    let fb = FeedbackBlock {
                                        id: fb_id.clone(),
                                        level: FeedbackLevel::Error,
                                        message: msg,
                                        actions: vec![FeedbackAction::Retry],
                                    };
                                    state.feedback_blocks.insert(fb_id.clone(), fb);
                                    state.active_feedback_id = Some(fb_id);
                                    state.status = StatusState::Idle;
                                    state.focus = FocusState::Chat;
                                }
                                crate::domain::models::NoticeLevel::Warning => {
                                    apply_warning_notice(&mut state, msg);
                                }
                                _ => {
                                    state.status_before_flash = Some(state.status.clone());
                                    state.status = StatusState::Flash {
                                        message: msg,
                                        remaining_ms: state.theme.timing.status_flash_ms,
                                    };
                                }
                            }
                            state.needs_redraw = true;
                        } else if let Some(id) = notice_conv_id {
                            // Background tab error — apply to its stored state in TabManager
                            if let Some(tab) = tab_manager.find_by_conversation_mut(&id) {
                                if !matches!(level, crate::domain::models::NoticeLevel::Info) {
                                    tab.streaming.is_streaming = false;
                                    tab.streaming.phase = crate::domain::models::StreamingPhase::Idle;
                                    tab.streaming.current_blocks.clear();
                                    tab.streaming.active_tool_calls.clear();
                                }
                                if matches!(level, crate::domain::models::NoticeLevel::Error) {
                                    static BG_FB_COUNTER: AtomicUsize = AtomicUsize::new(0);
                                    let fb_id = format!("bgfb-{}", BG_FB_COUNTER.fetch_add(1, Ordering::Relaxed));
                                    let fb = FeedbackBlock {
                                        id: fb_id.clone(),
                                        level: FeedbackLevel::Error,
                                        message: msg,
                                        actions: vec![FeedbackAction::Retry],
                                    };
                                    tab.feedback_blocks.insert(fb_id.clone(), fb);
                                    tab.active_feedback_id = Some(fb_id);
                                    tab.streaming.is_streaming = false;
                                }
                                // Redraw so tab bar can reflect the state change
                                state.needs_redraw = true;
                            }
                        }
                    }
                    AppEvent::ApprovalRuntimeEventBridged { event } => {
                        use crate::domain::services::approval_runtime::ApprovalRuntimeEvent;
                        match event {
                            ApprovalRuntimeEvent::Requested { id, source, tool, input_preview, risk } => {
                                use crate::adapters::tui::state::PendingPermission;
                                let new_pending = PendingPermission {
                                    id,
                                    source,
                                    tool_name: tool,
                                    tool_input: input_preview,
                                    risk,
                                };
                                if state.pending_plan_card.is_some() || state.pending_permission.is_some() {
                                    state.permission_queue.push(new_pending);
                                } else {
                                    state.pending_permission = Some(new_pending);
                                    state.focus = FocusState::Overlay(OverlayType::Confirmation(
                                        ConfirmationType::Permission,
                                    ));
                                }
                                state.needs_redraw = true;
                            }
                            ApprovalRuntimeEvent::Resolved { id, .. } | ApprovalRuntimeEvent::Cancelled { id, .. } => {
                                if state.pending_feedback_input.as_ref().map(|fi| fi.pending_permission.id.0 == id.0).unwrap_or(false) {
                                    state.pending_feedback_input = None;
                                    state.focus = FocusState::Input;
                                    state.needs_redraw = true;
                                } else if state.pending_permission.as_ref().map(|p| p.id.0 == id.0).unwrap_or(false) {
                                    state.pending_permission = None;
                                    advance_permission_queue(&mut state);
                                    state.needs_redraw = true;
                                } else {
                                    let mut new_queue = std::collections::VecDeque::new();
                                    while let Some(p) = state.permission_queue.pop() {
                                        if p.id.0 != id.0 {
                                            new_queue.push_back(p);
                                        }
                                    }
                                    state.permission_queue = crate::adapters::tui::state::PermissionQueue { queue: new_queue };
                                }
                            }
                        }
                    }
                    AppEvent::PlanApprovalRequested { conversation_id, plan_path, contents, summary } => {
                        let pending = crate::adapters::tui::state::PendingPlanApproval {
                            conversation_id,
                            plan_path,
                            contents,
                            summary,
                        };
                        state.pending_plan_approval = Some(pending);
                        state.focus = FocusState::Overlay(OverlayType::Confirmation(
                            ConfirmationType::PlanApproval,
                        ));
                        state.needs_redraw = true;
                    }
                    AppEvent::PlanApprovalResolved { conversation_id: _conversation_id, outcome } => {
                        state.pending_plan_approval = None;
                        state.focus = FocusState::Input;
                        state.needs_redraw = true;

                        match outcome {
                            crate::domain::models::PlanApprovalOutcome::ApproveNormal => {
                                app_state.event_bus.emit_domain(AppEvent::SetPermissionMode(PermissionMode::Normal));
                                let plan_path = app_state.plan_manager.plan_file_for(&mut tab_manager.active_tab_mut().session_meta).path;
                                let synthetic_msg = crate::domain::models::ChatMessage {
                                    id: crate::domain::models::generate_conversation_id(),
                                    role: crate::domain::models::MessageRole::User,
                                    content: format!("The plan at {} has been approved. Execute it.", plan_path.display()),
                                    content_blocks: vec![],
                                    tool_calls: vec![],
                                    created_at: crate::domain::models::session_meta::now_unix(),
                                    token_count: None,
                                    stop_reason: None,
                                    synthetic: true,
                                    images: vec![],
                                };
                                conversation.messages.push(synthetic_msg);
                                let text = conversation.messages.last().map(|m| m.content.clone()).unwrap_or_default();
                                let _snap = skill_activator.snapshot_for_turn(&conversation.id).await;
                                let _agent_snap = agent_activator.snapshot(&conversation.id).await;
                                state.plan_file_path = None;
                                tool_scheduler.set_plan_file(None).await;
                                start_turn(&text, vec![], &mut conversation, &mut streaming, &mut state, &mut _active_turn, &provider, config, &domain_tx, &security, &tools, &tool_scheduler, &persona, &workspace_path, &mut session_manager, &fs_storage, &storage,
                                              &app_state.plan_manager, &app_state.plan_injector, None, _snap, _agent_snap, tab_manager.reset_and_clone_turn_cancel(), app_state.usage_ledger.clone()).await;
                            }
                            crate::domain::models::PlanApprovalOutcome::ApproveAutoEdit => {
                                app_state.event_bus.emit_domain(AppEvent::SetPermissionMode(PermissionMode::AutoEdit));
                                let plan_path = app_state.plan_manager.plan_file_for(&mut tab_manager.active_tab_mut().session_meta).path;
                                let synthetic_msg = crate::domain::models::ChatMessage {
                                    id: crate::domain::models::generate_conversation_id(),
                                    role: crate::domain::models::MessageRole::User,
                                    content: format!("The plan at {} has been approved. Execute it.", plan_path.display()),
                                    content_blocks: vec![],
                                    tool_calls: vec![],
                                    created_at: crate::domain::models::session_meta::now_unix(),
                                    token_count: None,
                                    stop_reason: None,
                                    synthetic: true,
                                    images: vec![],
                                };
                                conversation.messages.push(synthetic_msg);
                                let text = conversation.messages.last().map(|m| m.content.clone()).unwrap_or_default();
                                let _snap = skill_activator.snapshot_for_turn(&conversation.id).await;
                                let _agent_snap = agent_activator.snapshot(&conversation.id).await;
                                state.plan_file_path = None;
                                tool_scheduler.set_plan_file(None).await;
                                start_turn(&text, vec![], &mut conversation, &mut streaming, &mut state, &mut _active_turn, &provider, config, &domain_tx, &security, &tools, &tool_scheduler, &persona, &workspace_path, &mut session_manager, &fs_storage, &storage,
                                              &app_state.plan_manager, &app_state.plan_injector, None, _snap, _agent_snap, tab_manager.reset_and_clone_turn_cancel(), app_state.usage_ledger.clone()).await;
                            }
                            crate::domain::models::PlanApprovalOutcome::Reject => {
                                let _ = domain_tx.send(AppEvent::SystemNotice {
                                    conversation_id: Some(conversation.id.clone()),
                                    level: NoticeLevel::Info,
                                    message: "Plan rejected. The agent has been informed.".to_string(),
                                });
                                let synthetic_msg = crate::domain::models::ChatMessage {
                                    id: crate::domain::models::generate_conversation_id(),
                                    role: crate::domain::models::MessageRole::User,
                                    content: "Plan rejected. Please revise the plan based on the user's feedback.".to_string(),
                                    content_blocks: vec![],
                                    tool_calls: vec![],
                                    created_at: crate::domain::models::session_meta::now_unix(),
                                    token_count: None,
                                    stop_reason: None,
                                    synthetic: true,
                                    images: vec![],
                                };
                                conversation.messages.push(synthetic_msg);
                            }
                            crate::domain::models::PlanApprovalOutcome::Revise => {
                            }
                        }
                    }
                    AppEvent::PlanProposed { conversation_id, plan } => {
                        if conversation.id != conversation_id {
                            tracing::warn!("PlanProposed for unknown conversation {}", conversation_id);
                        } else {
                            // Helper: find the ID of the turn that is currently proposing
                            // this plan (open streaming turn, or last committed turn).
                            // Using the turn ID ensures the PlanCard survives
                            // rebuild_messages_mirror() because the message ID matches
                            // the turn ID that gets committed.
                            let current_turn_id = tab_manager
                                .active_tab()
                                .reducer
                                .open_turn
                                .as_ref()
                                .map(|t| t.id.0.clone())
                                .or_else(|| conversation.turns.last().map(|t| t.id.0.clone()));

                            let msg_id = if let Some(last_msg) = conversation.messages.last_mut() {
                                if last_msg.role == MessageRole::Assistant {
                                    last_msg.content_blocks.push(ContentBlockType::PlanCard);
                                    last_msg.id.clone()
                                } else if let Some(ref turn_id) = current_turn_id {
                                    // Attach to existing message for this turn if present
                                    if let Some(existing) = conversation.messages.iter_mut().find(|m| m.id == *turn_id) {
                                        existing.content_blocks.push(ContentBlockType::PlanCard);
                                        turn_id.clone()
                                    } else {
                                        let msg = ChatMessage {
                                            id: turn_id.clone(),
                                            role: MessageRole::Assistant,
                                            content: String::new(),
                                            content_blocks: vec![ContentBlockType::PlanCard],
                                            tool_calls: vec![],
                                            created_at: crate::domain::models::session_meta::now_unix(),
                                            token_count: None,
                                            stop_reason: None,
                                            synthetic: false,
                                            images: vec![],
                                        };
                                        conversation.messages.push(msg);
                                        turn_id.clone()
                                    }
                                } else {
                                    let id = crate::domain::models::generate_message_id();
                                    let msg = ChatMessage {
                                        id: id.clone(),
                                        role: MessageRole::Assistant,
                                        content: String::new(),
                                        content_blocks: vec![ContentBlockType::PlanCard],
                                        tool_calls: vec![],
                                        created_at: crate::domain::models::session_meta::now_unix(),
                                        token_count: None,
                                        stop_reason: None,
                                        synthetic: false,
                                        images: vec![],
                                    };
                                    conversation.messages.push(msg);
                                    id
                                }
                            } else if let Some(ref turn_id) = current_turn_id {
                                let msg = ChatMessage {
                                    id: turn_id.clone(),
                                    role: MessageRole::Assistant,
                                    content: String::new(),
                                    content_blocks: vec![ContentBlockType::PlanCard],
                                    tool_calls: vec![],
                                    created_at: crate::domain::models::session_meta::now_unix(),
                                    token_count: None,
                                    stop_reason: None,
                                    synthetic: false,
                                    images: vec![],
                                };
                                conversation.messages.push(msg);
                                turn_id.clone()
                            } else {
                                let id = crate::domain::models::generate_message_id();
                                let msg = ChatMessage {
                                    id: id.clone(),
                                    role: MessageRole::Assistant,
                                    content: String::new(),
                                    content_blocks: vec![ContentBlockType::PlanCard],
                                    tool_calls: vec![],
                                    created_at: crate::domain::models::session_meta::now_unix(),
                                    token_count: None,
                                    stop_reason: None,
                                    synthetic: false,
                                    images: vec![],
                                };
                                conversation.messages.push(msg);
                                id
                            };

                            let mut plan = plan;
                            plan.host_message_id = Some(msg_id);
                            let plan_id = plan.id.clone();
                            let plan_title = plan.title.clone();
                            let task_count = plan.tasks.len();
                            conversation.plans.insert(plan.id.clone(), plan.clone());

                            if security.current_mode() == PermissionMode::Yolo {
                                if let Some(stored_plan) = conversation.plans.get_mut(&plan_id) {
                                    stored_plan.status = crate::domain::models::plan::PlanStatus::Executing;
                                    stored_plan.resolved_at = Some(crate::domain::models::session_meta::now_unix());
                                }
                                let _ = domain_tx.send(AppEvent::SystemNotice {
                                    conversation_id: Some(conversation.id.clone()),
                                    level: NoticeLevel::Info,
                                    message: format!("Plan auto-approved (YOLO mode): {} — {} tasks", plan_title, task_count),
                                });
                                let _ = domain_tx.send(AppEvent::PlanExecutionStarted {
                                    conversation_id: conversation.id.clone(),
                                    plan_id,
                                });
                            } else {
                                if let Some(ref old_pending) = state.pending_plan_card {
                                    if let Some(old_plan) = conversation.plans.get_mut(&old_pending.plan_id) {
                                        tracing::warn!("Superseding pending plan card: {}", old_pending.plan_id);
                                        old_plan.status = crate::domain::models::plan::PlanStatus::Rejected;
                                        old_plan.resolved_at = Some(crate::domain::models::session_meta::now_unix());
                                    }
                                }
                                state.pending_plan_card = Some(crate::adapters::tui::state::PendingPlanCard {
                                    conversation_id: conversation.id.clone(),
                                    plan_id,
                                    plan_snapshot: plan,
                                });
                            }
                            state.needs_redraw = true;
                        }
                    }
                    AppEvent::PlanCardResolved { conversation_id, plan_id, decision } => {
                        if conversation.id != conversation_id {
                            tracing::warn!("PlanCardResolved for mismatched conversation");
                        } else {
                            let cid = conversation.id.clone();
                            match decision {
                                crate::domain::models::plan::PlanDecision::Approve => {
                                    if let Some(plan) = conversation.plans.get_mut(&plan_id) {
                                        plan.status = crate::domain::models::plan::PlanStatus::Executing;
                                        plan.resolved_at = Some(crate::domain::models::session_meta::now_unix());
                                    }
                                    state.pending_plan_card = None;
                                    state.focus = FocusState::Input;
                                    state.needs_redraw = true;
                                    let _ = domain_tx.send(AppEvent::PlanExecutionStarted {
                                        conversation_id: cid,
                                        plan_id,
                                    });
                                }
                                crate::domain::models::plan::PlanDecision::Reject => {
                                    if let Some(plan) = conversation.plans.get_mut(&plan_id) {
                                        plan.status = crate::domain::models::plan::PlanStatus::Rejected;
                                        plan.resolved_at = Some(crate::domain::models::session_meta::now_unix());
                                    }
                                    state.pending_plan_card = None;
                                    state.focus = FocusState::Input;
                                    state.needs_redraw = true;
                                    let _ = domain_tx.send(AppEvent::SystemNotice {
                                        conversation_id: Some(conversation.id.clone()),
                                        level: NoticeLevel::Info,
                                        message: "Plan rejected. You can provide feedback or try a different approach.".to_string(),
                                    });
                                    let conv_id = conversation.id.clone();
                                    app_state.event_bus.emit_domain(AppEvent::AgentThenSubmit {
                                        conversation_id: conv_id,
                                        text: "Plan rejected by user. Please revise the approach or ask clarifying questions before retrying.".to_string(),
                                        synthetic: true,
                                    });
                                }
                                crate::domain::models::plan::PlanDecision::Edit => {
                                    if let Some(plan) = conversation.plans.get_mut(&plan_id) {
                                        plan.status = crate::domain::models::plan::PlanStatus::Editing;
                                    }
                                    if let Some(ref pending) = state.pending_plan_card {
                                        if pending.plan_id == plan_id {
                                            let plan_snapshot = pending.plan_snapshot.clone();
                                            let result = crate::adapters::tui::editor_suspend::run_editor_on_plan(&plan_snapshot).await;
                                            let edited_plan = match result {
                                                Ok(Some(p)) => Some(p),
                                                Ok(None) => None,
                                                Err(e) => {
                                                    tracing::warn!("Plan card editor error: {}", e);
                                                    None
                                                }
                                            };
                                            app_state.event_bus.emit_domain(AppEvent::PlanCardEditCompleted {
                                                conversation_id: conversation.id.clone(),
                                                plan_id,
                                                edited_plan,
                                            });
                                        }
                                    }
                                }
                                crate::domain::models::plan::PlanDecision::AutoApproveYolo => {
                                    tracing::warn!("PlanCardResolved with AutoApproveYolo is unexpected — use PlanProposed YOLO branch instead");
                                }
                            }
                        }
                    }
                    AppEvent::PlanCardEditCompleted { conversation_id, plan_id, edited_plan } => {
                        if conversation.id != conversation_id {
                            tracing::warn!("PlanCardEditCompleted for mismatched conversation");
                        } else {
                            match edited_plan {
                                Some(new_plan) => {
                                    if let Some(stored) = conversation.plans.get_mut(&plan_id) {
                                        stored.title = new_plan.title;
                                        stored.tasks = new_plan.tasks;
                                        stored.estimated_effort = new_plan.estimated_effort;
                                        stored.status = crate::domain::models::plan::PlanStatus::Pending;
                                    }
                                    if let Some(stored) = conversation.plans.get(&plan_id) {
                                        state.pending_plan_card = Some(crate::adapters::tui::state::PendingPlanCard {
                                            conversation_id,
                                            plan_id,
                                            plan_snapshot: stored.clone(),
                                        });
                                    }
                                    state.needs_redraw = true;
                                }
                                None => {
                                    if let Some(stored) = conversation.plans.get_mut(&plan_id) {
                                        stored.status = crate::domain::models::plan::PlanStatus::Pending;
                                    }
                                    if let Some(stored) = conversation.plans.get(&plan_id) {
                                        state.pending_plan_card = Some(crate::adapters::tui::state::PendingPlanCard {
                                            conversation_id,
                                            plan_id,
                                            plan_snapshot: stored.clone(),
                                        });
                                    }
                                    let _ = domain_tx.send(AppEvent::SystemNotice {
                                        conversation_id: Some(conversation.id.clone()),
                                        level: NoticeLevel::Warning,
                                        message: "Plan edit failed (parse error). Showing original card.".to_string(),
                                    });
                                    state.needs_redraw = true;
                                }
                            }
                        }
                    }
                    AppEvent::PlanExecutionStarted { conversation_id, plan_id } => {
                        plan_runtime.clone().start(
                            conversation_id.clone(),
                            plan_id.clone(),
                            &mut conversation,
                            &app_state.event_bus,
                        );
                        let term_width = state.terminal_width;
                        let auto_open_setting = state.auto_open_on_task_plan.clone();
                        let outcome = crate::adapters::tui::task_panel_handlers::handle_plan_execution_started(
                            &mut state,
                            &conversation,
                            &conversation_id,
                            &plan_id,
                            term_width,
                            crate::adapters::tui::layout::SIDEBAR_MIN_WIDTH,
                            &auto_open_setting,
                        );
                        for notice in outcome.notices {
                            app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                conversation_id: Some(notice.conversation_id),
                                level: notice.level,
                                message: notice.message,
                            });
                        }
                    }
                    AppEvent::PlanTaskStatusChanged { conversation_id, plan_id, task_number, status: _ } => {
                        let _ = crate::adapters::tui::task_panel_handlers::handle_plan_task_status_changed(
                            &mut state,
                            &conversation,
                            &conversation_id,
                            &plan_id,
                        );
                        tracing::trace!(
                            plan = %plan_id, task = %task_number,
                            "PlanTaskStatusChanged received"
                        );
                    }
                    AppEvent::PlanCompleted { conversation_id, plan_id, completed, failed, skipped, total_elapsed_ms } => {
                        tracing::info!(
                            "Plan completed: {} completed, {} failed, {} skipped, ~{}ms",
                            completed, failed, skipped, total_elapsed_ms
                        );
                        if conversation_id == conversation.id {
                            state.task_panel_state.last_executed_plan_id = Some(plan_id.clone());
                            // PD1 (Sally A1): a real plan-level failure trumps user-set
                            // suppression — clear the flag and force the panel back open
                            // so the failure is surfaced even if the user had dismissed it.
                            if failed > 0
                                && state
                                    .task_panel_state
                                    .auto_open_suppressed_conversations
                                    .remove(&conversation.id)
                                && state.terminal_width >= crate::adapters::tui::layout::SIDEBAR_MIN_WIDTH
                            {
                                state.sidebar_visible = true;
                                state.sidebar_panel = Some(crate::domain::models::visual::PanelType::Tasks);
                                state.task_panel_state.drill_down_task = None;
                                state.task_panel_state.expanded_detail = false;
                                state.task_panel_state.detail_scroll_offset = 0;
                                state.task_panel_state.task_count = conversation
                                    .plans
                                    .get(&plan_id)
                                    .map(|p| p.tasks.len())
                                    .unwrap_or(0);
                                state.needs_redraw = true;
                                app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                    conversation_id: Some(conversation.id.clone()),
                                    level: NoticeLevel::Warning,
                                    message: "Plan failed — task panel reopened.".to_string(),
                                });
                            }
                        }
                    }
                        AppEvent::PlanCancelled { conversation_id, plan_id, cancelled_at_task } => {
                            let msg = match cancelled_at_task {
                                Some(n) => format!("Plan cancelled at task {}", n),
                                None => "Plan cancelled".to_string(),
                            };
                            tracing::info!("{}", msg);
                            if conversation_id == conversation.id {
                                state.task_panel_state.last_executed_plan_id = Some(plan_id);
                                app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                    conversation_id: Some(conversation.id.clone()),
                                    level: NoticeLevel::Info,
                                    message: msg,
                                });
                            }
                        }
                        AppEvent::PlanDeviation { conversation_id: cid, plan_id: pid, deviation_kind, original_step_count: _, current_step_count: _, changed_steps: _, summary } => {
                            let yolo = security.current_mode() == PermissionMode::Yolo;
                            if yolo {
                                app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                    conversation_id: Some(cid.clone()),
                                    level: NoticeLevel::Info,
                                    message: format!("Plan adjusted: {}", summary),
                                });
                                plan_runtime.clear_deviation_pending(&pid).await;
                                if cid == conversation.id {
                                    plan_runtime.resume_advance(
                                        &cid,
                                        &pid,
                                        &mut conversation,
                                        app_state.event_bus.as_ref(),
                                    ).await;
                                }
                            } else if cid == conversation.id {
                                state.task_panel_state.pending_deviation = Some((pid, deviation_kind));
                                state.needs_redraw = true;
                            }
                        }
                        AppEvent::SetPermissionMode(mode) => {
                        security.set_mode(mode);
                        // Update sandbox policy
                        let new_policy = crate::domain::models::SandboxPolicy::from_mode(mode, &workspace_path);
                        *app_state.sandbox_policy.write().await = new_policy;

                        // Plan mode warmup
                        if mode == PermissionMode::Plan {
                            let mut meta = tab_manager.active_tab_mut().session_meta.clone();
                            let _ = app_state.plan_manager.ensure_dir().await;
                            let plan = app_state.plan_manager.plan_file_for(&mut meta);
                            tab_manager.active_tab_mut().session_meta.plan_slug = meta.plan_slug;
                            tool_scheduler.set_plan_file(Some(plan.path.clone())).await;
                            state.plan_file_path = Some(plan.path);
                            app_state.plan_injector.as_ref().reset_reentry();
                            state.pending_plan_reminder_at_turn = Some(0);
                        } else {
                            tool_scheduler.set_plan_file(None).await;
                            state.plan_file_path = None;
                            state.pending_plan_reminder_at_turn = None;
                        }

                        let mode_str = match mode {
                            PermissionMode::Plan => "Plan",
                            PermissionMode::Normal => "Normal",
                            PermissionMode::AutoEdit => "AutoEdit",
                            PermissionMode::Yolo => "YOLO",
                        };
                        // Only capture status_before_flash if current state isn't already a Flash —
                        // prevents rapid mode switches from burying the pre-flash Idle state under
                        // another Flash as the revert target.
                        if !matches!(state.status, StatusState::Flash { .. }) {
                            state.status_before_flash = Some(state.status.clone());
                        }
                        if matches!(mode, PermissionMode::Yolo) {
                            // AC7: Yolo mode warning stays until mode changes — use u64::MAX so
                            // the tick-based Flash expiry never decrements to zero. Transitioning
                            // out of Yolo replaces status with the new mode's flash (below).
                            state.status = StatusState::Flash {
                                message: "⚠ YOLO mode active — all tools auto-approved".to_string(),
                                remaining_ms: u64::MAX,
                            };
                        } else {
                            state.status = StatusState::Flash {
                                message: format!("Permission mode: {}", mode_str),
                                remaining_ms: state.theme.timing.status_flash_ms,
                            };
                        }
                        state.needs_redraw = true;
                    }
                    AppEvent::RetryMessage { content: text, .. } => {
                        // Delayed retry arrived — start the turn now (no images on retry)
                        state.status = StatusState::Streaming;
                        let _skill_snap = skill_activator.snapshot_for_turn(&conversation.id).await;
                        let _agent_snap = agent_activator.snapshot(&conversation.id).await;
                        start_turn(
                            &text,
                            vec![],
                            &mut conversation,
                            &mut streaming,
                            &mut state,
                            &mut _active_turn,
                            &provider,
                            config,
                            &domain_tx,
                            &security,
                            &tools, &tool_scheduler,
                            &persona,
                            &workspace_path,
                            &mut session_manager,
                            &fs_storage,
                            &storage,
                                              &app_state.plan_manager, &app_state.plan_injector,
                            None,
                            _skill_snap,
                            _agent_snap,
                                                              tab_manager.reset_and_clone_turn_cancel(),
                        app_state.usage_ledger.clone()).await;
                        match render(terminal, &mut state, &conversation, &streaming, &config.model, &router.active_delegate_id(), security.as_ref(), tab_manager.tab_count(), tab_manager.active_tab_index(), Some(&tab_manager), &session_index) {
                            Ok(()) => state.needs_redraw = false,
                            Err(e) => handle_render_error(e, &mut _active_turn, &mut streaming, &mut state, terminal),
                        }
                    }
                    AppEvent::TitleGenerated { conversation_id, title } => {
                        // Update sidebar index with new title and reorder to front
                        session_index.touch(&conversation_id, Some(title.clone()), None);
                        if conversation_id == conversation.id {
                            // Active tab
                            conversation.title = title;
                            state.needs_redraw = true;
                            if let Some(prev) = _pending_save.take() {
                                let _ = prev.await;
                            }
                            let storage_ref = storage.clone();
                            let conv_clone = conversation.clone();
                            _pending_save = Some(tokio::spawn(async move {
                                match tokio::time::timeout(
                                    BACKGROUND_TASK_TIMEOUT,
                                    storage_ref.save_conversation(&conv_clone),
                                ).await {
                                    Ok(Ok(())) => {}
                                    Ok(Err(e)) => tracing::error!("Failed to persist title: {}", e),
                                    Err(_) => tracing::warn!("Background task 'title_save' timed out after {}s", BACKGROUND_TASK_TIMEOUT.as_secs()),
                                }
                            }));
                        } else if let Some(tab) = tab_manager.find_by_conversation_mut(&conversation_id) {
                            // Background tab — update its stored conversation title
                            tab.conversation.title = title;
                            if let Some(prev) = _pending_save.take() {
                                let _ = prev.await;
                            }
                            let storage_ref = storage.clone();
                            let conv_clone = tab.conversation.clone();
                            _pending_save = Some(tokio::spawn(async move {
                                match tokio::time::timeout(
                                    BACKGROUND_TASK_TIMEOUT,
                                    storage_ref.save_conversation(&conv_clone),
                                ).await {
                                    Ok(Ok(())) => {}
                                    Ok(Err(e)) => tracing::error!("Failed to persist background tab title: {}", e),
                                    Err(_) => tracing::warn!("Background tab title save timed out"),
                                }
                            }));
                            // Redraw tab bar to show the new title
                            state.needs_redraw = true;
                        }
                    }
                    AppEvent::AskUserQuestion {
                        conversation_id,
                        tool_use_id,
                        question,
                        response_tx,
                    } => {
                        // Only display question if it belongs to the active tab
                        if conversation_id == conversation.id {
                            // DF-018 inverse guard (AC6, Story 4-3b): rewind overlay active →
                            // drop the question request silently (its turn will be truncated).
                            if matches!(
                                state.focus,
                                FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Rewind))
                            ) {
                                tracing::debug!(
                                    "Dropping AskUserQuestion for conversation {} — rewind in progress",
                                    conversation_id
                                );
                                // Drop response_tx — turn gets RecvError on the other end
                            } else {
                                use crate::adapters::tui::widgets::ask_user_question::AskUserQuestionState;
                                state.ask_user_question = Some(AskUserQuestionState {
                                    tool_use_id,
                                    question,
                                    input_buffer: String::new(),
                                    cursor_position: 0,
                                    submitted_answer: None,
                                });
                                state.question_response_tx = Some(response_tx);
                                state.focus = FocusState::Overlay(OverlayType::Confirmation(
                                    ConfirmationType::Question,
                                ));
                                state.needs_redraw = true;
                            }
                        } else if let Some(_tab) = tab_manager.find_by_conversation_mut(&conversation_id) {
                            // Background tab: store for later (when tab becomes active)
                            // TODO: Add ask_user_question state to TabState for proper routing
                            tracing::warn!("AskUserQuestion for background tab {} - storing for later", conversation_id);
                            // For now, show it immediately (temporary until full per-tab state)
                            use crate::adapters::tui::widgets::ask_user_question::AskUserQuestionState;
                            state.ask_user_question = Some(AskUserQuestionState {
                                tool_use_id,
                                question,
                                input_buffer: String::new(),
                                cursor_position: 0,
                                submitted_answer: None,
                            });
                            state.question_response_tx = Some(response_tx);
                            state.focus = FocusState::Overlay(OverlayType::Confirmation(
                                ConfirmationType::Question,
                            ));
                            state.needs_redraw = true;
                        }
                        // Silently drop if conversation not found (tab closed)
                    }
                    AppEvent::RecoveryPrompt { title, token_count, .. } => {
                        let msg = format!(
                            "\u{2139} Recovered: '{}' (partial response, {} tokens). [Enter/y] continue [n] new",
                            title, token_count,
                        );
                        let fb = FeedbackBlock {
                            id: "recovery".to_string(),
                            level: FeedbackLevel::Info,
                            message: msg,
                            actions: vec![
                                FeedbackAction::Custom("[Enter] continue".to_string()),
                                FeedbackAction::StartFresh,
                            ],
                        };
                        state.feedback_blocks.insert("recovery".to_string(), fb);
                        state.active_feedback_id = Some("recovery".to_string());
                        state.focus = FocusState::Chat;
                        state.needs_redraw = true;
                    }
                    AppEvent::CrossSearchResultsReady {
                        query,
                        results,
                        truncated_by_count,
                        truncated_by_time,
                    } => {
                        // Story 4-4 AC5 stale-result guard — delegated to the
                        // `apply_cross_search_results` helper so the event loop
                        // and tests exercise the same code path
                        // (third-audit Fix R4).
                        let outcome = apply_cross_search_results(
                            &mut state,
                            query,
                            results,
                            truncated_by_count,
                            truncated_by_time,
                        );
                        if matches!(outcome, CrossSearchResultsOutcome::Applied) {
                            state.needs_redraw = true;
                        }
                    }
                    AppEvent::PeekHighlightExpired { tab_id: _ } => {
                        // Story 4-4 AC6 — peek highlight window has elapsed.
                        // Clear the peek on the active tab's search state.
                        // `tab_id` is passed for future multi-tab routing but
                        // currently all peeks are applied to the active tab.
                        state.search_state.peek_highlight = None;
                        state.needs_redraw = true;
                    }
                    AppEvent::SkillsDiscovered { count, warnings } => {
                        // Story 16-0 AC1: registry was already written to the shared
                        // Arc<RwLock<SkillRegistry>> by the background scan task.
                        // Both TuiState and SkillActivator share the same Arc, so
                        // no clone/set_registry/replace_skill_registry needed.
                        state.refresh_skill_name_cache().await;
                        state.needs_redraw = true;
                        // AC6: if the user already has `/` autocomplete open when
                        // discovery finishes, refresh suggestions so newly
                        // discovered skills appear immediately.
                        if state.autocomplete.active {
                            last_autocomplete_filter.clear();
                            populate_autocomplete_suggestions(
                                &mut state,
                                &mut command_registry,
                                &workspace_path,
                            )
                            .await;
                        }
                        if count > 0 {
                            app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                conversation_id: None,
                                level: NoticeLevel::Info,
                                message: format!("Loaded {} skills", count),
                            });
                        }
                        if warnings > 0 {
                            app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                conversation_id: None,
                                level: NoticeLevel::Warning,
                                message: format!(
                                    "{} skills failed validation (see log)",
                                    warnings
                                ),
                            });
                        }
                    }
                    AppEvent::AgentsDiscovered { count, warnings } => {
                        {
                            let mut slot = agent_registry_slot.lock().await;
                            if let Some(registry) = slot.take() {
                                state.replace_agent_registry(registry);
                            }
                        }
                        state.refresh_agent_suggestions();
                        state.needs_redraw = true;
                        if state.autocomplete.active {
                            last_autocomplete_filter.clear();
                            populate_autocomplete_suggestions(
                                &mut state,
                                &mut command_registry,
                                &workspace_path,
                            )
                            .await;
                        }
                        if count > 0 {
                            app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                conversation_id: None,
                                level: NoticeLevel::Info,
                                message: format!("Discovered {} custom agent(s) in .claude/agents/", count),
                            });
                        }
                        if warnings > 0 {
                            app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                conversation_id: None,
                                level: NoticeLevel::Warning,
                                message: format!(
                                    "{} agent file(s) excluded due to validation errors — see logs",
                                    warnings
                                ),
                            });
                        }
                        // Patch #11: retry pending agent activation queued before discovery
                        if let Some((pending_name, then_submit)) =
                            state.pending_agent_activation.take()
                        {
                            if state.agent_registry.find(&pending_name).is_some() {
                                let conv_id = conversation.id.clone();
                                let prior = agent_activator.active_agent_name(&conv_id).await;
                                match agent_activator.activate(&conv_id, &pending_name).await {
                                    Ok(_active) => {
                                        state.active_agent_name = Some(pending_name.clone());
                                        let notice_msg = if let Some(prior_name) = prior {
                                            format!(
                                                "Active agent: {} (was {})",
                                                pending_name, prior_name
                                            )
                                        } else {
                                            format!("Active agent: {}", pending_name)
                                        };
                                        app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                            conversation_id: Some(conv_id.clone()),
                                            level: crate::domain::models::NoticeLevel::Info,
                                            message: notice_msg,
                                        });
                                        if let Some(text) = then_submit {
                                            app_state.event_bus.emit_domain(AppEvent::AgentThenSubmit {
                                                conversation_id: conv_id,
                                                text,
                                                synthetic: false,
                                            });
                                        }
                                    }
                                    Err(e) => {
                                        let level = match e {
                                            crate::adapters::agent_activation::AgentActivationError::FileMissing { .. }
                                            | crate::adapters::agent_activation::AgentActivationError::FileTooLarge { .. } => {
                                                crate::domain::models::NoticeLevel::Error
                                            }
                                            _ => crate::domain::models::NoticeLevel::Warning,
                                        };
                                        if matches!(
                                            e,
                                            crate::adapters::agent_activation::AgentActivationError::OutsideWorkspace(..)
                                        ) {
                                            state.agent_registry.remove(&pending_name);
                                        }
                                        app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                            conversation_id: Some(conv_id),
                                            level,
                                            message: format!("Agent activation failed: {}", e),
                                        });
                                    }
                                }
                                state.needs_redraw = true;
                            } else {
                                app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                    conversation_id: Some(conversation.id.clone()),
                                    level: crate::domain::models::NoticeLevel::Warning,
                                    message: format!(
                                        "Unknown agent: '{}'. Type @Agents/ to see available agents.",
                                        pending_name
                                    ),
                                });
                                state.needs_redraw = true;
                            }
                        }
                    }
                    AppEvent::AgentThenSubmit { conversation_id, text, synthetic } => {
                        if conversation.id == *conversation_id {
                            if streaming.is_streaming {
                                tracing::debug!(
                                    "AgentThenSubmit deferred: still streaming, queuing for dispatch after turn completes"
                                );
                                state.pending_agent_then_submit = Some((text, synthetic));
                            } else {
                                let _skill_snap = skill_activator.snapshot_for_turn(&conversation.id).await;
                                let _agent_snap = agent_activator.snapshot(&conversation.id).await;
                                start_turn_inner(
                                &text,
                                Vec::new(),
                                synthetic,
                                &mut conversation,
                                &mut streaming,
                                &mut state,
                                &mut _active_turn,
                                &provider,
                                config,
                                &domain_tx,
                                &security,
                                &tools, &tool_scheduler,
                                &persona,
                                &workspace_path,
                                &mut session_manager,
                                &fs_storage,
                                &storage,
                                              &app_state.plan_manager, &app_state.plan_injector,
                                None,
                                _skill_snap,
                                _agent_snap,
                                                                  tab_manager.reset_and_clone_turn_cancel(),
                            app_state.usage_ledger.clone()).await;
                            }
                        }
                    }
                    AppEvent::AskActivateSkill { conversation_id, name, arguments } => {
                        if name.starts_with("__deactivate_all__") {
                            let deactivated = skill_activator.deactivate_all(&conversation_id).await;
                            for active in &deactivated {
                                security.remove_active_skill_dir(&active.directory);
                            }
                            state.active_skill_count = skill_activator
                                .active_count(&conversation_id)
                                .await;
                            let msg = if deactivated.is_empty() {
                                "No active skills to deactivate".to_string()
                            } else {
                                let names: Vec<&str> = deactivated.iter().map(|s| s.name.as_str()).collect();
                                format!("Deactivated {} skill(s): [{}]", names.len(), names.join(", "))
                            };
                            app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                conversation_id: Some(conversation_id.clone()),
                                level: NoticeLevel::Info,
                                message: msg,
                            });
                        } else if let Some(stripped) = name.strip_prefix("__deactivate__") {
                            match skill_activator.deactivate(&conversation_id, stripped).await {
                                Some(deactivated) => {
                                    security.remove_active_skill_dir(&deactivated.directory);
                                    state.active_skill_count = skill_activator
                                        .active_count(&conversation_id)
                                        .await;
                                    app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                        conversation_id: Some(conversation_id.clone()),
                                        level: NoticeLevel::Info,
                                        message: format!("Deactivated skill '{}'", deactivated.name),
                                    });
                                }
                                None => {
                                    app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                        conversation_id: Some(conversation_id.clone()),
                                        level: NoticeLevel::Info,
                                        message: format!("Skill '{}' is not active", stripped),
                                    });
                                }
                            }
                        } else {
                            let def = skill_activator.lookup_skill(&name).await;
                            let def = match def {
                                Some(d) => d,
                                None => {
                                    app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                        conversation_id: Some(conversation_id.clone()),
                                        level: NoticeLevel::Error,
                                        message: format!("Skill '{}' not found", name),
                                    });
                                    state.needs_redraw = true;
                                    continue;
                                }
                            };

                            let needs_trust = def.source != crate::domain::models::SkillSource::GlobalAgents
                                && !skill_activator.is_trusted(&conversation_id, &def.file).await;

                            if needs_trust {
                                let pending = crate::adapters::tui::state::PendingSkillActivation {
                                    skill_name: name.clone(),
                                    skill_file: def.file.clone(),
                                    arguments: arguments.clone(),
                                    conversation_id: conversation_id.clone(),
                                };

                                if state.pending_activation.is_some() {
                                    tracing::error!(
                                        skill = %name,
                                        existing = ?state.pending_activation.as_ref().map(|p| &p.skill_name),
                                        "second user-driven skill trust prompt while one is already pending; dropping"
                                    );
                                    app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                        conversation_id: Some(conversation_id.clone()),
                                        level: NoticeLevel::Info,
                                        message: format!(
                                            "Skill '{}' activation dropped — resolve the current trust prompt first",
                                            name
                                        ),
                                    });
                                } else {
                                    state.pending_activation = Some(pending);
                                    state.focus = FocusState::Overlay(OverlayType::Confirmation(
                                        ConfirmationType::SkillTrust,
                                    ));
                                    state.skill_trust_inspect_mode = false;
                                    state.needs_redraw = true;
                                }
                                continue;
                            }

                            // H6: Re-validate that the active conversation still matches the
                            // one the user was prompted for. The user may have switched tabs
                            // during `rx.await` — if so, skip activation + turn kickoff
                            // (start_turn would run against the wrong conversation).
                            if conversation.id != conversation_id {
                                app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                    conversation_id: Some(conversation_id.clone()),
                                    level: NoticeLevel::Info,
                                    message: format!(
                                        "Skill '{}' trust recorded, but activation was skipped because the conversation changed during the prompt",
                                        name
                                    ),
                                });
                                state.needs_redraw = true;
                                continue;
                            }

                            match skill_activator.activate(&def, arguments.clone(), &conversation_id, 0).await {
                                Ok(active) => {
                                    security.add_active_skill_dir(active.directory.clone());
                                    state.active_skill_count = skill_activator
                                        .active_count(&conversation_id)
                                        .await;
                                    let args_trimmed = arguments.trim();
                                    let user_msg = format!("ARGUMENTS: {}", args_trimmed);
                                    let snap = skill_activator
                                        .snapshot_for_turn(&conversation.id)
                                        .await;
                                    let agent_snap = agent_activator.snapshot(&conversation.id).await;
                                    start_turn(
                                        &user_msg,
                                        Vec::new(),
                                        &mut conversation,
                                        &mut streaming,
                                        &mut state,
                                        &mut _active_turn,
                                        &provider,
                                        config,
                                        &domain_tx,
                                        &security,
                                        &tools, &tool_scheduler,
                                        &persona,
                                        &workspace_path,
                                        &mut session_manager,
                                        &fs_storage,
                                        &storage,
                                              &app_state.plan_manager, &app_state.plan_injector,
                                        None,
                                        snap,
                                        agent_snap,
                                                                          tab_manager.reset_and_clone_turn_cancel(),
                                    app_state.usage_ledger.clone()).await;
                                    app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                        conversation_id: Some(conversation_id.clone()),
                                        level: NoticeLevel::Info,
                                        message: format!("Skill '{}' activated", active.name),
                                    });
                                }
                                Err(e) => {
                                    app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                        conversation_id: Some(conversation_id.clone()),
                                        level: NoticeLevel::Error,
                                        message: e.to_string(),
                                    });
                                }
                            }
                        }
                        state.needs_redraw = true;
                    }
                    AppEvent::SkillTrustPrompt { skill_name, skill_file, response_tx } => {
                        // Model-driven activation path: the SkillActivator emits this event
                        // when a workspace-tier skill requires trust (Story 5-2 AC9).
                        // Promote the carried oneshot sender into pending_skill_trust so
                        // the existing y/n/i input actions consume it.
                        let trust_state = crate::adapters::tui::state::SkillTrustState {
                            skill_name,
                            skill_file,
                            response_tx,
                            inspect_content: None,
                        };
                        if state.pending_skill_trust.is_some() {
                            state.skill_trust_queue.push_back(trust_state);
                        } else {
                            state.pending_skill_trust = Some(trust_state);
                            state.focus = FocusState::Overlay(OverlayType::Confirmation(
                                ConfirmationType::SkillTrust,
                            ));
                        }
                        state.needs_redraw = true;
                    }
                    AppEvent::CompleteSkillActivation {
                        conversation_id,
                        skill_name,
                        skill_file,
                        arguments,
                        trusted,
                    } => {
                        if !trusted {
                            app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                conversation_id: Some(conversation_id.clone()),
                                level: NoticeLevel::Info,
                                message: format!(
                                    "Skill '{}' not trusted — activation declined",
                                    skill_name
                                ),
                            });
                            state.needs_redraw = true;
                            continue;
                        }

                        if conversation.id != conversation_id {
                            app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                conversation_id: Some(conversation_id.clone()),
                                level: NoticeLevel::Info,
                                message: format!(
                                    "Skill '{}' trust recorded, but activation was skipped because the conversation changed during the prompt",
                                    skill_name
                                ),
                            });
                            state.needs_redraw = true;
                            continue;
                        }

                        let def = match skill_activator.lookup_skill(&skill_name).await {
                            Some(d) => d,
                            None => {
                                app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                    conversation_id: Some(conversation_id.clone()),
                                    level: NoticeLevel::Error,
                                    message: format!(
                                        "Skill '{}' no longer available (removed during trust prompt)",
                                        skill_name
                                    ),
                                });
                                state.needs_redraw = true;
                                continue;
                            }
                        };

                        if def.file != skill_file {
                            app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                conversation_id: Some(conversation_id.clone()),
                                level: NoticeLevel::Info,
                                message: format!(
                                    "Skill '{}' changed location during trust prompt — activation skipped",
                                    skill_name
                                ),
                            });
                            state.needs_redraw = true;
                            continue;
                        }

                        match skill_activator.activate(&def, arguments.clone(), &conversation_id, 0).await {
                            Ok(active) => {
                                security.add_active_skill_dir(active.directory.clone());
                                state.active_skill_count = skill_activator
                                    .active_count(&conversation_id)
                                    .await;
                                let args_trimmed = arguments.trim();
                                let user_msg = format!("ARGUMENTS: {}", args_trimmed);
                                let snap = skill_activator
                                    .snapshot_for_turn(&conversation.id)
                                    .await;
                                let agent_snap = agent_activator.snapshot(&conversation.id).await;
                                start_turn(
                                    &user_msg,
                                    Vec::new(),
                                    &mut conversation,
                                    &mut streaming,
                                    &mut state,
                                    &mut _active_turn,
                                    &provider,
                                    config,
                                    &domain_tx,
                                    &security,
                                    &tools, &tool_scheduler,
                                    &persona,
                                    &workspace_path,
                                    &mut session_manager,
                                    &fs_storage,
                                    &storage,
                                              &app_state.plan_manager, &app_state.plan_injector,
                                    None,
                                    snap,
                                    agent_snap,
                                                                      tab_manager.reset_and_clone_turn_cancel(),
                                app_state.usage_ledger.clone()).await;
                                app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                    conversation_id: Some(conversation_id.clone()),
                                    level: NoticeLevel::Info,
                                    message: format!("Skill '{}' activated", active.name),
                                });
                            }
                            Err(e) => {
                                app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                                    conversation_id: Some(conversation_id.clone()),
                                    level: NoticeLevel::Error,
                                    message: format!("Skill '{}' activation failed: {}", skill_name, e),
                                });
                            }
                        }
                        state.needs_redraw = true;
                    }
                    _ => {
                        state.needs_redraw = true;
                    }
                }
            }

            // Branch 5: Tool progress events for live stdout tail. Story 16.9.
            // When progress_rx is None (live_tail OFF), this branch is a
            // std::future::pending() — it never fires (no idle wakeups).
            event = async {
                match &mut progress_rx {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                let Some(event) = event else {
                    // All senders dropped — disable this branch to prevent
                    // spinning on the closed channel.
                    progress_rx = None;
                    continue;
                };
                use crate::domain::events::ToolProgressEvent;
                match event {
                    ToolProgressEvent::Counter {
                        tool_use_id,
                        k,
                        n,
                    } => {
                        let found_id = tab_manager
                            .find_tab_with_pending_tool(&tool_use_id)
                            .map(|tab| {
                                tab.reducer.set_progress(&tool_use_id, k, n);
                                update_streaming_mirror(
                                    &tab.reducer,
                                    &mut tab.streaming,
                                );
                                tab.id
                            });
                        if found_id.is_none() {
                            tracing::debug!(
                                "stale Counter progress for {} — tool no longer pending",
                                tool_use_id
                            );
                        }
                        if found_id.is_some_and(|id| id == tab_manager.active_tab_id()) {
                            state.needs_redraw = true;
                        }
                    }
                    ToolProgressEvent::Tail {
                        tool_use_id,
                        text,
                    } => {
                        let found_id = tab_manager
                            .find_tab_with_pending_tool(&tool_use_id)
                            .map(|tab| {
                                tab.reducer.set_tail(&tool_use_id, text);
                                update_streaming_mirror(
                                    &tab.reducer,
                                    &mut tab.streaming,
                                );
                                tab.id
                            });
                        if found_id.is_none() {
                            tracing::debug!(
                                "stale Tail progress for {} — tool no longer pending",
                                tool_use_id
                            );
                        }
                        if found_id.is_some_and(|id| id == tab_manager.active_tab_id()) {
                            state.needs_redraw = true;
                        }
                    }
                }
            }

            // Branch 3: Render tick (250ms interval with needs_redraw optimization)
            _ = tick_interval.tick() => {
                // Story 7.2 AC4: poll pending health check completion
                if let Some((ref pid, ref mid, ref mut handle)) = pending_health_check {
                    match handle.now_or_never() {
                        Some(Ok(Ok(()))) => {
                            app_state.provider_registry.update_health(pid, true);
                            state.model_selector.connecting = None;
                            let saved_pid = pid.clone();
                            let saved_mid = mid.clone();
                            pending_health_check = None;
                            complete_model_switch(
                                &mut state,
                                &router,
                                &app_state,
                                &conversation,
                                &saved_pid,
                                &saved_mid,
                            ).await;
                        }
                        Some(Ok(Err(e))) => {
                            state.model_selector.connecting = None;
                            pending_health_check = None;
                            let fb = crate::domain::models::FeedbackBlock {
                                id: generate_conversation_id(),
                                level: crate::domain::models::FeedbackLevel::Warning,
                                message: format!("Connection failed: {}", e),
                                actions: vec![],
                            };
                            state.feedback_blocks.insert(fb.id.clone(), fb);
                            state.needs_redraw = true;
                        }
                        Some(Err(e)) => {
                            state.model_selector.connecting = None;
                            pending_health_check = None;
                            let fb = crate::domain::models::FeedbackBlock {
                                id: generate_conversation_id(),
                                level: crate::domain::models::FeedbackLevel::Warning,
                                message: format!("Health check panicked: {}", e),
                                actions: vec![],
                            };
                            state.feedback_blocks.insert(fb.id.clone(), fb);
                            state.needs_redraw = true;
                        }
                        None => {
                            state.needs_redraw = true;
                        }
                    }
                }

                // Update elapsed_ms for Executing state each tick
                let tick_ms = state.theme.timing.tick_interval_ms;
                if let StatusState::Executing { elapsed_ms, .. } = &mut state.status {
                    *elapsed_ms += tick_ms;
                    state.needs_redraw = true;
                }

                // PD2: keep Running-task elapsed display live in the Tasks panel.
                // Without this tick-driven redraw, `task.elapsed_ms()` for Running
                // tasks (which calls `now_unix_ms()` each render) only refreshes
                // on `PlanTaskStatusChanged` events — between status changes the
                // counter visibly freezes. Piggybacks on the existing 4Hz tick.
                if state.sidebar_visible
                    && state.sidebar_panel
                        == Some(crate::domain::models::visual::PanelType::Tasks)
                    && crate::adapters::tui::task_panel_handlers::any_plan_has_running_task(
                        &conversation,
                    )
                {
                    state.needs_redraw = true;
                }

                // Flash message expiry: decrement remaining_ms each tick.
                // remaining_ms == u64::MAX is a sticky sentinel (used by YOLO warning, AC7) —
                // it never expires; only a mode change or CancelOrQuit replaces it.
                if let StatusState::Flash { remaining_ms, .. } = &mut state.status {
                    if *remaining_ms == u64::MAX {
                        // sticky — do nothing
                    } else if *remaining_ms <= tick_ms {
                        // Flash expired — revert to previous status or Idle
                        state.status = state.status_before_flash.take().unwrap_or(StatusState::Idle);
                        state.needs_redraw = true;
                    } else {
                        *remaining_ms -= tick_ms;
                    }
                }

                // Which-key timeout check: auto-dismiss if expired (AC6)
                if state.which_key.active && state.which_key.is_timed_out(state.theme.timing.which_key_timeout_ms) {
                    let prev = state.which_key.dismiss();
                    if let Some(focus) = prev {
                        state.focus = focus;
                    }
                    state.needs_redraw = true;
                }

                if state.needs_redraw {
                                        match render(terminal, &mut state, &conversation, &streaming, &config.model, &router.active_delegate_id(), security.as_ref(), tab_manager.tab_count(), tab_manager.active_tab_index(), Some(&tab_manager), &session_index) {
                                            Ok(()) => state.needs_redraw = false,
                                            Err(e) => handle_render_error(e, &mut _active_turn, &mut streaming, &mut state, terminal),
                                        }
                                    }

                                    // Story 6-2a: if a plan task was running, advance the runtime.
                                    // Guard: only call on_turn_complete when a NEW assistant message
                                    // has arrived since the task started. Without this guard, the
                                    // 250ms render tick repeatedly classifies the same stale message
                                    // (the plan proposal) and instantly completes every task.
                                    let executing_plan_info: Option<(String, u32)> = conversation
                                        .plans
                                        .values()
                                        .find(|p| p.status == PlanStatus::Executing)
                                        .and_then(|p| {
                                            p.tasks.iter().find(|t| t.status == PlanTaskStatus::Running).map(|t| (p.id.clone(), t.number))
                                        });
                                    if let Some((pid, task_num)) = executing_plan_info {
                                        let assistant_count = conversation.messages.iter().filter(|m| m.role == MessageRole::Assistant).count();
                                        if plan_runtime.can_complete_turn(&pid, assistant_count) && !streaming.is_streaming {
                                            let conv_id = conversation.id.clone();
                                            let (msg_text, tool_count, any_success, token_count, stop_reason) = conversation
                                                .messages
                                                .iter()
                                                .rev()
                                                .find(|m| m.role == MessageRole::Assistant)
                                                .map(|m| {
                                                    let tc = m.tool_calls.len() as u32;
                                                    let success = m.tool_calls.iter().any(|tc_info| {
                                                        tc_info.result.as_ref().is_some_and(|r| !r.is_error)
                                                    });
                                                    (m.content.clone(), tc, success, m.token_count, m.stop_reason.clone())
                                                })
                                                .unwrap_or_else(|| (String::new(), 0, false, None, None));
                                            let outcome = crate::domain::services::plan_runtime::PlanRuntime::classify_outcome(
                                                &msg_text, tool_count, any_success, stop_reason,
                                            );
                                            let outcome = match &outcome {
                                                crate::domain::services::plan_runtime::TaskTurnOutcome::Success { .. } => {
                                                    crate::domain::services::plan_runtime::TaskTurnOutcome::Success {
                                                        result_text: msg_text,
                                                        tool_call_count: tool_count,
                                                        token_count,
                                                    }
                                                }
                                                other => other.clone(),
                                            };
                                            plan_runtime.on_turn_complete(
                                                &conv_id,
                                                &pid,
                                                task_num,
                                                outcome,
                                                &mut conversation,
                                                &app_state.event_bus,
                                            ).await;
                                        }
                                    }
                                }

            // Branch 4: Active task monitoring (placeholder for future stories)
            // Currently a no-op future that never resolves
        }

        if state.should_quit {
            break;
        }
    }

    // Await any in-flight save before shutdown persist
    if let Some(prev) = _pending_save.take() {
        let _ = prev.await;
    }

    // Persist active tab on shutdown with clean_exit = true (graceful shutdown)
    if !conversation.messages.is_empty() {
        conversation.updated_at = crate::domain::models::session_meta::now_unix();
        match tokio::time::timeout(
            std::time::Duration::from_secs(2),
            fs_storage.save_conversation_with_exit(&conversation, true),
        )
        .await
        {
            Ok(Ok(())) => {
                tracing::info!("Session persisted on shutdown (clean_exit=true)");
            }
            Ok(Err(e)) => {
                tracing::error!("Failed to persist session on shutdown: {}", e);
            }
            Err(_) => {
                tracing::warn!(
                    "Session save timed out on shutdown (>2s), proceeding with teardown"
                );
            }
        }
    }
    // Persist all background (non-active) tabs on shutdown
    let active_conv_id = conversation.id.clone();
    for tab in tab_manager.tabs() {
        if tab.conversation.id != active_conv_id && !tab.conversation.messages.is_empty() {
            let mut bg_conv = tab.conversation.clone();
            bg_conv.updated_at = crate::domain::models::session_meta::now_unix();
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                fs_storage.save_conversation_with_exit(&bg_conv, true),
            )
            .await;
        }
    }

    Ok(())
}

/// Save the active-tab proxy state into TabManager before switching tabs.
/// Re-scan the active conversation for search matches and apply the calm-jump
/// rule (Story 4-4 AC2 amendment). Called from event-loop arms that mutate
/// `state.search_state.query` (keystrokes in Typing sub-state, Ctrl+U clear,
/// and return-from-Navigating transitions).
///
/// Calm-jump rule:
///   1. Jump to match 0 only if match count transitioned 0 → ≥1
///   2. Jump to match 0 only if the previously focused match no longer exists
///   3. Otherwise preserve the viewport (no yo-yo)
fn apply_search_rescan(conversation: &Conversation, state: &mut TuiState) {
    use crate::adapters::tui::widgets::chat_pane;
    use crate::domain::services::search::find_matches;

    // Story 4-4 Task 3.3 — debounce guard: skip the scan if the last scan
    // happened less than 30 ms ago AND the query length did not change.
    // This avoids burning CPU on held-key repeats without losing accuracy on
    // normal typing (where length changes every keystroke).
    let prev_query_len = state.search_state.last_query_len;
    let cur_query_len = state.search_state.query.chars().count();
    if let Some(last) = state.search_state.last_search_instant {
        if last.elapsed() < std::time::Duration::from_millis(30) && prev_query_len == cur_query_len
        {
            return;
        }
    }
    state.search_state.last_query_len = cur_query_len;

    let prev_focused = state
        .search_state
        .matches
        .get(state.search_state.focused_match_index)
        .cloned();
    let prev_was_empty = state.search_state.matches.is_empty();

    let new_matches = find_matches(conversation, &state.search_state.query);
    let new_is_empty = new_matches.is_empty();
    let prev_focused_still_valid = match &prev_focused {
        Some(f) => new_matches.contains(f),
        None => false,
    };
    state.search_state.matches = new_matches;
    state.search_state.last_search_instant = Some(std::time::Instant::now());

    let should_jump = (prev_was_empty && !new_is_empty) || !prev_focused_still_valid;
    if should_jump && !state.search_state.matches.is_empty() {
        state.search_state.focused_match_index = 0;
        let target_msg = state.search_state.matches[0].message_index;
        state.scroll_snapshot = chat_pane::find_scroll_offset_for_message(
            target_msg,
            &state.message_boundaries,
            state.total_content_height,
            state.viewport_height as usize,
        );
        state.auto_snapshot = state.scroll_snapshot == 0;
    }
    state.needs_redraw = true;
}

/// Action returned by `apply_cross_search_query_change` so the event loop
/// knows whether to spawn an async scan task or skip scanning entirely
/// (Story 4-4 AC5 + third-audit Fix R4).
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrossSearchScanAction {
    /// Query is ≥ 2 chars — the event loop should spawn `run_cross_search`
    /// with the carried query string.
    Spawn { query: String },
    /// Query is < 2 chars — `state.cross_search.results` has already been
    /// cleared in-place by this helper. No scan should be spawned.
    Cleared,
}

/// Guard helper for the `CrossSearchQueryChanged` input action. Encapsulates
/// the "≥ 2 chars → scan, else clear" decision so the event loop and the
/// tests call the SAME logic — no more tautological copies of the guard
/// (third-audit Fix R4).
#[doc(hidden)]
pub fn apply_cross_search_query_change(state: &mut TuiState) -> CrossSearchScanAction {
    if state.cross_search.query.chars().count() >= 2 {
        state.cross_search.running = true;
        CrossSearchScanAction::Spawn {
            query: state.cross_search.query.clone(),
        }
    } else {
        state.cross_search.results.clear();
        state.cross_search.selected = 0;
        state.cross_search.truncated_by_count = false;
        state.cross_search.truncated_by_time = false;
        state.cross_search.running = false;
        CrossSearchScanAction::Cleared
    }
}

/// Story 4-4 AC12 + third-audit Fix R6 — commit the pre-staged export
/// content when the user confirms the overwrite overlay with `y`.
///
/// Atomic write: tmp → fsync → rename, all routed through
/// `tokio::task::spawn_blocking` so the event loop is never held on disk
/// latency. Takes `state.pending_export` and clears it regardless of
/// success/failure. Sets a success or error flash and returns focus to Chat.
///
/// Extracted as `pub` so integration tests can exercise the full
/// confirmation path against a real tempdir workspace.
#[doc(hidden)]
pub async fn apply_confirm_export_overwrite(state: &mut TuiState) {
    if let Some((target_path, content)) = state.pending_export.take() {
        let target_for_msg = target_path.clone();
        let write_result = tokio::task::spawn_blocking(move || {
            use std::io::Write as _;
            let tmp_path = target_path.with_extension("md.tmp");
            let res: std::io::Result<()> = (|| {
                let mut f = std::fs::File::create(&tmp_path)?;
                f.write_all(content.as_bytes())?;
                f.sync_all()?;
                drop(f);
                std::fs::rename(&tmp_path, &target_path)?;
                Ok(())
            })();
            if res.is_err() {
                let _ = std::fs::remove_file(&tmp_path);
            }
            res
        })
        .await;
        match write_result {
            Ok(Ok(())) => {
                state.status = StatusState::Flash {
                    message: format!("Overwrote {}", target_for_msg.display()),
                    remaining_ms: 3000,
                };
            }
            Ok(Err(e)) => {
                state.status = StatusState::Flash {
                    message: format!("Export failed: {}", e),
                    remaining_ms: 3000,
                };
            }
            Err(join_err) => {
                state.status = StatusState::Flash {
                    message: format!("Export failed: {}", join_err),
                    remaining_ms: 3000,
                };
            }
        }
    }
    state.focus = FocusState::Chat;
    state.needs_redraw = true;
}

/// Story 4-4 AC12 + third-audit Fix R6 — drop the pre-staged export content
/// when the user dismisses the overwrite overlay with `n` or `Esc`.
///
/// Clears `state.pending_export`, sets a "cancelled" flash, and returns
/// focus to Chat. The original file on disk is never touched by this path.
#[doc(hidden)]
pub fn apply_cancel_export_overwrite(state: &mut TuiState) {
    state.pending_export = None;
    state.status = StatusState::Flash {
        message: "Export cancelled".to_string(),
        remaining_ms: 2000,
    };
    state.focus = FocusState::Chat;
    state.needs_redraw = true;
}

/// Outcome returned by `apply_cross_search_results` so callers know whether
/// the incoming scan results were applied or dropped as stale (Story 4-4 AC5
/// stale-result guard + third-audit Fix R4).
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrossSearchResultsOutcome {
    /// Results matched the current query and were written into state.
    Applied,
    /// Results were for an older query; discarded silently.
    DiscardedStale,
}

/// Handler helper for the `AppEvent::CrossSearchResultsReady` event.
/// Applies the stale-result guard: if the incoming `query` still matches
/// `state.cross_search.query`, the results are written into state;
/// otherwise they're dropped.
#[doc(hidden)]
pub fn apply_cross_search_results(
    state: &mut TuiState,
    query: String,
    results: Vec<crate::domain::services::cross_search::CrossSearchResult>,
    truncated_by_count: bool,
    truncated_by_time: bool,
) -> CrossSearchResultsOutcome {
    if state.cross_search.query != query {
        return CrossSearchResultsOutcome::DiscardedStale;
    }
    state.cross_search.results = results;
    state.cross_search.truncated_by_count = truncated_by_count;
    state.cross_search.truncated_by_time = truncated_by_time;
    state.cross_search.running = false;
    if state.cross_search.results.is_empty() {
        state.cross_search.selected = 0;
    } else {
        state.cross_search.selected = state
            .cross_search
            .selected
            .min(state.cross_search.results.len() - 1);
    }
    CrossSearchResultsOutcome::Applied
}

/// Advance or reverse the focused search match, wrapping at boundaries.
/// Emits a "Wrapped to top" / "Wrapped to bottom" flash (800 ms) when the
/// index wraps past 0 or `matches.len() - 1` (Story 4-4 AC3 amendment Fix 3).
///
/// `delta`: +1 for `n` (next), -1 for `N` (previous).
fn apply_search_navigate(state: &mut TuiState, delta: i32) {
    use crate::adapters::tui::widgets::chat_pane;
    if state.search_state.matches.is_empty() {
        state.needs_redraw = true;
        return;
    }
    let len = state.search_state.matches.len();
    let prev = state.search_state.focused_match_index;
    let new_idx = if delta > 0 {
        (prev + 1) % len
    } else {
        (prev + len - 1) % len
    };
    state.search_state.focused_match_index = new_idx;
    let target_msg = state.search_state.matches[new_idx].message_index;
    state.scroll_snapshot = chat_pane::find_scroll_offset_for_message(
        target_msg,
        &state.message_boundaries,
        state.total_content_height,
        state.viewport_height as usize,
    );
    state.auto_snapshot = state.scroll_snapshot == 0;

    // Wrap-around flash (only fires when len > 1 — a single match never wraps visibly).
    if len > 1 {
        let wrapped_forward = delta > 0 && prev == len - 1 && new_idx == 0;
        let wrapped_backward = delta < 0 && prev == 0 && new_idx == len - 1;
        if wrapped_forward {
            state.status_before_flash = Some(state.status.clone());
            state.status = StatusState::Flash {
                message: "Wrapped to top".to_string(),
                remaining_ms: 800,
            };
        } else if wrapped_backward {
            state.status_before_flash = Some(state.status.clone());
            state.status = StatusState::Flash {
                message: "Wrapped to bottom".to_string(),
                remaining_ms: 800,
            };
        }
    }
    state.needs_redraw = true;
}

/// Story 4-4 AC8: toggle a bookmark on the currently focused message.
///
/// Resolves the target via `find_message_index_from_scroll_offset`, mutates
/// the in-memory mirror on the active tab's `session_meta.bookmarks`, then
/// atomically persists `meta.json` via `save_session_meta`. If the disk save
/// fails, the in-memory change is rolled back and an error flash is shown
/// (AC8 optimistic-UI contract).
///
/// Rejects tool-call / tool-result messages with a flash — bookmarks only
/// apply to user/assistant messages per Dev Notes § Bookmarkable Message
/// Types.
async fn apply_bookmark_toggle(
    tab_manager: &mut TabManager,
    state: &mut TuiState,
    conversation: &Conversation,
    fs_storage: &crate::adapters::filesystem::FileSystemStorage,
) {
    use crate::adapters::tui::widgets::chat_pane::find_message_index_from_scroll_offset;

    if conversation.messages.is_empty() {
        state.status = StatusState::Flash {
            message: "No messages to bookmark".to_string(),
            remaining_ms: 2000,
        };
        state.needs_redraw = true;
        return;
    }
    if state.message_boundaries.is_empty() {
        state.status = StatusState::Flash {
            message: "Chat pane not ready — try again after first render".to_string(),
            remaining_ms: 2000,
        };
        state.needs_redraw = true;
        return;
    }

    let target_idx = find_message_index_from_scroll_offset(
        state.auto_snapshot,
        state.scroll_snapshot,
        &state.message_boundaries,
        state.total_content_height,
        state.viewport_height as usize,
        conversation.messages.len(),
    );

    // Guard: bookmark only user/assistant messages. Tool messages have
    // structural first lines; prefixing them with `» ` looks like an error.
    use crate::domain::models::MessageRole;
    let role = conversation.messages[target_idx].role;
    if !matches!(
        role,
        MessageRole::User | MessageRole::Assistant | MessageRole::System
    ) {
        state.status = StatusState::Flash {
            message: "Cannot bookmark tool message — target a user or assistant message"
                .to_string(),
            remaining_ms: 2000,
        };
        state.needs_redraw = true;
        return;
    }

    // Clone-mutate-save pattern for rollback safety.
    let mut new_meta = tab_manager.active_tab().session_meta.clone();
    let was_bookmarked = new_meta.bookmarks.binary_search(&target_idx).is_ok();
    if was_bookmarked {
        new_meta.bookmarks.retain(|&i| i != target_idx);
    } else {
        match new_meta.bookmarks.binary_search(&target_idx) {
            Ok(_) => {} // already there, should not happen given we checked above
            Err(pos) => new_meta.bookmarks.insert(pos, target_idx),
        }
    }

    // Optimistic UI: update in-memory first.
    let old_meta = tab_manager.active_tab().session_meta.clone();
    tab_manager.active_tab_mut().session_meta = new_meta.clone();

    // Persist to disk. Rollback on failure.
    match fs_storage
        .save_session_meta(&conversation.id, &new_meta)
        .await
    {
        Ok(()) => {
            // Third-audit Fix R1: flash indices are 0-based per AC10 — match
            // the bookmark list display and the internal Vec<usize> storage.
            let msg = if was_bookmarked {
                format!("Bookmark removed (msg {})", target_idx)
            } else {
                format!("Bookmark added (msg {})", target_idx)
            };
            state.status = StatusState::Flash {
                message: msg,
                remaining_ms: 2000,
            };
        }
        Err(e) => {
            tab_manager.active_tab_mut().session_meta = old_meta;
            state.status = StatusState::Flash {
                message: format!("Failed to save bookmark: {}", e),
                remaining_ms: 3000,
            };
        }
    }
    state.needs_redraw = true;
}

/// Story 4-4 AC10: open the bookmark list panel. Flashes a teaching message
/// when the active tab has no bookmarks (reviewer Fix 10 — warmer copy).
fn apply_open_bookmark_list(tab_manager: &TabManager, state: &mut TuiState) {
    let bookmarks = &tab_manager.active_tab().session_meta.bookmarks;
    if bookmarks.is_empty() {
        state.status = StatusState::Flash {
            message: "No bookmarks in this conversation — press 'm' on a message to add one"
                .to_string(),
            remaining_ms: 2000,
        };
        state.needs_redraw = true;
        return;
    }
    state.focus = FocusState::Overlay(crate::domain::models::visual::OverlayType::BookmarkList);
    // AC10 amendment (party-mode Fix 30): reset selection to 0 on every open
    // so a stale index from a previous session doesn't survive the close.
    state.bookmark_list_selected = 0;
    // AC10 amendment (party-mode Fix 18): mirror bookmark count onto state
    // for the key-handler's upper-bound clamp (j / Down).
    state.bookmark_list_count = bookmarks.len();
    state.needs_redraw = true;
}

/// Story 4-4 AC10: jump to the currently selected bookmark.
fn apply_jump_bookmark(tab_manager: &TabManager, state: &mut TuiState) {
    use crate::adapters::tui::widgets::chat_pane;
    let bookmarks = &tab_manager.active_tab().session_meta.bookmarks;
    if bookmarks.is_empty() {
        state.focus = FocusState::Chat;
        state.needs_redraw = true;
        return;
    }
    let sel = state.bookmark_list_selected.min(bookmarks.len() - 1);
    let target_msg = bookmarks[sel];
    state.scroll_snapshot = chat_pane::find_scroll_offset_for_message(
        target_msg,
        &state.message_boundaries,
        state.total_content_height,
        state.viewport_height as usize,
    );
    state.auto_snapshot = state.scroll_snapshot == 0;
    state.focus = FocusState::Chat;
    state.bookmark_list_selected = 0;
    state.needs_redraw = true;
}

/// Story 4-4 AC10: delete the currently selected bookmark, stashing it in
/// the undo buffer for 5 s (synthesis of Sally #7 + Reviewer Fix 8).
async fn apply_delete_bookmark(
    tab_manager: &mut TabManager,
    state: &mut TuiState,
    conversation: &Conversation,
    fs_storage: &crate::adapters::filesystem::FileSystemStorage,
) {
    let mut new_meta = tab_manager.active_tab().session_meta.clone();
    if new_meta.bookmarks.is_empty() {
        return;
    }
    let sel = state
        .bookmark_list_selected
        .min(new_meta.bookmarks.len() - 1);
    let removed_idx = new_meta.bookmarks.remove(sel);

    let old_meta = tab_manager.active_tab().session_meta.clone();
    tab_manager.active_tab_mut().session_meta = new_meta.clone();

    match fs_storage
        .save_session_meta(&conversation.id, &new_meta)
        .await
    {
        Ok(()) => {
            // Stash in undo buffer (single-entry, 5 s window).
            state.bookmark_undo_buffer = Some((removed_idx, std::time::Instant::now()));
            // Keep the mirror count in sync for the key handler's clamp.
            state.bookmark_list_count = new_meta.bookmarks.len();
            // Clamp selection to the new list length.
            if new_meta.bookmarks.is_empty() {
                state.bookmark_list_selected = 0;
                state.focus = FocusState::Chat;
            } else {
                state.bookmark_list_selected = state
                    .bookmark_list_selected
                    .min(new_meta.bookmarks.len() - 1);
            }
            state.status = StatusState::Flash {
                message: format!("Bookmark removed (msg {}) — press u to undo", removed_idx),
                remaining_ms: 2000,
            };
        }
        Err(e) => {
            tab_manager.active_tab_mut().session_meta = old_meta;
            state.status = StatusState::Flash {
                message: format!("Failed to delete bookmark: {}", e),
                remaining_ms: 3000,
            };
        }
    }
    state.needs_redraw = true;
}

/// Story 4-4 AC10: undo the most recent bookmark delete (if within 5 s).
async fn apply_undo_bookmark_delete(
    tab_manager: &mut TabManager,
    state: &mut TuiState,
    conversation: &Conversation,
    fs_storage: &crate::adapters::filesystem::FileSystemStorage,
) {
    let Some((idx, when)) = state.bookmark_undo_buffer else {
        return; // silent no-op
    };
    if when.elapsed() > std::time::Duration::from_secs(5) {
        state.bookmark_undo_buffer = None;
        return; // silent no-op (expired)
    }

    let mut new_meta = tab_manager.active_tab().session_meta.clone();
    // Re-insert sorted.
    match new_meta.bookmarks.binary_search(&idx) {
        Ok(_) => {
            // Already present — nothing to do but clear the buffer.
            state.bookmark_undo_buffer = None;
            return;
        }
        Err(pos) => new_meta.bookmarks.insert(pos, idx),
    }

    let old_meta = tab_manager.active_tab().session_meta.clone();
    tab_manager.active_tab_mut().session_meta = new_meta.clone();
    match fs_storage
        .save_session_meta(&conversation.id, &new_meta)
        .await
    {
        Ok(()) => {
            state.bookmark_undo_buffer = None;
            state.status = StatusState::Flash {
                message: format!("Bookmark restored (msg {})", idx),
                remaining_ms: 1500,
            };
        }
        Err(e) => {
            tab_manager.active_tab_mut().session_meta = old_meta;
            state.status = StatusState::Flash {
                message: format!("Failed to restore bookmark: {}", e),
                remaining_ms: 3000,
            };
        }
    }
    state.needs_redraw = true;
}

/// Story 4-4 AC11/AC12: export the active conversation to markdown.
///
/// Dual-mode semantics:
/// - `/export` (no arg) → **auto-number mode**: write to
///   `.rustain/exports/<slug>.md`, incrementing a `-N` suffix until a free
///   name is found. Never prompts, never overwrites.
/// - `/export <filename>` → **explicit-path mode**: write to the named path
///   relative to `.rustain/exports/`. On collision, opens the Tier-1
///   `ExportOverwrite` confirmation overlay with the pre-rendered content
///   stashed in `state.pending_export` for a stable snapshot at confirm time.
///
/// Runs async. All blocking filesystem I/O is routed through
/// `tokio::task::spawn_blocking` so the TUI event loop is never held up by
/// disk latency.
///
/// Path-traversal hardening (second-audit Fix 27): `/export <name>` rejects
/// absolute paths and any relative path that contains `..` components, and
/// additionally verifies the canonicalized parent stays within the
/// `.rustain/exports/` directory. Supersedes the original Dev Notes wording
/// that allowed absolute paths.
///
/// Atomic write: `.tmp` → `fsync` → `rename` per Dev Notes § Atomic Write
/// Pattern. Keeps crash-safety guarantees for exported files.
///
/// **Visibility:** marked `pub` for integration test access — the tests in
/// `tests/e2e_export_filesystem.rs` need to exercise the full flow with a
/// real tempdir workspace. Not intended for external callers.
#[doc(hidden)]
pub async fn apply_export_command(
    arg: Option<&str>,
    conversation: &Conversation,
    meta: &crate::domain::models::SessionMeta,
    workspace_path: &std::path::Path,
    state: &mut TuiState,
) {
    use crate::domain::services::export::{render_conversation_markdown, slugify};

    let exports_dir = workspace_path.join(".rustain").join("exports");
    // Create the exports dir on a blocking task.
    {
        let exports_dir = exports_dir.clone();
        let create_result =
            tokio::task::spawn_blocking(move || std::fs::create_dir_all(&exports_dir)).await;
        match create_result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                state.status = StatusState::Flash {
                    message: format!("Export failed: cannot create exports dir: {}", e),
                    remaining_ms: 3000,
                };
                state.needs_redraw = true;
                return;
            }
            Err(join_err) => {
                state.status = StatusState::Flash {
                    message: format!("Export failed: {}", join_err),
                    remaining_ms: 3000,
                };
                state.needs_redraw = true;
                return;
            }
        }
    }

    // Canonicalize the exports_dir for the path-traversal check.
    let canonical_exports = match tokio::task::spawn_blocking({
        let exports_dir = exports_dir.clone();
        move || std::fs::canonicalize(&exports_dir)
    })
    .await
    {
        Ok(Ok(p)) => p,
        _ => exports_dir.clone(),
    };

    // Resolve the target path.
    let target_path: std::path::PathBuf = match arg {
        None => {
            // Auto-number mode — browser-style. Loop unbounded; if the user
            // has 1000+ exports they hit them in practice, that's their
            // problem per Story 4-4 Dev Notes § Export Invocation Modes.
            let base_slug = if conversation.title.is_empty() {
                format!(
                    "conversation-{}",
                    &conversation.id[..8.min(conversation.id.len())]
                )
            } else {
                slugify(&conversation.title)
            };
            let candidate = exports_dir.join(format!("{}.md", base_slug));
            find_available_numbered_path(candidate, &exports_dir, &base_slug).await
        }
        Some(name) => {
            // Explicit-path mode — Phase 3 security: reject absolute paths
            // and path-traversal attempts that escape the exports dir.
            let raw = std::path::PathBuf::from(name);
            if raw.is_absolute() {
                state.status = StatusState::Flash {
                    message:
                        "Export failed: absolute paths are not allowed (use a name relative to .rustain/exports/)"
                            .to_string(),
                    remaining_ms: 3500,
                };
                state.needs_redraw = true;
                return;
            }
            // Reject paths containing `..` components outright — simpler and
            // stronger than canonicalize-then-compare because non-existent
            // files cannot be canonicalized.
            if raw
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                state.status = StatusState::Flash {
                    message: "Export failed: path contains '..' — not allowed".to_string(),
                    remaining_ms: 3500,
                };
                state.needs_redraw = true;
                return;
            }
            let candidate = exports_dir.join(&raw);
            // Defense-in-depth: canonicalize the parent and verify containment.
            // If the parent doesn't exist yet, that's acceptable — we rejected
            // `..` above, so the candidate cannot escape.
            if let Some(parent) = candidate.parent() {
                if let Ok(Ok(canonical_parent)) = tokio::task::spawn_blocking({
                    let parent = parent.to_path_buf();
                    move || std::fs::canonicalize(&parent)
                })
                .await
                {
                    if !canonical_parent.starts_with(&canonical_exports) {
                        state.status = StatusState::Flash {
                            message: "Export failed: target escapes .rustain/exports/".to_string(),
                            remaining_ms: 3500,
                        };
                        state.needs_redraw = true;
                        return;
                    }
                }
            }
            candidate
        }
    };

    // Render content before checking for collisions so the overlay captures a
    // stable snapshot (avoids races with the conversation mutating between
    // the `y` press and the write).
    let now = crate::domain::models::session_meta::now_unix();
    let content = render_conversation_markdown(conversation, meta, now);

    // Check collision for explicit-path mode (arg = Some) — open the Tier-1
    // ExportOverwrite confirmation overlay instead of flashing an error.
    let target_exists = tokio::task::spawn_blocking({
        let target_path = target_path.clone();
        move || target_path.exists()
    })
    .await
    .unwrap_or(false);
    if arg.is_some() && target_exists {
        state.pending_export = Some((target_path.clone(), content));
        state.focus =
            FocusState::Overlay(crate::domain::models::visual::OverlayType::Confirmation(
                crate::domain::models::visual::ConfirmationType::ExportOverwrite(target_path),
            ));
        state.needs_redraw = true;
        return;
    }

    // Atomic write on a blocking thread.
    let workspace_path_owned = workspace_path.to_path_buf();
    let target_path_owned = target_path.clone();
    let write_result = tokio::task::spawn_blocking(move || {
        use std::io::Write as _;
        let tmp_path = target_path_owned.with_extension("md.tmp");
        let write: std::io::Result<()> = (|| {
            let mut f = std::fs::File::create(&tmp_path)?;
            f.write_all(content.as_bytes())?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&tmp_path, &target_path_owned)?;
            Ok(())
        })();
        if write.is_err() {
            let _ = std::fs::remove_file(&tmp_path);
        }
        write
    })
    .await;

    match write_result {
        Ok(Ok(())) => {
            let display_path = target_path
                .strip_prefix(&workspace_path_owned)
                .unwrap_or(&target_path);
            state.status = StatusState::Flash {
                message: format!("Exported to {}", display_path.display()),
                remaining_ms: 3000,
            };
        }
        Ok(Err(e)) => {
            state.status = StatusState::Flash {
                message: format!("Export failed: {}", e),
                remaining_ms: 3000,
            };
        }
        Err(join_err) => {
            state.status = StatusState::Flash {
                message: format!("Export failed: {}", join_err),
                remaining_ms: 3000,
            };
        }
    }
    state.needs_redraw = true;
}

/// Auto-number helper: walk `<slug>.md`, `<slug>-2.md`, ... until we find a
/// filename that doesn't exist. Unbounded per spec — in the absurd case of
/// 10000+ exports the user will notice before we do.
///
/// Second-audit Fix 5: the existence probe loop is wrapped in a single
/// `spawn_blocking` call so the entire walk runs off the async event loop.
/// For the common case of 0–2 collisions this is a <100 µs round trip.
async fn find_available_numbered_path(
    initial: std::path::PathBuf,
    exports_dir: &std::path::Path,
    base_slug: &str,
) -> std::path::PathBuf {
    let exports_dir_owned = exports_dir.to_path_buf();
    let exports_dir_fallback = exports_dir_owned.clone();
    let base_slug_owned = base_slug.to_string();
    let base_slug_fallback = base_slug_owned.clone();
    tokio::task::spawn_blocking(move || {
        let mut path = initial;
        let mut n: u64 = 2;
        while path.exists() {
            path = exports_dir_owned.join(format!("{}-{}.md", base_slug_owned, n));
            n = n.saturating_add(1);
        }
        path
    })
    .await
    .unwrap_or_else(|_| exports_dir_fallback.join(format!("{}.md", base_slug_fallback)))
}

/// Story 4-4 AC5: run a cross-conversation search scan and populate
/// `state.cross_search` with the results. Blocks the event loop for up to
/// DEPRECATED in favor of the async-spawned path that delivers results via
/// `AppEvent::CrossSearchResultsReady`. Kept temporarily for reference until
/// Phase 5 E2E tests are rewritten. No longer called from the event loop.
#[allow(dead_code)]
async fn apply_cross_search_scan(
    storage: &dyn StoragePort,
    session_index: &SessionIndex,
    state: &mut TuiState,
) {
    use crate::domain::services::cross_search::{CrossSearchBudget, run_cross_search};
    state.cross_search.running = true;
    let outcome = run_cross_search(
        storage,
        session_index,
        &state.cross_search.query,
        CrossSearchBudget::default(),
    )
    .await;
    state.cross_search.running = false;
    state.cross_search.results = outcome.results;
    state.cross_search.truncated_by_count = outcome.truncated_by_count;
    state.cross_search.truncated_by_time = outcome.truncated_by_time;
    state.cross_search.scanned = outcome.scanned;
    state.cross_search.total = outcome.total;
    // Clamp selection.
    if state.cross_search.results.is_empty() {
        state.cross_search.selected = 0;
    } else {
        state.cross_search.selected = state
            .cross_search
            .selected
            .min(state.cross_search.results.len() - 1);
    }
}

/// Story 4-4 AC6 (amended): open the selected cross-search result in a new
/// tab (or switch to its existing tab), scroll to the target message, and
/// apply a transient peek highlight for 1500 ms.
///
/// Does NOT auto-open the inline search bar — the reviewer's Fix 5 synthesis
/// replaces the original auto-open with a peek highlight, eliminating
/// cognitive whiplash.
#[allow(clippy::too_many_arguments)]
async fn apply_open_cross_search_result(
    tab_manager: &mut TabManager,
    conversation: &mut Conversation,
    streaming: &mut StreamingState,
    session_manager: &mut SessionManager,
    state: &mut TuiState,
    turn_queue: &mut TurnQueue,
    storage: &dyn StoragePort,
    fs_storage: &crate::adapters::filesystem::FileSystemStorage,
    session_index: &mut SessionIndex,
    domain_tx: &mpsc::UnboundedSender<AppEvent>,
) {
    // Resolve the selected result.
    let Some(result) = state
        .cross_search
        .results
        .get(state.cross_search.selected)
        .cloned()
    else {
        return;
    };

    // If already open in a tab, switch to it. Otherwise load and create new.
    let existing_tab_idx = tab_manager
        .tabs()
        .iter()
        .position(|t| t.conversation.id == result.conversation_id);

    if let Some(idx) = existing_tab_idx {
        save_active_tab(
            tab_manager,
            conversation,
            streaming,
            session_manager,
            state,
            turn_queue,
        );
        tab_manager.switch_to_index(idx + 1); // 1-based
        let _ = load_active_tab(
            tab_manager,
            conversation,
            streaming,
            session_manager,
            state,
            turn_queue,
        );
    } else {
        match storage.load_conversation(&result.conversation_id).await {
            Ok(Some(loaded)) => {
                save_active_tab(
                    tab_manager,
                    conversation,
                    streaming,
                    session_manager,
                    state,
                    turn_queue,
                );
                tab_manager.create_tab_with_conversation(loaded.clone());
                // Hydrate session_meta (bookmarks) for the new tab.
                if let Ok(Some(meta)) = fs_storage.load_session_meta(&result.conversation_id).await
                {
                    tab_manager.active_tab_mut().session_meta = meta;
                }
                *conversation = loaded;
                session_index.set_open(&result.conversation_id, true);
                session_index.set_active(Some(&result.conversation_id));
            }
            Ok(None) => {
                tracing::warn!(
                    "cross-search: conversation {} not found at open time",
                    result.conversation_id
                );
                return;
            }
            Err(e) => {
                tracing::error!(
                    "cross-search: load_conversation({}) failed: {}",
                    result.conversation_id,
                    e
                );
                state.status = StatusState::Flash {
                    message: format!("Failed to open conversation: {}", e),
                    remaining_ms: 3000,
                };
                state.needs_redraw = true;
                return;
            }
        }
    }

    // Stash the cross-search query before resetting the overlay, so we can
    // pre-populate the inline search bar on the next Ctrl+F press (AC6
    // amendment: we do NOT auto-open the bar, but we DO preserve the query
    // in case the user wants to jump to other matches in the same
    // conversation).
    let preserved_query = state.cross_search.query.clone();

    // Close the cross-search overlay.
    state.cross_search = crate::adapters::tui::state::CrossSearchState::new();

    // Scroll to the target message. `message_boundaries` for the newly
    // activated tab will be computed on the next render tick, so this scroll
    // may be briefly off until the first repaint — acceptable for v1.
    use crate::adapters::tui::widgets::chat_pane;
    state.scroll_snapshot = chat_pane::find_scroll_offset_for_message(
        result.first_match_message_index,
        &state.message_boundaries,
        state.total_content_height,
        state.viewport_height as usize,
    );
    state.auto_snapshot = state.scroll_snapshot == 0;
    state.focus = FocusState::Chat;

    // Preserve the query on state.search_state.query (inactive) so Ctrl+F
    // opens the bar pre-populated. AC6 amendment: no auto-open.
    state.search_state.query = preserved_query.clone();

    // Set a 1500 ms peek highlight on the cross-search flagged match in the
    // newly loaded conversation (AC6 — replaces the v1 status flash).
    //
    // Second-audit Fix 6: target the match in `result.first_match_message_index`
    // explicitly rather than relying on `matches.first()`. Under normal
    // operation the two are the same (because `find_matches` is deterministic
    // and returns matches sorted by `(message_index, byte_start)`), but the
    // explicit filter is defensive against conversation mutation between scan
    // and open, and makes the intent visible to future readers.
    //
    // The event loop wakes itself via AppEvent::PeekHighlightExpired to clear
    // the highlight on time even without further user input.
    if !preserved_query.is_empty() {
        use crate::adapters::tui::state::PeekHighlight;
        use crate::domain::services::search::find_matches;
        let matches = find_matches(conversation, &preserved_query);
        let target = matches
            .iter()
            .find(|m| m.message_index == result.first_match_message_index)
            .or_else(|| matches.first());
        if let Some(target_match) = target {
            let expires_at = std::time::Instant::now() + std::time::Duration::from_millis(1500);
            state.search_state.peek_highlight = Some(PeekHighlight {
                m: target_match.clone(),
                expires_at,
            });
            // Schedule the expiry event: tokio::time::sleep_until the deadline,
            // then send PeekHighlightExpired to the event loop so the render
            // loop clears the highlight on time even if no other input occurs.
            let tx = domain_tx.clone();
            let tab_id = tab_manager.active_tab_index();
            tokio::spawn(async move {
                tokio::time::sleep_until(tokio::time::Instant::from_std(expires_at)).await;
                let _ = tx.send(AppEvent::PeekHighlightExpired { tab_id });
            });
        }
    }
    state.needs_redraw = true;
}

fn save_active_tab(
    tab_manager: &mut TabManager,
    conversation: &Conversation,
    streaming: &StreamingState,
    session_manager: &SessionManager,
    state: &TuiState,
    turn_queue: &TurnQueue,
) {
    let tab = tab_manager.active_tab_mut();
    tab.conversation = conversation.clone();
    tab.streaming = streaming.clone();
    tab.session = session_manager.clone();
    tab.view_state.scroll_offset = state.scroll_snapshot;
    // P1: Always persist mode so Reading/Pinned is not lost on tab switch.
    if state.auto_snapshot {
        tab.view_state.mode = crate::domain::models::AnchorMode::Following;
    } else {
        // Keep the current mode (Reading or Pinned) — don't clobber with stale.
    }
    tab.block_boundaries = state.block_boundaries.clone();
    tab.message_boundaries = state.message_boundaries.clone();
    tab.user_message_boundaries = state.user_message_boundaries.clone();
    tab.focused_tool_id = state.focused_tool_id.clone();
    tab.feedback_blocks = state.feedback_blocks.clone();
    tab.active_feedback_id = state.active_feedback_id.clone();
    tab.total_content_height = state.total_content_height;
    tab.pending_anchor = state.pending_anchor;
    tab.turn_queue = turn_queue.clone();
}

/// Load the new active tab's state from TabManager into the proxy variables.
/// Returns `true` if the tab is idle and has queued messages ready to send
/// (e.g. a turn completed in the background while the tab was inactive).
fn load_active_tab(
    tab_manager: &TabManager,
    conversation: &mut Conversation,
    streaming: &mut StreamingState,
    session_manager: &mut SessionManager,
    state: &mut TuiState,
    turn_queue: &mut TurnQueue,
) -> bool {
    let tab = tab_manager.active_tab();
    *conversation = tab.conversation.clone();
    *streaming = tab.streaming.clone();
    *session_manager = tab.session.clone();
    *turn_queue = tab.turn_queue.clone();
    state.scroll_snapshot = tab.view_state.scroll_offset;
    // P2: Restore saved mode. Non-streaming tabs keep their view_state.mode
    // (Reading, Following, or Pinned) rather than unconditionally forcing Following.
    // Streaming tabs also restore the saved mode so Pinned anchors survive tab switch.
    state.auto_snapshot = matches!(
        tab.view_state.mode,
        crate::domain::models::AnchorMode::Following
    );
    state.block_boundaries = tab.block_boundaries.clone();
    state.message_boundaries = tab.message_boundaries.clone();
    state.user_message_boundaries = tab.user_message_boundaries.clone();
    state.focused_tool_id = tab.focused_tool_id.clone();
    state.feedback_blocks = tab.feedback_blocks.clone();
    state.active_feedback_id = tab.active_feedback_id.clone();
    state.total_content_height = tab.total_content_height;
    state.pending_anchor = tab.pending_anchor;
    state.active_tab_id = tab_manager.active_tab_id();
    state.chord_leader_active = false;
    // tool_block_states is global (cleared on tab switch); version resets for new tab
    state.tool_block_states.clear();
    if let Some(trs) = state
        .tab_render_states
        .get_mut(&tab_manager.active_tab_id())
    {
        trs.tool_block_states_version = 0;
    }
    // Sync token usage from the loaded conversation
    state.token_usage = tab.conversation.usage.clone();
    // Infer status from loaded streaming state so the status bar reflects the tab being entered.
    // Without this, a stale "Streaming" status from the departing tab persists on screen
    // even after the background turn completed (the background TurnComplete handler intentionally
    // does not touch state.status because it belongs to a different tab).
    state.status = if tab.streaming.is_streaming {
        StatusState::Streaming
    } else {
        StatusState::Idle
    };
    // Clear any retry state that belonged to the previous tab
    state.retry_state = None;
    // Signal caller if queued messages should be drained: the tab's turn completed
    // in the background but auto-send (line 948) only runs in the active-tab path.
    !tab.streaming.is_streaming && !tab.turn_queue.is_empty()
}

/// Start a new turn: add user message, spawn provider streaming task.
#[allow(clippy::too_many_arguments)]
/// Decode, hash, and persist the `pending_images` drained on message submit.
///
/// Returns the image refs to attach to the new user `ChatMessage` so they survive
/// a reload (Story 4-3a.1 AC3). Failures are logged via `tracing::warn!` and the
/// corresponding attachment is dropped from the persisted list — AC4's
/// graceful-degradation on load handles missing files if a partial save occurs.
fn persist_image_attachments(
    conversation_id: &str,
    fs_storage: &crate::adapters::filesystem::FileSystemStorage,
    attachments: &[ImageAttachment],
) -> Vec<crate::domain::models::ImageReference> {
    use crate::adapters::filesystem::{content_hash, normalize_extension};
    use base64::{Engine, engine::general_purpose::STANDARD};

    // Guard: base64 encoding inflates size by ~4/3. MAX_RAW_IMAGE_SIZE is 20MB, so the
    // encoded ceiling is ceil(20MB * 4/3) ≈ 27MB. Reject larger input before decoding to
    // prevent memory exhaustion from a malformed or hostile attachment (P1, party-mode
    // review 2026-04-12). The upstream paste handler already enforces MAX_RAW_IMAGE_SIZE
    // on the raw bytes; this is a second line of defence at the persistence boundary.
    const MAX_BASE64_ENCODED_SIZE: usize = 27 * 1024 * 1024; // 27MB

    let mut refs = Vec::with_capacity(attachments.len());
    for attachment in attachments {
        if attachment.data.len() > MAX_BASE64_ENCODED_SIZE {
            tracing::warn!(
                size = attachment.data.len(),
                limit = MAX_BASE64_ENCODED_SIZE,
                "Skipping image attachment: base64 data exceeds size limit"
            );
            continue;
        }
        let bytes = match STANDARD.decode(attachment.data.as_bytes()) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("Failed to base64-decode pending image: {}", e);
                continue;
            }
        };
        let hash = content_hash(&bytes);
        let ext = normalize_extension(&attachment.media_type);
        let file_name = format!("{}.{}", hash, ext);
        let image_ref = crate::domain::models::ImageReference {
            file_name,
            media_type: attachment.media_type.clone(),
            original_size: bytes.len(),
        };
        // Block on the async save via tokio's current-runtime handle. This
        // runs inside the event loop task which already holds the runtime
        // context, so `block_in_place` + a short-lived local is sufficient.
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(fs_storage.save_image(
                conversation_id,
                &image_ref,
                &bytes,
            ))
        });
        match result {
            Ok(()) => refs.push(image_ref),
            Err(e) => {
                tracing::warn!(
                    conversation_id,
                    file_name = %image_ref.file_name,
                    "Failed to persist image attachment: {}",
                    e
                );
                // Still record the ref — the ChatMessage will carry it, and
                // load-time validation will flag the missing file to the user.
                refs.push(image_ref);
            }
        }
    }
    refs
}

/// Walk `conversation.messages` in parallel with the `Vec<Message>` produced
/// by `build_api_messages` and, for each historical user message that carries
/// persisted `ImageReference` entries, load the bytes from disk, base64-encode
/// them, and attach the resulting `ImageAttachment` blocks to the matching
/// API `Message`.
///
/// This is the runtime half of DF-067: Story 4-3a.1's original scope only
/// covered persisting images to disk so they survive reload. It did not cover
/// re-attaching them to the outbound API request on turn N+1, which meant the
/// assistant "forgot" images across turns. Addendum 2 (2026-04-12 party-mode
/// follow-up) closes that gap.
///
/// Messages whose `images` vec is already populated are skipped — the fresh
/// turn's just-submitted message already has raw base64 attached by the
/// caller and does not need a disk read.
///
/// Missing files are logged via `tracing::warn!` and the corresponding block
/// is dropped from the request (AC4 graceful degradation extended to the API
/// request path).
///
/// The index-advance logic must stay in sync with `build_api_messages`: it
/// emits one `Message` per `ChatMessage`, plus a second synthetic user
/// `Message` when an assistant turn carries tool-call results.
///
/// NOTE on `block_in_place`: mirrors `persist_image_attachments` — both call
/// sites share the same sync-over-async pattern and will be lifted together
/// whenever DF-102 (async conversion of `start_turn`) is resolved.
pub fn rehydrate_historical_images(
    conversation: &Conversation,
    messages: &mut [crate::domain::models::Message],
    fs_storage: &crate::adapters::filesystem::FileSystemStorage,
) {
    use base64::{Engine, engine::general_purpose::STANDARD};

    let mut msg_idx = 0;
    for cm in &conversation.messages {
        if msg_idx >= messages.len() {
            break;
        }

        if cm.role == MessageRole::User
            && !cm.images.is_empty()
            && messages[msg_idx].images.is_empty()
        {
            for image_ref in &cm.images {
                let result = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current()
                        .block_on(fs_storage.load_image(&conversation.id, image_ref))
                });
                match result {
                    Ok(bytes) => {
                        let data = STANDARD.encode(&bytes);
                        messages[msg_idx].images.push(ImageAttachment {
                            media_type: image_ref.media_type.clone(),
                            data,
                        });
                    }
                    Err(e) => {
                        tracing::warn!(
                            conversation_id = %conversation.id,
                            file_name = %image_ref.file_name,
                            "Skipping historical image in API request: {}",
                            e
                        );
                    }
                }
            }
        }

        msg_idx += 1;

        // Mirror `build_api_messages`: assistant messages with at least one
        // tool-call result produce a second synthetic user message for the
        // tool results.
        if cm.role == MessageRole::Assistant
            && !cm.tool_calls.is_empty()
            && cm.tool_calls.iter().any(|tc| tc.result.is_some())
        {
            msg_idx += 1;
        }
    }
}

fn cycle_mode(current: PermissionMode) -> PermissionMode {
    match current {
        PermissionMode::Normal => PermissionMode::AutoEdit,
        PermissionMode::AutoEdit => PermissionMode::Plan,
        PermissionMode::Plan => PermissionMode::Yolo,
        PermissionMode::Yolo => PermissionMode::Normal,
    }
}

/// Wrapper that always creates a non-synthetic user message.
/// Call `start_turn_inner` directly when you need `synthetic: true`.
#[allow(dead_code)]
async fn start_turn(
    text: &str,
    images: Vec<ImageAttachment>,
    conversation: &mut Conversation,
    streaming: &mut StreamingState,
    state: &mut TuiState,
    active_turn: &mut Option<tokio::task::JoinHandle<()>>,
    provider: &Arc<dyn StreamingProvider>,
    config: &AppConfig,
    domain_tx: &mpsc::UnboundedSender<AppEvent>,
    security: &Arc<dyn SecurityPort>,
    tools: &Arc<dyn ToolSetPort>,
    tool_scheduler: &Arc<crate::domain::services::tool_scheduler::ToolScheduler>,
    persona: &Arc<dyn PersonaPort>,
    workspace_path: &std::path::Path,
    session_manager: &mut SessionManager,
    fs_storage: &crate::adapters::filesystem::FileSystemStorage,
    storage: &Arc<dyn StoragePort>,
    _plan_manager: &Arc<crate::domain::services::plan_manager::PlanManager>,
    plan_injector: &Arc<crate::domain::services::plan_mode_injector::DefaultPlanInjector>,
    _plan_file: Option<std::path::PathBuf>,
    activation_set: Option<crate::domain::models::SkillActivationSet>,
    agent_snapshot: Option<crate::domain::models::ActiveAgent>,
    turn_cancel: CancellationToken,
    usage_ledger: Arc<dyn UsageLedgerPort>,
) {
    start_turn_inner(
        text,
        images,
        false,
        conversation,
        streaming,
        state,
        active_turn,
        provider,
        config,
        domain_tx,
        security,
        tools,
        tool_scheduler,
        persona,
        workspace_path,
        session_manager,
        fs_storage,
        storage,
        _plan_manager,
        plan_injector,
        _plan_file,
        activation_set,
        agent_snapshot,
        turn_cancel,
        usage_ledger,
    )
    .await;
}

async fn start_turn_inner(
    text: &str,
    images: Vec<ImageAttachment>,
    synthetic: bool,
    conversation: &mut Conversation,
    streaming: &mut StreamingState,
    state: &mut TuiState,
    active_turn: &mut Option<tokio::task::JoinHandle<()>>,
    provider: &Arc<dyn StreamingProvider>,
    config: &AppConfig,
    domain_tx: &mpsc::UnboundedSender<AppEvent>,
    security: &Arc<dyn SecurityPort>,
    tools: &Arc<dyn ToolSetPort>,
    tool_scheduler: &Arc<crate::domain::services::tool_scheduler::ToolScheduler>,
    persona: &Arc<dyn PersonaPort>,
    workspace_path: &std::path::Path,
    session_manager: &mut SessionManager,
    fs_storage: &crate::adapters::filesystem::FileSystemStorage,
    storage: &Arc<dyn StoragePort>,
    _plan_manager: &Arc<crate::domain::services::plan_manager::PlanManager>,
    plan_injector: &Arc<crate::domain::services::plan_mode_injector::DefaultPlanInjector>,
    _plan_file: Option<std::path::PathBuf>,
    activation_set: Option<crate::domain::models::SkillActivationSet>,
    agent_snapshot: Option<crate::domain::models::ActiveAgent>,
    turn_cancel: CancellationToken,
    usage_ledger: Arc<dyn UsageLedgerPort>,
) {
    tracing::debug!(
        "start_turn_inner: synthetic={synthetic} text_len={}",
        text.len()
    );
    // Persist any image attachments and collect their references. These are
    // attached to the user ChatMessage so they survive a session reload
    // (Story 4-3a.1 AC3 / DF-067).
    let persisted_refs = if images.is_empty() {
        Vec::new()
    } else {
        persist_image_attachments(&conversation.id, fs_storage, &images)
    };

    // Add user ChatMessage to conversation
    conversation.messages.push(ChatMessage {
        id: generate_conversation_id(),
        role: MessageRole::User,
        content: text.to_string(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: crate::domain::models::session_meta::now_unix(),
        token_count: None,
        stop_reason: None,
        synthetic,
        images: persisted_refs,
    });

    // Build messages list for provider
    let mut messages = message_builder::build_api_messages(conversation);

    // Plan mode reminder injection (Story 6-0d AC2/AC8)
    if security.current_mode() == PermissionMode::Plan {
        if let Some(ref plan_path) = state.plan_file_path {
            if let Some(reminder) = plan_injector.pre_turn(conversation, plan_path).await {
                if let Some(first_user_msg) =
                    messages.iter_mut().find(|m| m.role == MessageRole::User)
                {
                    first_user_msg.context_prefix = Some(reminder);
                }
                let assistant_turns = conversation
                    .messages
                    .iter()
                    .filter(|m| m.role == MessageRole::Assistant)
                    .count() as u32;
                state.pending_plan_reminder_at_turn = Some(assistant_turns);
            } else {
                state.pending_plan_reminder_at_turn = None;
            }
        }
    }

    // Attach images to the last user message in the API request
    // Covers: FR112 (AC1, AC2)
    if !images.is_empty() {
        if let Some(last_user_msg) = messages
            .iter_mut()
            .rev()
            .find(|m| m.role == MessageRole::User && m.content == text)
        {
            last_user_msg.images = images;
        }
    }

    // Rehydrate historical images from disk so the provider sees the same
    // visual context on every subsequent turn (Story 4-3a.1 Addendum 2 /
    // multi-turn image rehydration). `build_api_messages` is a pure domain
    // function and cannot touch disk, so rehydration happens here — after
    // the fresh-turn attachment above, so the just-submitted message is
    // skipped (its `images` vec is already populated with the raw base64
    // and we don't need to re-read it from disk).
    rehydrate_historical_images(conversation, &mut messages, fs_storage);

    // If session manager indicates history rebuild needed, prepend context
    if session_manager.needs_history_rebuild() {
        use crate::domain::services::history_rebuild;
        let context = history_rebuild::build_history_context(
            &conversation.messages[..conversation.messages.len().saturating_sub(1)],
        );
        // Attach context_prefix to the actual user input message (matching `text`),
        // not to a synthetic tool-result message that build_api_messages may have appended.
        if let Some(target_msg) = messages
            .iter_mut()
            .rev()
            .find(|m| m.role == MessageRole::User && m.content == text)
        {
            target_msg.context_prefix = Some(context);
        } else if let Some(last_msg) = messages.last_mut() {
            // Fallback: attach to last message if exact match not found
            last_msg.context_prefix = Some(context);
        }
        let new_session_id = generate_conversation_id();
        conversation.session_id = Some(new_session_id.clone());
        session_manager.mark_active(new_session_id);
        let msg_count = conversation.messages.len().saturating_sub(1);
        domain_tx.send(AppEvent::SystemNotice {
            conversation_id: Some(conversation.id.clone()),
            level: crate::domain::models::NoticeLevel::Info,
            message: format!(
                "\u{2139}\u{fe0f} Session restarted with your conversation history ({} messages).",
                msg_count
            ),
        }).ok();
    }

    let all_tool_defs = tools.available_tools();
    let persona_prompt = persona.system_prompt(workspace_path);
    let empty_set = crate::domain::models::SkillActivationSet::new();
    let activation = activation_set.as_ref().unwrap_or(&empty_set);
    let agent_body = agent_snapshot.as_ref().map(|a| a.body.as_str());
    let system_prompt = crate::domain::services::skill_context::assemble_system_prompt_with_agent(
        &persona_prompt,
        agent_body,
        activation,
        workspace_path,
    );
    let all_tool_names: Vec<String> = all_tool_defs.iter().map(|t| t.name.clone()).collect();
    let agent_filter = agent_snapshot
        .as_ref()
        .and_then(|a| a.effective_tool_filter(&all_tool_names));
    let skill_filter = activation.effective_allowed_tools();
    let combined: Option<std::collections::HashSet<String>> = match (agent_filter, skill_filter) {
        (None, None) => None,
        (Some(a), None) => Some(a),
        (None, Some(s)) => Some(s),
        (Some(a), Some(s)) => Some(a.intersection(&s).cloned().collect()),
    };
    if let Some(ref allowed) = combined {
        if allowed.is_empty() {
            domain_tx.send(AppEvent::SystemNotice {
                conversation_id: Some(conversation.id.clone()),
                level: crate::domain::models::NoticeLevel::Warning,
                message: "Active agent and skill tool filters are disjoint — no tools available for this turn".to_string(),
            }).ok();
        }
    }
    let tool_defs = match combined {
        Some(allowed) => {
            let mut filtered: Vec<_> = all_tool_defs
                .into_iter()
                .filter(|t| allowed.contains(&t.name) || t.name == "activate_skill")
                .collect();
            if !filtered.iter().any(|t| t.name == "activate_skill") {
                let act_tool = crate::domain::models::ToolDefinition {
                    name: "activate_skill".to_string(),
                    description: "Activate an Agent Skill to gain its procedural instructions and tool restrictions. Arg: name of the skill to activate (must match a discovered skill).".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "name": { "type": "string", "description": "Skill name (exact match, case-sensitive)" },
                            "arguments": { "type": "string", "description": "Optional trailing arguments passed to the skill" }
                        },
                        "required": ["name"]
                    }),
                    parallel_safe: true,
                };
                filtered.push(act_tool);
            }
            filtered
        }
        None => all_tool_defs,
    };
    // --- Model resolution (Story 7.1c) ---
    let retry_count = state.retry_state.as_ref().map_or(0, |r| r.attempt as u32);
    let input_tokens = conversation.usage.as_ref().map_or(0, |u| u.input_tokens);
    let explicit_override = agent_snapshot
        .as_ref()
        .and_then(|a| {
            let m = a.model.as_ref()?;
            if m.is_empty() { None } else { Some(m.clone()) }
        })
        .or_else(|| state.selected_model.clone());
    let req = crate::domain::services::model_router::ModelResolutionRequest {
        explicit_override,
        tier_hint: None,
        step_kind: None,
        retry_count,
        input_tokens,
        fallback_model: config.model.clone(),
    };
    let resolved =
        crate::domain::services::model_router::resolve_effective_model(&req, &config.router);
    if resolved.escalation_reason != crate::domain::models::EscalationReason::None {
        tracing::info!(
            target: "router",
            "tier escalated: reason={:?} model={}",
            resolved.escalation_reason,
            resolved.model
        );
    }
    let options = CompletionOptions {
        model: resolved.model.clone(),
        max_tokens: 8192,
        system_prompt: system_prompt.clone(),
        temperature: None,
        tools: tool_defs,
    };

    // Forward-compat: construct AgentLaunchSpec even though foreground path
    // only consumes effective_model today (Story 10.7 will use the full struct).
    let _launch_spec = crate::domain::models::AgentLaunchSpec {
        prompt: system_prompt.clone(),
        effective_model: resolved.model.clone(),
        tools_allow: all_tool_names,
        parent_ctx_tokens: 0,
    };

    let session_id = conversation
        .session_id
        .clone()
        .unwrap_or_else(|| conversation.id.clone());

    // Clear any stale buffers from a previous turn (e.g. after TurnContinuing or SystemNotice)
    streaming.current_text_buffer.clear();
    streaming.current_blocks.clear();
    streaming.active_tool_calls.clear();
    streaming.is_streaming = true;
    streaming.phase = crate::domain::models::StreamingPhase::AccumulatingText;

    let handle = tokio::spawn(turn::run_turn(
        provider.clone(),
        messages,
        options,
        domain_tx.clone(),
        security.clone(),
        tools.clone(),
        tool_scheduler.clone(),
        conversation.id.clone(),
        storage.clone(),
        conversation.clone(),
        activation_set,
        turn_cancel,
        usage_ledger.clone(),
        resolved,
        None,
        0,
        session_id,
    ));
    *active_turn = Some(handle);

    state.status = StatusState::Streaming;
    state.needs_redraw = true;
}

/// Handle a render failure: abort active turn, reset streaming, attempt terminal recovery.
fn handle_render_error(
    err: anyhow::Error,
    active_turn: &mut Option<tokio::task::JoinHandle<()>>,
    streaming: &mut StreamingState,
    state: &mut TuiState,
    terminal: &mut Tui,
) {
    tracing::error!("Render failed: {}", err);

    // Abort active turn if running
    if let Some(handle) = active_turn.take() {
        handle.abort();
    }

    // Reset streaming state
    streaming.is_streaming = false;
    streaming.phase = crate::domain::models::StreamingPhase::Idle;
    streaming.current_text_buffer.clear();
    streaming.current_blocks.clear();
    streaming.active_tool_calls.clear();

    state.status_before_flash = Some(state.status.clone());
    state.status = StatusState::Flash {
        message: format!("Render failed: {}", err),
        remaining_ms: state.theme.timing.status_flash_ms,
    };
    state.needs_redraw = true;

    // Attempt terminal recovery
    crate::adapters::tui::terminal::restore_terminal_raw();
    match crate::adapters::tui::terminal::setup(crate::adapters::tui::terminal::is_mouse_enabled())
    {
        Ok(new_terminal) => {
            *terminal = new_terminal;
            tracing::info!("Terminal recovered after render failure");
        }
        Err(recovery_err) => {
            tracing::error!("Terminal recovery failed: {}", recovery_err);
            state.should_quit = true;
        }
    }
}

/// Advance the permission queue: pop next pending request or restore Input focus.
fn advance_permission_queue(state: &mut TuiState) {
    if let Some(next) = state.permission_queue.pop() {
        state.pending_permission = Some(next);
        state.focus = FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Permission));
    } else {
        state.focus = FocusState::Input;
    }
    state.needs_redraw = true;
}

fn advance_skill_trust_queue(state: &mut TuiState) {
    if let Some(next) = state.skill_trust_queue.pop_front() {
        state.pending_skill_trust = Some(next);
        state.skill_trust_inspect_mode = false;
        state.focus = FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::SkillTrust));
    } else {
        state.pending_skill_trust = None;
        state.skill_trust_inspect_mode = false;
        state.focus = FocusState::Input;
    }
    state.needs_redraw = true;
}

/// Render the full TUI frame.
#[allow(clippy::too_many_arguments)]
fn render(
    terminal: &mut Tui,
    state: &mut TuiState,
    conversation: &Conversation,
    streaming: &StreamingState,
    config_model: &str,
    provider_id: &str,
    security: &dyn SecurityPort,
    tab_count: usize,
    active_tab_index: usize,
    tab_manager_for_bar: Option<&crate::domain::models::tab::TabManager>,
    session_index: &SessionIndex,
) -> Result<()> {
    let scroll_offset = state.scroll_snapshot;
    let auto_scroll = state.auto_snapshot;
    let mut content_height = 0usize;
    let mut block_bounds: Vec<usize> = Vec::new();
    let mut msg_bounds: Vec<usize> = Vec::new();
    let mut user_msg_bounds: Vec<usize> = Vec::new();
    let mut focused_tool_id: Option<String> = None;
    let mut vp_height: u16 = state.viewport_height;

    let permission_mode = security.current_mode();

    // Split borrows: tool_block_states needs &, feedback_blocks needs &
    // height_cache is now in tab_render_states (side-table) and borrowed inside the closure.
    let TuiState {
        ref tool_block_states,
        ref feedback_blocks,
        ref pending_permission,
        ref permission_queue,
        ref pending_feedback_input,
        ref pending_skill_trust,
        ref skill_trust_queue,
        skill_trust_inspect_mode,
        ref pending_activation,
        ref ask_user_question,
        ref theme,
        ref status,
        ref token_usage,
        ref input_buffer,
        cursor_position,
        ref focus,
        has_project_context,
        multiline_mode,
        input_scroll_offset,
        ref reverse_search,
        ref search_state,
        ref autocomplete,
        ref command_palette,
        ref which_key,
        ref help_overlay,
        ref model_selector,
        ref selected_model,
        ref image_indicator,
        ref current_hint,
        sidebar_visible,
        ref pending_fork_index,
        ref pending_rewind_index,
        ref rewind_preview,
        ..
    } = *state;

    let display_model = selected_model.as_deref().unwrap_or(config_model);

    terminal.draw(|frame| {
        let area = frame.area();

        match layout::compute_layout(area, theme, input_buffer, tab_count, sidebar_visible) {
            Some(mut app_layout) => {
                let _is_compact = area.width < 80 || area.height < 24;

                // Story 4-4 AC1: reserve the top row for the search bar when active.
                app_layout.reserve_search_bar(search_state.active);
                // Story 4-4 AC10: reserve the bottom panel when the bookmark
                // list overlay is focused. Panel height scales with the
                // bookmark count (1 header + N entries + 1 footer, capped at 6).
                if focus
                    == &FocusState::Overlay(
                        crate::domain::models::visual::OverlayType::BookmarkList,
                    )
                {
                    let bookmark_count = tab_manager_for_bar
                        .map(|tm| tm.active_tab().session_meta.bookmarks.len())
                        .unwrap_or(0);
                    let requested_height = ((bookmark_count + 3).min(6)) as u16;
                    app_layout.reserve_bookmark_panel(requested_height);
                }

                // Render tab bar if present
                if let (Some(tab_bar_area), Some(tm)) = (app_layout.tab_bar, tab_manager_for_bar) {
                    use crate::adapters::tui::widgets::tab_bar;
                    tab_bar::render_tab_bar(
                        tm,
                        active_tab_index,
                        tab_bar_area,
                        frame.buffer_mut(),
                        theme,
                    );
                }

                // P1/AC1: Render sidebar when visible
                if let Some(sidebar_area) = app_layout.sidebar {
                    let active_conv_id =
                        tab_manager_for_bar.map(|tm| tm.active_tab().conversation.id.as_str());
                    match state.sidebar_panel {
                        Some(crate::domain::models::visual::PanelType::Tasks) => {
                            use crate::adapters::tui::widgets::task_panel;
                            let plan_to_render = task_panel::resolve_panel_plan(
                                conversation,
                                state.task_panel_state.last_executed_plan_id.as_deref(),
                            );
                            state.task_panel_state.task_count =
                                plan_to_render.map(|p| p.tasks.len()).unwrap_or(0);
                            let is_focused = matches!(
                                state.focus,
                                FocusState::Sidebar {
                                    panel: crate::domain::models::visual::PanelType::Tasks,
                                    ..
                                }
                            );
                            task_panel::render_task_panel(
                                sidebar_area,
                                frame.buffer_mut(),
                                plan_to_render,
                                state.task_panel_state.selected_index,
                                is_focused,
                                theme,
                            );
                        }
                        Some(crate::domain::models::visual::PanelType::History) | None => {
                            sidebar::render_history_panel(
                                sidebar_area,
                                frame.buffer_mut(),
                                session_index.entries(),
                                state.sidebar_selected,
                                active_conv_id,
                                theme,
                            );
                        }
                        Some(crate::domain::models::visual::PanelType::Agents)
                        | Some(crate::domain::models::visual::PanelType::Adapters) => {
                            use ratatui::widgets::Widget;
                            let block = ratatui::widgets::Block::default()
                                .title(" (panel deferred) ")
                                .borders(ratatui::widgets::Borders::ALL)
                                .border_style(
                                    ratatui::style::Style::default().fg(theme.colors.fg_muted),
                                );
                            block.render(sidebar_area, frame.buffer_mut());
                        }
                    }
                }

                // Story 4-4 AC2: thread search query + focused match into chat pane
                // rendering so substring matches render with highlight styling.
                let search_query_opt: Option<&str> =
                    if search_state.active && !search_state.query.is_empty() {
                        Some(search_state.query.as_str())
                    } else {
                        None
                    };
                let focused_match_opt = if search_state.active {
                    search_state.matches.get(search_state.focused_match_index)
                } else {
                    None
                };

                vp_height = app_layout.chat_pane.height;

                // Story 4-4 AC9: source the bookmark list from the active
                // tab's session_meta (in-memory mirror of meta.json).
                let bookmark_indices: &[usize] = tab_manager_for_bar
                    .map(|tm| tm.active_tab().session_meta.bookmarks.as_slice())
                    .unwrap_or(&[]);
                let mut skip_chat_render = false;

                // Story 6.4: render priority chain (cards > drill-down > chat)
                // 1. Plan deviation card (AC5)
                if let Some((ref pid, _)) = state.task_panel_state.pending_deviation {
                    use crate::adapters::tui::widgets::plan_deviation_card;
                    let plan_data = conversation.plans.get(pid);
                    let (original, current, changed, summary) = if let Some(plan) = plan_data {
                        let original_count = plan.tasks.len() as u32;
                        let changed_steps: Vec<u32> = plan
                            .tasks
                            .iter()
                            .filter(|t| t.status == PlanTaskStatus::Skipped)
                            .map(|t| t.number)
                            .collect();
                        let current_count = original_count;
                        let summary_str = format!(
                            "{} task(s) auto-skipped due to upstream failure.",
                            changed_steps.len()
                        );
                        (original_count, current_count, changed_steps, summary_str)
                    } else {
                        (0u32, 0u32, vec![], "".to_string())
                    };
                    plan_deviation_card::render(
                        app_layout.chat_pane,
                        frame.buffer_mut(),
                        original,
                        current,
                        &changed,
                        &summary,
                    );
                    skip_chat_render = true;
                }
                // 2. Cancel plan confirm card (AC6)
                else if let Some(ref pid) = state.task_panel_state.cancel_plan_confirm {
                    use crate::adapters::tui::widgets::cancel_plan_confirm_card;
                    let (plan_title, n_pending, n_completed) = conversation
                        .plans
                        .get(pid)
                        .map(|plan| {
                            let pending = plan
                                .tasks
                                .iter()
                                .filter(|t| {
                                    !crate::domain::services::plan_runtime::is_terminal_pub(
                                        t.status,
                                    )
                                })
                                .count() as u32;
                            let completed = plan
                                .tasks
                                .iter()
                                .filter(|t| t.status == PlanTaskStatus::Completed)
                                .count() as u32;
                            (plan.title.clone(), pending, completed)
                        })
                        .unwrap_or_default();
                    cancel_plan_confirm_card::render(
                        app_layout.chat_pane,
                        frame.buffer_mut(),
                        &plan_title,
                        n_pending,
                        n_completed,
                    );
                    skip_chat_render = true;
                }
                // 3. Skip cascade card (AC2)
                else if let Some(ref pending) = state.task_panel_state.skip_cascade_pending {
                    use crate::adapters::tui::widgets::task_skip_cascade_card;
                    task_skip_cascade_card::render(
                        app_layout.chat_pane,
                        frame.buffer_mut(),
                        pending,
                    );
                    skip_chat_render = true;
                }
                // 4. Existing drill-down view (6-3)
                else if let Some(task_number) = state.task_panel_state.drill_down_task {
                    let plan_opt = state
                        .task_panel_state
                        .last_executed_plan_id
                        .as_deref()
                        .and_then(|id| conversation.plans.get(id))
                        .or_else(|| {
                            // Story 6.4: fallback to resolve_panel_plan when
                            // last_executed_plan_id is None (e.g. after restart).
                            // Also back-fill last_executed_plan_id so subsequent
                            // drill-down lookups hit the cached path.
                            use crate::adapters::tui::widgets::task_panel;
                            let p = task_panel::resolve_panel_plan(conversation, None)?;
                            state.task_panel_state.last_executed_plan_id = Some(p.id.clone());
                            Some(p)
                        });
                    if let Some(plan) = plan_opt {
                        if let Some(task) = plan.tasks.get((task_number - 1) as usize) {
                            use crate::adapters::tui::widgets::task_detail;
                            task_detail::render(
                                app_layout.chat_pane,
                                frame.buffer_mut(),
                                plan,
                                task,
                                theme,
                                app_layout.chat_pane.height,
                                state.task_panel_state.expanded_detail,
                                &mut state.task_panel_state.detail_scroll_offset,
                            );
                            skip_chat_render = true;
                        } else {
                            state.task_panel_state.drill_down_task = None;
                            state.task_panel_state.expanded_detail = false;
                            state.task_panel_state.detail_scroll_offset = 0;
                        }
                    } else {
                        state.task_panel_state.drill_down_task = None;
                        state.task_panel_state.expanded_detail = false;
                        state.task_panel_state.detail_scroll_offset = 0;
                        state.task_panel_state.last_executed_plan_id = None;
                    }
                }
                if !skip_chat_render {
                    // Extract view_state, open_turn, and clock from the active tab.
                    // If no tab manager is available, use defaults (render-only views
                    // like standalone conversation previews).
                    let default_vs = crate::domain::models::view_state::ViewState::default();
                    let default_clock = crate::domain::clock::SystemClock::default();
                    let (view_state_ref, open_turn_ref, clock_ref): (
                        &crate::domain::models::view_state::ViewState,
                        Option<&crate::domain::models::turn::Turn>,
                        &dyn crate::domain::clock::Clock,
                    ) = if let Some(tm) = tab_manager_for_bar {
                        let tab = tm.active_tab();
                        (
                            &tab.view_state,
                            tab.reducer.open_turn.as_ref(),
                            &*tab.clock as &dyn crate::domain::clock::Clock,
                        )
                    } else {
                        (&default_vs, None, &default_clock)
                    };
                    // Story 16.9: capture liveness snapshot for live rail rendering
                    let liveness_snapshot: Option<crate::domain::models::LivenessSnapshot> =
                        tab_manager_for_bar.map(|tm| tm.active_tab().reducer.liveness());
                    let liveness_ref: Option<&crate::domain::models::LivenessSnapshot> =
                        liveness_snapshot.as_ref();
                    let tab_id = state.active_tab_id;
                    let render_state = state.tab_render_states.entry(tab_id).or_default();
                    let result = chat_pane::render_with_search(
                        frame,
                        app_layout.chat_pane,
                        conversation,
                        open_turn_ref,
                        streaming,
                        view_state_ref,
                        clock_ref,
                        scroll_offset,
                        auto_scroll,
                        theme,
                        render_state,
                        tool_block_states,
                        feedback_blocks,
                        search_query_opt,
                        focused_match_opt,
                        search_state.matches.as_slice(),
                        bookmark_indices,
                        state.pending_plan_card.as_ref(),
                        liveness_ref,
                    );
                    content_height = result.total_content_height;
                    block_bounds = result.block_boundaries;
                    msg_bounds = result.message_boundaries;
                    user_msg_bounds = result.user_message_boundaries;
                    focused_tool_id = result.focused_tool_id;
                }

                // Story 4-4 AC1: render the search bar widget into its reserved slot.
                if let Some(search_bar_area) = app_layout.search_bar {
                    crate::adapters::tui::widgets::search_bar::render(
                        frame,
                        search_bar_area,
                        search_state,
                        theme,
                    );
                }

                // Story 4-4 AC5: render cross-search overlay inside the sidebar column.
                if state.cross_search.active {
                    if let Some(sidebar_area) = app_layout.sidebar {
                        crate::adapters::tui::widgets::cross_search::render(
                            frame,
                            sidebar_area,
                            &state.cross_search,
                            theme,
                        );
                    }
                }

                // Story 4-4 AC10: render the bookmark list panel into its reserved slot.
                if let Some(bookmark_panel_area) = app_layout.bookmark_panel {
                    let bookmarks: &[usize] = tab_manager_for_bar
                        .map(|tm| tm.active_tab().session_meta.bookmarks.as_slice())
                        .unwrap_or(&[]);
                    crate::adapters::tui::widgets::bookmark_list::render(
                        frame,
                        bookmark_panel_area,
                        conversation,
                        bookmarks,
                        state.bookmark_list_selected,
                        theme,
                    );
                }

                // Render peek overlay if any tool block has peek_active (AC5)
                for (tool_id, tbs) in tool_block_states.iter() {
                    if tbs.peek_active {
                        // Find the tool call info for this tool block
                        let tc = conversation
                            .messages
                            .iter()
                            .flat_map(|m| m.tool_calls.iter())
                            .chain(streaming.active_tool_calls.values())
                            .find(|tc| tc.id == *tool_id);
                        if let Some(tc) = tc {
                            let (paragraph, area) =
                                crate::adapters::tui::widgets::tool_block::render_peek_overlay(
                                    tc,
                                    theme,
                                    app_layout.chat_pane,
                                );
                            frame.render_widget(ratatui::widgets::Clear, area);
                            frame.render_widget(paragraph, area);
                        }
                        break; // Only one peek at a time
                    }
                }

                // Render permission prompt or feedback input at bottom of chat pane
                if let Some(ref feedback_input) = *pending_feedback_input {
                    use crate::adapters::tui::widgets::permission_prompt;
                    let prompt_lines = permission_prompt::render_feedback_input_lines(
                        &feedback_input.buffer,
                        theme,
                    );
                    let prompt_height = prompt_lines.len() as u16;
                    let prompt_area = ratatui::prelude::Rect {
                        x: app_layout.chat_pane.x,
                        y: app_layout.chat_pane.y + app_layout.chat_pane.height
                            - prompt_height.min(app_layout.chat_pane.height),
                        width: app_layout.chat_pane.width,
                        height: prompt_height.min(app_layout.chat_pane.height),
                    };
                    let paragraph = ratatui::widgets::Paragraph::new(prompt_lines);
                    frame.render_widget(ratatui::widgets::Clear, prompt_area);
                    frame.render_widget(paragraph, prompt_area);
                } else if let Some(pending) = pending_permission {
                    use crate::adapters::tui::widgets::permission_prompt;
                    let prompt_lines = permission_prompt::render_permission_lines(
                        &pending.source,
                        &pending.tool_name,
                        &pending.tool_input,
                        theme,
                        permission_queue.len(),
                    );
                    let prompt_height = prompt_lines.len() as u16;
                    let prompt_area = ratatui::prelude::Rect {
                        x: app_layout.chat_pane.x,
                        y: app_layout.chat_pane.y + app_layout.chat_pane.height
                            - prompt_height.min(app_layout.chat_pane.height),
                        width: app_layout.chat_pane.width,
                        height: prompt_height.min(app_layout.chat_pane.height),
                    };
                    let paragraph = ratatui::widgets::Paragraph::new(prompt_lines);
                    frame.render_widget(ratatui::widgets::Clear, prompt_area);
                    frame.render_widget(paragraph, prompt_area);
                }

                // Render skill trust prompt at bottom of chat pane (Story 5-2 AC4)
                if let Some(ref pending) = *pending_activation {
                    use crate::adapters::tui::widgets::skill_trust_prompt;
                    if skill_trust_inspect_mode {
                        let fallback = "(loading…)".to_string();
                        let content = state
                            .pending_activation_inspect_content
                            .as_ref()
                            .unwrap_or(&fallback);
                        let prompt_lines = skill_trust_prompt::render_inspect_lines(
                            &pending.skill_name,
                            content,
                            20,
                            theme,
                        );
                        let prompt_height = prompt_lines.len() as u16;
                        let prompt_area = ratatui::prelude::Rect {
                            x: app_layout.chat_pane.x,
                            y: app_layout.chat_pane.y + app_layout.chat_pane.height
                                - prompt_height.min(app_layout.chat_pane.height),
                            width: app_layout.chat_pane.width,
                            height: prompt_height.min(app_layout.chat_pane.height),
                        };
                        let paragraph = ratatui::widgets::Paragraph::new(prompt_lines);
                        frame.render_widget(ratatui::widgets::Clear, prompt_area);
                        frame.render_widget(paragraph, prompt_area);
                    } else {
                        let prompt_lines =
                            skill_trust_prompt::render_trust_lines(&pending.skill_name, theme, 0);
                        let prompt_height = prompt_lines.len() as u16;
                        let prompt_area = ratatui::prelude::Rect {
                            x: app_layout.chat_pane.x,
                            y: app_layout.chat_pane.y + app_layout.chat_pane.height
                                - prompt_height.min(app_layout.chat_pane.height),
                            width: app_layout.chat_pane.width,
                            height: prompt_height.min(app_layout.chat_pane.height),
                        };
                        let paragraph = ratatui::widgets::Paragraph::new(prompt_lines);
                        frame.render_widget(ratatui::widgets::Clear, prompt_area);
                        frame.render_widget(paragraph, prompt_area);
                    }
                } else if let Some(ref trust) = *pending_skill_trust {
                    use crate::adapters::tui::widgets::skill_trust_prompt;
                    if skill_trust_inspect_mode {
                        // H5: read from the pre-cached body populated when the user pressed `i`
                        // (populated via spawn_blocking in the SkillTrustInspect handler).
                        // Never read from disk on the render path.
                        let fallback = "(loading…)".to_string();
                        let content = trust.inspect_content.as_ref().unwrap_or(&fallback);
                        let prompt_lines = skill_trust_prompt::render_inspect_lines(
                            &trust.skill_name,
                            content,
                            20,
                            theme,
                        );
                        let prompt_height = prompt_lines.len() as u16;
                        let prompt_area = ratatui::prelude::Rect {
                            x: app_layout.chat_pane.x,
                            y: app_layout.chat_pane.y + app_layout.chat_pane.height
                                - prompt_height.min(app_layout.chat_pane.height),
                            width: app_layout.chat_pane.width,
                            height: prompt_height.min(app_layout.chat_pane.height),
                        };
                        let paragraph = ratatui::widgets::Paragraph::new(prompt_lines);
                        frame.render_widget(ratatui::widgets::Clear, prompt_area);
                        frame.render_widget(paragraph, prompt_area);
                    } else {
                        let prompt_lines = skill_trust_prompt::render_trust_lines(
                            &trust.skill_name,
                            theme,
                            skill_trust_queue.len(),
                        );
                        let prompt_height = prompt_lines.len() as u16;
                        let prompt_area = ratatui::prelude::Rect {
                            x: app_layout.chat_pane.x,
                            y: app_layout.chat_pane.y + app_layout.chat_pane.height
                                - prompt_height.min(app_layout.chat_pane.height),
                            width: app_layout.chat_pane.width,
                            height: prompt_height.min(app_layout.chat_pane.height),
                        };
                        let paragraph = ratatui::widgets::Paragraph::new(prompt_lines);
                        frame.render_widget(ratatui::widgets::Clear, prompt_area);
                        frame.render_widget(paragraph, prompt_area);
                    }
                }

                // Render AskUserQuestion card at bottom of chat pane if active
                if let Some(aq) = ask_user_question {
                    use crate::adapters::tui::widgets::ask_user_question;
                    let aq_lines = ask_user_question::render_ask_user_lines(
                        aq,
                        app_layout.chat_pane.width,
                        theme,
                    );
                    let aq_height = aq_lines.len() as u16;
                    let aq_area = ratatui::prelude::Rect {
                        x: app_layout.chat_pane.x,
                        y: app_layout.chat_pane.y + app_layout.chat_pane.height
                            - aq_height.min(app_layout.chat_pane.height),
                        width: app_layout.chat_pane.width,
                        height: aq_height.min(app_layout.chat_pane.height),
                    };
                    let paragraph = ratatui::widgets::Paragraph::new(aq_lines);
                    frame.render_widget(ratatui::widgets::Clear, aq_area);
                    frame.render_widget(paragraph, aq_area);
                }

                // Render fork confirmation card at bottom of chat pane if pending (Story 4-3a, AC1)
                if *focus
                    == FocusState::Overlay(
                        crate::domain::models::visual::OverlayType::Confirmation(
                            crate::domain::models::visual::ConfirmationType::Fork,
                        ),
                    )
                {
                    if let Some(fork_idx) = pending_fork_index {
                        use crate::adapters::tui::widgets::fork_confirm;
                        // Get message preview from conversation
                        let preview = conversation
                            .messages
                            .get(*fork_idx)
                            .map(|m| m.content.as_str())
                            .unwrap_or("");
                        let fork_lines = fork_confirm::render_fork_confirmation_lines(
                            preview,
                            *fork_idx,
                            app_layout.chat_pane.width,
                            theme,
                        );
                        let fork_height = fork_lines.len() as u16;
                        let fork_area = ratatui::prelude::Rect {
                            x: app_layout.chat_pane.x,
                            y: app_layout.chat_pane.y + app_layout.chat_pane.height
                                - fork_height.min(app_layout.chat_pane.height),
                            width: app_layout.chat_pane.width,
                            height: fork_height.min(app_layout.chat_pane.height),
                        };
                        let paragraph = ratatui::widgets::Paragraph::new(fork_lines);
                        frame.render_widget(ratatui::widgets::Clear, fork_area);
                        frame.render_widget(paragraph, fork_area);
                    }
                }

                // Render rewind confirmation card at bottom of chat pane if pending (Story 4-3b, AC1)
                if *focus
                    == FocusState::Overlay(
                        crate::domain::models::visual::OverlayType::Confirmation(
                            crate::domain::models::visual::ConfirmationType::Rewind,
                        ),
                    )
                {
                    let _ = pending_rewind_index; // used by handler; preview drives render
                    if let Some(preview) = rewind_preview {
                        use crate::adapters::tui::widgets::rewind_confirm;
                        let rewind_lines = rewind_confirm::render_rewind_confirmation_lines(
                            preview,
                            app_layout.chat_pane.width,
                            theme,
                        );
                        let rewind_height = rewind_lines.len() as u16;
                        let rewind_area = ratatui::prelude::Rect {
                            x: app_layout.chat_pane.x,
                            y: app_layout.chat_pane.y + app_layout.chat_pane.height
                                - rewind_height.min(app_layout.chat_pane.height),
                            width: app_layout.chat_pane.width,
                            height: rewind_height.min(app_layout.chat_pane.height),
                        };
                        let paragraph = ratatui::widgets::Paragraph::new(rewind_lines);
                        frame.render_widget(ratatui::widgets::Clear, rewind_area);
                        frame.render_widget(paragraph, rewind_area);
                    }
                }

                // Render plan approval card if pending (Story 6-0d AC4)
                if *focus
                    == FocusState::Overlay(
                        crate::domain::models::visual::OverlayType::Confirmation(
                            crate::domain::models::visual::ConfirmationType::PlanApproval,
                        ),
                    )
                {
                    if let Some(ref pending) = state.pending_plan_approval {
                        use crate::adapters::tui::widgets::plan_approval;
                        let card_area = plan_approval::plan_approval_area(frame.area());
                        plan_approval::render_plan_approval_card(frame, card_area, pending, theme);
                    }
                }

                let session_title = if !conversation.title.is_empty() {
                    Some(conversation.title.as_str())
                } else {
                    None
                };
                let drill_down_breadcrumb = state
                    .task_panel_state
                    .drill_down_task
                    .map(|n| format!("Tasks > Task {}", n));
                status_bar::render(
                    frame,
                    app_layout.status_bar,
                    display_model,
                    Some(provider_id),
                    status,
                    theme,
                    scroll_offset,
                    &msg_bounds,
                    content_height,
                    app_layout.chat_pane.height,
                    permission_mode,
                    token_usage.as_ref(),
                    has_project_context,
                    session_title,
                    multiline_mode,
                    current_hint.as_deref(),
                    state.active_skill_count,
                    state.active_agent_name.as_deref(),
                    state.pending_plan_reminder_at_turn,
                    drill_down_breadcrumb.as_deref(),
                    tab_manager_for_bar.is_some_and(|tm| {
                        matches!(
                            tm.active_tab().view_state.mode,
                            crate::domain::models::AnchorMode::Pinned(_)
                        )
                    }),
                );
                input_box::render(
                    frame,
                    app_layout.input_area,
                    input_buffer,
                    cursor_position,
                    focus.clone(),
                    theme,
                    multiline_mode,
                    input_scroll_offset,
                    image_indicator.as_deref(),
                );

                // Render reverse search overlay above input box
                if reverse_search.active {
                    reverse_search::render(frame, app_layout.input_area, reverse_search, theme);
                }

                // Render autocomplete popup above input box
                if autocomplete.active {
                    autocomplete_popup::render(frame, app_layout.input_area, autocomplete, theme);
                }

                // Render command palette overlay (centered, on top of everything)
                if command_palette.active {
                    command_palette_widget::render(frame, area, command_palette, theme);
                }

                // Render which-key hint bar (bottom of screen)
                if which_key.active {
                    which_key_bar::render(frame, area, which_key, theme);
                }

                // Render help overlay (full-screen modal, on top of everything)
                // Covers: FR108, UX-DR94
                if help_overlay.active {
                    help_overlay::render(
                        frame,
                        area,
                        help_overlay,
                        theme,
                        state.multiplexer_detected,
                    );
                }

                // Render model selector overlay (centered modal, Tier-1)
                // Story 7.2 AC1
                if model_selector.active {
                    model_selector::render(frame, area, model_selector, theme);
                }
            }
            None => {
                // Terminal too small
                let msg = ratatui::widgets::Paragraph::new("Terminal too small (min 60x16)")
                    .style(ratatui::prelude::Style::default().fg(ratatui::prelude::Color::Red))
                    .alignment(ratatui::prelude::Alignment::Center);
                frame.render_widget(msg, area);
            }
        }
    })?;

    state.total_content_height = content_height;
    state.block_boundaries = block_bounds;
    state.message_boundaries = msg_bounds;
    state.user_message_boundaries = user_msg_bounds;
    state.focused_tool_id = focused_tool_id;
    state.viewport_height = vp_height;

    // Resolve pending anchor from resize: use new heights to find correct scroll_offset.
    if let Some(anchor_idx) = state.pending_anchor.take() {
        if anchor_idx < state.block_boundaries.len() {
            let anchor_line = state.block_boundaries[anchor_idx];
            let vp = state.viewport_height as usize;
            let max_offset = content_height.saturating_sub(vp);
            state.scroll_snapshot = max_offset.saturating_sub(anchor_line);
            state.auto_snapshot = state.scroll_snapshot == 0;
        } else {
            // Anchor no longer valid (conversation changed during resize) — fall back to bottom
            state.scroll_snapshot = 0;
            state.auto_snapshot = true;
        }
    }

    Ok(())
}

/// Truncate a string to `max_len` characters, appending "…" if truncated.
fn truncate(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
    } else {
        let mut truncated: String = s.chars().take(max_len).collect();
        truncated.push('…');
        truncated
    }
}

/// Generate a concise title for a conversation using the LLM provider.
async fn generate_title(
    provider: &dyn StreamingProvider,
    model: &str,
    user_msg: &str,
    assistant_msg: &str,
) -> Result<String> {
    use crate::domain::models::{CompletionOptions, Message, MessageRole as MsgRole};

    let prompt_content = format!("User: {}\n\nAssistant: {}", user_msg, assistant_msg);
    let messages = vec![Message {
        role: MsgRole::User,
        content: prompt_content,
        images: vec![],
        tool_results: vec![],
        tool_uses: vec![],
        context_prefix: None,
    }];
    let options = CompletionOptions {
        model: model.to_string(),
        max_tokens: 30,
        system_prompt: "Generate a concise title (under 60 characters) for this conversation. Output only the title, no quotes or explanation.".to_string(),
        temperature: None,
        tools: vec![],
    };

    let stream = provider.stream_completion(messages, options).await?;
    futures::pin_mut!(stream);

    let mut title = String::new();
    while let Some(chunk) = stream.next().await {
        if let StreamChunk::Text { content, .. } = chunk {
            title.push_str(&content);
        }
    }

    // Post-process: trim, strip surrounding quotes, enforce 60-char max
    let title = post_process_title(&title);
    if title.is_empty() {
        anyhow::bail!("Title generation produced empty result");
    }
    Ok(title)
}

/// Post-process a generated title: trim whitespace, strip surrounding quotes,
/// enforce 60-character maximum with ellipsis truncation.
pub fn post_process_title(raw: &str) -> String {
    let mut title = raw.trim().to_string();
    // Strip surrounding quotes (both " and ') — uses strip_prefix/strip_suffix to avoid
    // byte-index panics on single-char or multi-byte inputs (review finding F3)
    if let Some(inner) = title.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        title = inner.to_string();
    } else if let Some(inner) = title.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        title = inner.to_string();
    }
    // Enforce 60-char max
    if title.chars().count() > 60 {
        title = title.chars().take(57).collect::<String>() + "...";
    }
    title
}

/// Populate autocomplete suggestions based on current state.
/// Called after handle_input when autocomplete is active.
async fn populate_autocomplete_suggestions(
    state: &mut TuiState,
    command_registry: &mut CommandRegistry,
    workspace_path: &std::path::Path,
) {
    use crate::domain::models::autocomplete::{AutocompleteKind, AutocompleteSuggestion};

    match state.autocomplete.kind {
        AutocompleteKind::SlashCommand => {
            // AC5: re-scan on popup open only (when filter_text is empty, meaning
            // the popup just transitioned from closed to open on the `/` keystroke).
            // Filter-as-you-type uses the cached registry.
            if state.autocomplete.filter_text.is_empty() {
                command_registry.refresh(workspace_path);
            }
            let filtered = command_registry.filter(&state.autocomplete.filter_text);
            let guard = state.skill_registry.read().await;
            if state.autocomplete.filter_text.is_empty() {
                command_registry.warn_skill_shadows(&guard);
            }
            let skill_results = guard.filter(&state.autocomplete.filter_text);

            state.autocomplete.suggestions =
                build_slash_suggestions_ordered(&filtered, &skill_results);
            // Reset selection if suggestions changed
            if state.autocomplete.selected_index >= state.autocomplete.suggestions.len() {
                state.autocomplete.selected_index = 0;
                state.autocomplete.scroll_offset = 0;
            }
        }
        AutocompleteKind::FileMention => {
            let results = file_scanner::scan_workspace_files(
                workspace_path,
                &state.autocomplete.filter_text,
                50,
            );
            state.autocomplete.suggestions = results
                .into_iter()
                .map(|f| AutocompleteSuggestion::FilePath {
                    path: f.relative_path,
                    is_dir: f.is_dir,
                })
                .collect();
            if state.autocomplete.selected_index >= state.autocomplete.suggestions.len() {
                state.autocomplete.selected_index = 0;
                state.autocomplete.scroll_offset = 0;
            }
        }
        AutocompleteKind::AgentMention => {
            state.refresh_agent_suggestions();
            let suggestions = state.agent_suggestions.clone();
            state.autocomplete.suggestions = suggestions;
            if state.autocomplete.selected_index >= state.autocomplete.suggestions.len() {
                state.autocomplete.selected_index = 0;
                state.autocomplete.scroll_offset = 0;
            }
        }
    }
}

/// Handle the result of a background skill scan, returning the optional registry
/// and any warning messages to log. Extracted from the spawn block for testability
/// (Story 5-1 P18 — timeout/panic paths were previously untested).
pub fn handle_scan_result(
    result: Result<Result<SkillRegistry, tokio::task::JoinError>, tokio::time::error::Elapsed>,
) -> (Option<SkillRegistry>, Vec<String>) {
    match result {
        Ok(Ok(registry)) => (Some(registry), vec![]),
        Ok(Err(join_err)) => (
            None,
            vec![format!("Skill discovery task panicked: {}", join_err)],
        ),
        Err(_) => (
            None,
            vec![format!(
                "Skill discovery timed out after {}s",
                BACKGROUND_TASK_TIMEOUT.as_secs()
            )],
        ),
    }
}

/// Merge filtered slash-command and skill results into a single ordered
/// autocomplete list. Order (Story 5-1 AC4): built-in commands first,
/// then skills (alphabetical), then user-defined commands (alphabetical).
pub fn build_slash_suggestions_ordered(
    command_results: &[&crate::adapters::command_registry::SlashCommandDef],
    skill_results: &[&crate::domain::models::SkillDef],
) -> Vec<crate::domain::models::autocomplete::AutocompleteSuggestion> {
    use crate::adapters::command_registry::CommandSource;
    use crate::domain::models::autocomplete::AutocompleteSuggestion;

    let mut suggestions: Vec<AutocompleteSuggestion> = Vec::new();

    // 1. Built-in commands (preserve registry order)
    for cmd in command_results {
        if matches!(cmd.source, CommandSource::BuiltIn) {
            suggestions.push(AutocompleteSuggestion::SlashCommand {
                name: cmd.name.clone(),
                description: cmd.description.clone(),
            });
        }
    }

    // 2. Skills (already alphabetical via SkillRegistry::filter)
    for s in skill_results {
        suggestions.push(AutocompleteSuggestion::Skill {
            name: s.name.clone(),
            description: s.description.clone(),
        });
    }

    // 3. User-defined commands
    for cmd in command_results {
        if matches!(cmd.source, CommandSource::UserDefined { .. }) {
            suggestions.push(AutocompleteSuggestion::SlashCommand {
                name: cmd.name.clone(),
                description: cmd.description.clone(),
            });
        }
    }

    suggestions
}

fn effective_model<'a>(state: &'a TuiState, config: &'a AppConfig) -> &'a str {
    state.selected_model.as_deref().unwrap_or(&config.model)
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
async fn apply_model_switch(
    state: &mut TuiState,
    router: &crate::adapters::provider::ProviderRouter,
    app_state: &crate::infrastructure::runtime::app_state::AppState,
    streaming: &StreamingState,
    domain_tx: &tokio::sync::mpsc::UnboundedSender<AppEvent>,
    conversation: &Conversation,
    provider_id: Option<String>,
    model_id: String,
    pending_health_check: &mut Option<(
        String,
        String,
        tokio::task::JoinHandle<Result<(), crate::domain::errors::ProviderError>>,
    )>,
) {
    use crate::domain::events::AppEvent;
    use crate::domain::models::NoticeLevel;

    let resolved_pid = match provider_id {
        Some(pid) => pid,
        None => match app_state.provider_registry.get_model_provider(&model_id) {
            Some(pid) => pid,
            None => {
                let _ = domain_tx.send(AppEvent::SystemNotice {
                    conversation_id: None,
                    level: NoticeLevel::Warning,
                    message: format!("Unknown model: {}", model_id),
                });
                return;
            }
        },
    };

    if streaming.is_streaming {
        let _ = domain_tx.send(AppEvent::SystemNotice {
            conversation_id: None,
            level: NoticeLevel::Info,
            message: "Cannot switch model while streaming".to_string(),
        });
        return;
    }

    let provider_desc = app_state
        .provider_registry
        .list_providers()
        .into_iter()
        .find(|p| p.provider_id == resolved_pid);
    let is_healthy = provider_desc.as_ref().is_some_and(|p| p.healthy);
    let provider_display_name = provider_desc
        .as_ref()
        .map(|p| p.display_name.clone())
        .unwrap_or_else(|| resolved_pid.clone());

    if !is_healthy {
        if let Some(provider) = router.get_provider(&resolved_pid) {
            state.model_selector.connecting = Some(provider_display_name.clone());
            state.needs_redraw = true;
            let pid_clone = resolved_pid.clone();
            let mid_clone = model_id.clone();
            let handle = tokio::spawn(async move { provider.health_check().await });
            *pending_health_check = Some((pid_clone, mid_clone, handle));
            return;
        } else {
            let fb = crate::domain::models::FeedbackBlock {
                id: generate_conversation_id(),
                level: crate::domain::models::FeedbackLevel::Warning,
                message: format!("Provider '{}' not found", resolved_pid),
                actions: vec![],
            };
            state.feedback_blocks.insert(fb.id.clone(), fb);
            state.needs_redraw = true;
            return;
        }
    }

    // If triggered from palette (model selector not open), open it for context-warning display
    let model_desc = app_state
        .provider_registry
        .get_model(&resolved_pid, &model_id);
    let context_window = model_desc.as_ref().map_or(0, |m| m.context_window);
    let model_display_name = model_desc
        .as_ref()
        .map(|m| m.display_name.clone())
        .unwrap_or_else(|| model_id.clone());

    if !state.model_selector.active && model_desc.is_some() {
        let providers = app_state.provider_registry.list_providers();
        let columns: Vec<crate::adapters::tui::state::ProviderColumn> = providers
            .into_iter()
            .map(|pd| {
                let models = app_state
                    .provider_registry
                    .list_models_by_provider(&pd.provider_id);
                crate::adapters::tui::state::ProviderColumn {
                    provider_id: pd.provider_id,
                    display_name: pd.display_name,
                    healthy: pd.healthy,
                    models,
                }
            })
            .filter(|c| !c.models.is_empty())
            .collect();
        if !columns.is_empty() {
            state
                .model_selector
                .open(state.focus.clone(), columns, &resolved_pid, &model_id);
            state.focus = crate::domain::models::FocusState::Overlay(
                crate::domain::models::visual::OverlayType::ModelSelector,
            );
        }
    }

    if let Some(ref warning) = state.model_selector.pending_context_warning {
        if warning.model_id == model_id && warning.provider_id == resolved_pid {
            state.model_selector.pending_context_warning = None;
        }
    } else {
        let current_tokens = conversation.usage.as_ref().map_or(0, |u| u.input_tokens);
        if context_window > 0 && current_tokens > context_window {
            state.model_selector.pending_context_warning =
                Some(crate::adapters::tui::state::ContextWarning {
                    provider_id: resolved_pid.clone(),
                    model_id: model_id.clone(),
                    model_display_name: model_display_name.clone(),
                    context_window,
                    current_tokens,
                });
            state.needs_redraw = true;
            return;
        }
    }

    complete_model_switch(
        state,
        router,
        app_state,
        conversation,
        &resolved_pid,
        &model_id,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn complete_model_switch(
    state: &mut TuiState,
    router: &crate::adapters::provider::ProviderRouter,
    app_state: &crate::infrastructure::runtime::app_state::AppState,
    _conversation: &Conversation,
    resolved_pid: &str,
    model_id: &str,
) {
    use crate::adapters::tui::widgets::model_selector::humanize_ctx;

    if resolved_pid != router.active_delegate_id() {
        if let Err(e) = router.set_active(resolved_pid) {
            let fb = crate::domain::models::FeedbackBlock {
                id: generate_conversation_id(),
                level: crate::domain::models::FeedbackLevel::Warning,
                message: format!("Switch failed: {}", e),
                actions: vec![],
            };
            state.feedback_blocks.insert(fb.id.clone(), fb);
            state.needs_redraw = true;
            return;
        }
    }

    state.selected_model = Some(model_id.to_string());

    let model_desc = app_state
        .provider_registry
        .get_model(resolved_pid, model_id);
    let context_window = model_desc.as_ref().map_or(0, |m| m.context_window);
    let provider_display_name = app_state
        .provider_registry
        .list_providers()
        .into_iter()
        .find(|p| p.provider_id == resolved_pid)
        .map(|p| p.display_name)
        .unwrap_or_else(|| resolved_pid.to_string());
    let model_display_name = model_desc
        .as_ref()
        .map(|m| m.display_name.clone())
        .unwrap_or_else(|| model_id.to_string());

    let fb = crate::domain::models::FeedbackBlock {
        id: generate_conversation_id(),
        level: crate::domain::models::FeedbackLevel::Info,
        message: format!(
            "Switched to {}/{} (context: {})",
            provider_display_name,
            model_display_name,
            humanize_ctx(context_window)
        ),
        actions: vec![],
    };
    state.feedback_blocks.insert(fb.id.clone(), fb);

    // Story 7.3 AC7: warn when switching to a non-tool model
    if let Some(ref md) = model_desc {
        use crate::domain::models::provider::ModelCapability;
        if !md.capabilities.contains(&ModelCapability::ToolUse) {
            let warning_fb = crate::domain::models::FeedbackBlock {
                id: generate_conversation_id(),
                level: crate::domain::models::FeedbackLevel::Warning,
                message: format!(
                    "{} does not support tool use. Tool execution will be unavailable.",
                    md.display_name
                ),
                actions: vec![],
            };
            state
                .feedback_blocks
                .insert(warning_fb.id.clone(), warning_fb);
        }
    }

    if state.model_selector.active {
        state.focus = state
            .model_selector
            .dismiss()
            .unwrap_or(crate::domain::models::FocusState::Input);
    }
    state.needs_redraw = true;
}

/// One-shot startup convenience: if the active provider is unhealthy,
/// fall back to the first healthy registered provider (deterministic — sorted).
/// Runtime provider failures are surfaced via the existing apply_model_switch path.
async fn apply_startup_provider_fallback(
    state: &mut TuiState,
    router: &crate::adapters::provider::ProviderRouter,
    app_state: &crate::infrastructure::runtime::app_state::AppState,
    _domain_tx: &tokio::sync::mpsc::UnboundedSender<AppEvent>,
) {
    let active = router.active_delegate_id();
    let providers = app_state.provider_registry.list_providers();
    let active_healthy = providers
        .iter()
        .find(|p| p.provider_id == active)
        .is_some_and(|p| p.healthy);

    if active_healthy {
        return;
    }

    let mut healthy_ids: Vec<String> = providers
        .iter()
        .filter(|p| p.healthy)
        .map(|p| p.provider_id.clone())
        .collect();
    healthy_ids.sort();

    if let Some(healthy_id) = healthy_ids.into_iter().next() {
        if let Err(e) = router.set_active(&healthy_id) {
            tracing::warn!("Failed to set active provider to '{}': {}", healthy_id, e);
            return;
        }
        if let Some(first_model) = app_state
            .provider_registry
            .list_models_by_provider(&healthy_id)
            .first()
        {
            state.selected_model = Some(first_model.model_id.clone());
        }
        app_state.event_bus.emit_domain(AppEvent::SystemNotice {
            conversation_id: None,
            level: NoticeLevel::Info,
            message: format!(
                "Active provider '{}' unavailable — using '{}'.",
                active, healthy_id
            ),
        });
    } else {
        app_state.event_bus.emit_domain(AppEvent::SystemNotice {
            conversation_id: None,
            level: NoticeLevel::Warning,
            message: "No provider is reachable. Start a provider (e.g. `ollama serve`) or open the model selector with Ctrl+X, M.".to_string(),
        });
    }
}

/// Populate command palette filtered entries from the registry.
/// Called after handle_input when palette is active and filter text changes.
async fn populate_palette_entries(
    state: &mut TuiState,
    command_registry: &mut CommandRegistry,
    palette_registry: &mut PaletteRegistry,
    workspace_path: &std::path::Path,
) {
    // Ensure command registry is discovered
    if !command_registry.is_discovered() {
        command_registry.refresh(workspace_path);
        let guard = state.skill_registry.read().await;
        command_registry.warn_skill_shadows(&guard);
    }

    // Populate palette from command registry (cached, only rebuilds if discovery state changed)
    palette_registry.populate_from_command_registry(command_registry);

    // Determine scope from filter prefix
    let (scope, query) = if let Some(palette_scope) = state.command_palette.current_scope {
        // Strip prefix character from query
        let q = if state.command_palette.filter_text.len() > 1 {
            state
                .command_palette
                .filter_text
                .chars()
                .skip(1)
                .collect::<String>()
        } else {
            String::new()
        };
        (Some(palette_scope), q)
    } else {
        (None, state.command_palette.filter_text.clone())
    };

    // Fuzzy filter entries
    let filtered = palette_registry.fuzzy_filter(&query, scope);
    let entries: Vec<_> = filtered.into_iter().cloned().collect();
    state.command_palette.filtered_entries = entries;

    // Clamp selection and scroll_offset to valid range after filter update
    let max_idx = state
        .command_palette
        .filtered_entries
        .len()
        .saturating_sub(1);
    state.command_palette.selected_index = state.command_palette.selected_index.min(max_idx);
    state.command_palette.scroll_offset = state.command_palette.scroll_offset.min(max_idx);
}

/// Resolve file mentions and build context prefix for a message submission.
/// Reads file contents in the adapter layer (no file I/O in domain).
/// Errors encountered during file context resolution, for FeedbackBlock display.
struct FileContextError {
    #[allow(dead_code)]
    path: String,
    reason: String,
}

/// Resolve what content to copy based on current focus state.
/// Priority: focused tool block output > last assistant message > empty.
// Covers: FR116 (AC6, AC8, AC9)
fn resolve_copy_content(state: &TuiState, conversation: &Conversation) -> String {
    // AC8: If a tool block is focused, copy its output
    if let Some(ref tool_id) = state.focused_tool_id {
        // Find the tool result in conversation messages
        for cm in conversation.messages.iter().rev() {
            for tc in &cm.tool_calls {
                if tc.id == *tool_id {
                    if let Some(ref result) = tc.result {
                        return result.content.clone();
                    }
                }
            }
        }
    }

    // AC9: Copy the last assistant message
    for cm in conversation.messages.iter().rev() {
        if cm.role == MessageRole::Assistant && !cm.content.is_empty() {
            return cm.content.clone();
        }
    }

    String::new()
}

fn resolve_file_context(
    mentions: &[crate::adapters::tui::state::ResolvedMention],
    workspace_path: &std::path::Path,
    security: &dyn SecurityPort,
) -> (
    Vec<message_builder::ResolvedFileContext>,
    Vec<ImageAttachment>,
    Vec<FileContextError>,
) {
    use crate::adapters::tui::image;
    use crate::domain::models::FileOperation;

    let mut resolved = Vec::new();
    let mut images = Vec::new();
    let mut errors = Vec::new();
    for mention in mentions {
        let full_path = workspace_path.join(&mention.path);

        // P3: Validate path is within workspace before reading
        let canonical = match full_path.canonicalize() {
            Ok(p) => p,
            Err(_) => {
                errors.push(FileContextError {
                    path: mention.path.clone(),
                    reason: format!("File not found: {}", mention.path),
                });
                continue;
            }
        };
        if let Err(e) = security.check_workspace_access(&canonical, FileOperation::Read) {
            tracing::warn!("Security check failed for mention {}: {}", mention.path, e);
            errors.push(FileContextError {
                path: mention.path.clone(),
                reason: format!("Access denied: {}", mention.path),
            });
            continue;
        }

        // Check if file is an image by extension
        // Covers: FR112 (AC2, AC3)
        let is_image = canonical
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| image::is_image_extension(ext));

        if is_image {
            // P2: Check file size before reading to prevent OOM on large images
            const MAX_IMAGE_FILE_SIZE: u64 = 20 * 1024 * 1024; // 20MB
            match std::fs::metadata(&canonical) {
                Ok(meta) if meta.len() > MAX_IMAGE_FILE_SIZE => {
                    errors.push(FileContextError {
                        path: mention.path.clone(),
                        reason: format!(
                            "Image file too large ({:.1}MB). Maximum: 20MB",
                            meta.len() as f64 / (1024.0 * 1024.0)
                        ),
                    });
                    continue;
                }
                Err(_) => {
                    errors.push(FileContextError {
                        path: mention.path.clone(),
                        reason: format!("Could not read file: {}", mention.path),
                    });
                    continue;
                }
                _ => {} // Size OK, proceed
            }
            // Read as bytes, validate format, base64-encode
            match std::fs::read(&canonical) {
                Ok(bytes) => match image::detect_image_format(&bytes) {
                    Ok(media_type) => {
                        use base64::Engine;
                        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
                        images.push(ImageAttachment {
                            media_type: media_type.to_string(),
                            data,
                        });
                    }
                    Err(msg) => {
                        errors.push(FileContextError {
                            path: mention.path.clone(),
                            reason: msg,
                        });
                    }
                },
                Err(_) => {
                    errors.push(FileContextError {
                        path: mention.path.clone(),
                        reason: format!("Could not read file: {}", mention.path),
                    });
                }
            }
        } else {
            // Non-image files: read as text (existing behavior)
            match std::fs::read_to_string(&canonical) {
                Ok(content) => {
                    // P2: Limit to ~100KB per file — use floor_char_boundary to avoid UTF-8 panic
                    #[allow(clippy::incompatible_msrv)] // stable since 1.80, MSRV is 1.85
                    let truncated = if content.len() > 100_000 {
                        let boundary = content.floor_char_boundary(100_000);
                        content[..boundary].to_string()
                    } else {
                        content
                    };
                    resolved.push(message_builder::ResolvedFileContext {
                        path: mention.path.clone(),
                        content: truncated,
                    });
                }
                Err(_) => {
                    errors.push(FileContextError {
                        path: mention.path.clone(),
                        reason: format!("File not found: {}", mention.path),
                    });
                }
            }
        }
    }
    (resolved, images, errors)
}

/// Resolve a user-defined slash command's content.
fn resolve_command_context(
    cmd_name: &str,
    command_args: Option<&str>,
    workspace: &std::path::Path,
    security: &dyn crate::domain::ports::SecurityPort,
    command_registry: &CommandRegistry,
) -> (
    Option<message_builder::ResolvedCommandContext>,
    Vec<crate::adapters::command_registry::FileRefError>,
) {
    use crate::adapters::command_registry;
    use crate::domain::services::command_interpolation;

    let cmd_def = match command_registry.find(cmd_name) {
        Some(d) => d,
        None => return (None, vec![]),
    };
    let content = match cmd_def.content.as_ref() {
        Some(c) => c,
        None => return (None, vec![]),
    };

    let args = command_args.unwrap_or("");
    let interpolated = command_interpolation::substitute_command_args(content, args);
    let (resolved_body, file_errors) =
        command_registry::resolve_file_refs(&interpolated, workspace, security);

    (
        Some(message_builder::ResolvedCommandContext {
            name: cmd_name.to_string(),
            content: resolved_body,
        }),
        file_errors,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warning_notice_does_not_transfer_focus() {
        let mut state = TuiState::new(80, 24);
        state.focus = FocusState::Input;
        let fb_id = apply_warning_notice(&mut state, "Auto-skipped task 3".to_string());
        assert_eq!(
            state.focus,
            FocusState::Input,
            "Warning notice must not transfer focus from Input"
        );
        assert_eq!(
            state.active_feedback_id,
            Some(fb_id.clone()),
            "Warning notice must set active_feedback_id"
        );
        assert!(
            state.feedback_blocks.contains_key(&fb_id),
            "Warning notice must insert feedback block"
        );
    }

    #[test]
    fn test_post_process_title_trims_whitespace() {
        assert_eq!(post_process_title("  Hello World  "), "Hello World");
    }

    #[test]
    fn test_post_process_title_strips_double_quotes() {
        assert_eq!(post_process_title("\"Quoted Title\""), "Quoted Title");
    }

    #[test]
    fn test_post_process_title_strips_single_quotes() {
        assert_eq!(post_process_title("'Single Quoted'"), "Single Quoted");
    }

    #[test]
    fn test_post_process_title_no_strip_mismatched_quotes() {
        assert_eq!(post_process_title("\"Mismatched'"), "\"Mismatched'");
    }

    #[test]
    fn test_post_process_title_single_char_quote_no_panic() {
        // F3: previously panicked with title[1..0] on single-char quote
        assert_eq!(post_process_title("\""), "\"");
        assert_eq!(post_process_title("'"), "'");
    }

    #[test]
    fn test_post_process_title_empty_inside_quotes() {
        assert_eq!(post_process_title("\"\""), "");
        assert_eq!(post_process_title("''"), "");
    }

    #[test]
    fn test_post_process_title_truncates_at_60_chars() {
        let long = "A".repeat(70);
        let result = post_process_title(&long);
        assert_eq!(result.len(), 60); // 57 A's + "..."
        assert!(result.ends_with("..."));
    }

    #[test]
    fn test_post_process_title_preserves_under_60() {
        let short = "Short Title";
        assert_eq!(post_process_title(short), "Short Title");
    }

    #[test]
    fn test_post_process_title_exactly_60_chars() {
        let exact = "A".repeat(60);
        assert_eq!(post_process_title(&exact), exact);
    }

    #[test]
    fn test_truncate_short_string() {
        assert_eq!(truncate("Hello", 10), "Hello");
    }

    #[test]
    fn test_truncate_long_string() {
        let long = "A".repeat(600);
        let result = truncate(&long, 500);
        assert_eq!(result.chars().count(), 501); // 500 chars + '…'
        assert!(result.ends_with('…'));
    }

    #[test]
    fn test_truncate_exact_length() {
        let exact = "A".repeat(500);
        assert_eq!(truncate(&exact, 500), exact);
    }

    // P1 (party-mode review 2026-04-12): base64 size guard in persist_image_attachments
    #[test]
    fn test_persist_image_attachments_rejects_oversized_base64() {
        use crate::adapters::filesystem::FileSystemStorage;
        use crate::domain::models::ImageAttachment;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));

        // Synthesize a base64 string that exceeds MAX_BASE64_ENCODED_SIZE (27MB).
        // 28MB of 'A' characters (all-same base64 input is fine for this test).
        let oversized_base64 = "A".repeat(28 * 1024 * 1024);
        let attachments = vec![ImageAttachment {
            media_type: "image/png".to_string(),
            data: oversized_base64,
        }];
        let refs = persist_image_attachments("test-conv-p1", &storage, &attachments);
        assert!(
            refs.is_empty(),
            "oversized base64 attachment must be dropped, got {} refs",
            refs.len()
        );
    }

    // P1: normal-sized attachment passes the guard and produces a ref.
    // Requires multi_thread flavor because persist_image_attachments uses
    // block_in_place internally (pre-existing DF-097).
    #[tokio::test(flavor = "multi_thread")]
    async fn test_persist_image_attachments_accepts_valid_small_attachment() {
        use crate::adapters::filesystem::FileSystemStorage;
        use crate::domain::models::ImageAttachment;
        use base64::{Engine, engine::general_purpose::STANDARD};
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));

        // 4 bytes of PNG → valid small attachment
        let data = STANDARD.encode(b"\x89PNG");
        let attachments = vec![ImageAttachment {
            media_type: "image/png".to_string(),
            data,
        }];
        let refs = persist_image_attachments("test-conv-p1-ok", &storage, &attachments);
        assert_eq!(
            refs.len(),
            1,
            "valid small attachment must produce one ImageReference"
        );
        assert!(refs[0].file_name.ends_with(".png"));
    }

    // ── Addendum 2 (2026-04-12): Multi-turn image rehydration ──────────
    // See `rehydrate_historical_images` for the full rationale. These tests
    // cover the unit-level contract: given a Conversation with persisted
    // ImageReferences, the output `Vec<Message>` must carry the bytes as
    // ImageAttachment blocks on the matching User messages.

    fn mk_chat_msg_user_with_images(
        content: &str,
        images: Vec<crate::domain::models::ImageReference>,
    ) -> ChatMessage {
        ChatMessage {
            id: generate_conversation_id(),
            role: MessageRole::User,
            content: content.to_string(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: 1_700_000_000,
            token_count: None,
            stop_reason: None,
            synthetic: false,
            images,
        }
    }

    fn mk_chat_msg_assistant(content: &str) -> ChatMessage {
        ChatMessage {
            id: generate_conversation_id(),
            role: MessageRole::Assistant,
            content: content.to_string(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: 1_700_000_001,
            token_count: None,
            stop_reason: None,
            synthetic: false,
            images: vec![],
        }
    }

    fn mk_conversation(id: &str, messages: Vec<ChatMessage>) -> Conversation {
        Conversation {
            id: id.to_string(),
            title: "rehydrate test".to_string(),
            messages,
            turns: Vec::new(),
            created_at: 1_700_000_000,
            updated_at: 1_700_000_001,
            last_response_at: None,
            session_id: None,
            usage: None,
            plans: std::collections::HashMap::new(),
            fork_source: None,
        }
    }

    /// Multi-turn happy path: image saved on turn 1 is rehydrated into the
    /// API request on turn 2. This is the core regression test for the
    /// reported bug ("assistant forgets image on next turn").
    #[tokio::test(flavor = "multi_thread")]
    async fn test_rehydrate_historical_images_multi_turn_happy_path() {
        use crate::adapters::filesystem::{FileSystemStorage, content_hash, normalize_extension};
        use crate::domain::models::ImageReference;
        use base64::{Engine, engine::general_purpose::STANDARD};
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));
        let conv_id = "conv-rehydrate-happy";

        // Given: a user turn-1 message with an image persisted on disk.
        let bytes: Vec<u8> = b"\x89PNG\r\n\x1a\n-turn1-payload".to_vec();
        let file_name = format!(
            "{}.{}",
            content_hash(&bytes),
            normalize_extension("image/png")
        );
        let image_ref = ImageReference {
            file_name: file_name.clone(),
            media_type: "image/png".to_string(),
            original_size: bytes.len(),
        };
        storage
            .save_image(conv_id, &image_ref, &bytes)
            .await
            .unwrap();

        // Conversation state on turn 2: turn-1 user msg (with persisted ref),
        // turn-1 assistant reply, turn-2 user msg (fresh text, no images).
        let conv = mk_conversation(
            conv_id,
            vec![
                mk_chat_msg_user_with_images("what is this?", vec![image_ref.clone()]),
                mk_chat_msg_assistant("a cat"),
                mk_chat_msg_user_with_images("what colour?", vec![]),
            ],
        );

        let mut messages = message_builder::build_api_messages(&conv);
        assert_eq!(messages.len(), 3);
        assert!(messages[0].images.is_empty(), "pre-rehydrate: empty");

        // When: rehydration runs
        rehydrate_historical_images(&conv, &mut messages, &storage);

        // Then: turn-1 user message carries the image bytes as base64 in
        // its API image attachment, and turn-2 remains empty (no ref).
        assert_eq!(messages[0].role, MessageRole::User);
        assert_eq!(messages[0].images.len(), 1, "turn-1 must be rehydrated");
        assert_eq!(messages[0].images[0].media_type, "image/png");
        let expected_b64 = STANDARD.encode(&bytes);
        assert_eq!(messages[0].images[0].data, expected_b64);

        assert_eq!(messages[2].role, MessageRole::User);
        assert!(messages[2].images.is_empty(), "turn-2 has no image ref");
    }

    /// A message whose `images` vec is already populated (e.g. fresh turn
    /// attachment happened before rehydrate ran) must NOT be touched. This
    /// guards against double-attaching the same image twice in the same
    /// API request.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_rehydrate_skips_already_populated_messages() {
        use crate::adapters::filesystem::{FileSystemStorage, content_hash, normalize_extension};
        use crate::domain::models::ImageReference;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));
        let conv_id = "conv-rehydrate-skip";

        let bytes: Vec<u8> = b"\x89PNG\r\n\x1a\n-skip".to_vec();
        let file_name = format!(
            "{}.{}",
            content_hash(&bytes),
            normalize_extension("image/png")
        );
        let image_ref = ImageReference {
            file_name,
            media_type: "image/png".to_string(),
            original_size: bytes.len(),
        };
        storage
            .save_image(conv_id, &image_ref, &bytes)
            .await
            .unwrap();

        let conv = mk_conversation(
            conv_id,
            vec![mk_chat_msg_user_with_images("hi", vec![image_ref.clone()])],
        );

        let mut messages = message_builder::build_api_messages(&conv);
        // Pre-populate with a sentinel "fresh" attachment — rehydrate must leave it alone.
        messages[0].images.push(ImageAttachment {
            media_type: "image/png".to_string(),
            data: "SENTINEL-FRESH".to_string(),
        });

        rehydrate_historical_images(&conv, &mut messages, &storage);

        assert_eq!(messages[0].images.len(), 1, "must not double-attach");
        assert_eq!(messages[0].images[0].data, "SENTINEL-FRESH");
    }

    /// Missing image file on disk must not panic or fail the turn — just
    /// warn and skip. This extends AC4 graceful degradation to the API
    /// request path.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_rehydrate_missing_file_degrades_gracefully() {
        use crate::adapters::filesystem::FileSystemStorage;
        use crate::domain::models::ImageReference;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));
        let conv_id = "conv-rehydrate-missing";

        // Dangling reference: valid shape (16 hex chars + .png) but no file.
        let image_ref = ImageReference {
            file_name: "deadbeefdeadbeef.png".to_string(),
            media_type: "image/png".to_string(),
            original_size: 42,
        };

        let conv = mk_conversation(
            conv_id,
            vec![mk_chat_msg_user_with_images("ghost", vec![image_ref])],
        );

        let mut messages = message_builder::build_api_messages(&conv);
        rehydrate_historical_images(&conv, &mut messages, &storage);

        assert_eq!(messages.len(), 1);
        assert!(
            messages[0].images.is_empty(),
            "missing image must be dropped, not panicked"
        );
    }

    /// Index-parity regression: when an assistant message with tool-call
    /// results sits between two user messages, `build_api_messages` merges
    /// tool results into the next User message (anthropic API forbids consecutive
    /// User roles). Rehydration must still land images on turn-1's User message.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_rehydrate_index_parity_with_tool_results() {
        use crate::adapters::filesystem::{FileSystemStorage, content_hash, normalize_extension};
        use crate::domain::models::ImageReference;
        use crate::domain::models::{ToolCallInfo, ToolResultInfo};
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));
        let conv_id = "conv-rehydrate-tools";

        let bytes: Vec<u8> = b"\x89PNG\r\n\x1a\n-tools".to_vec();
        let file_name = format!(
            "{}.{}",
            content_hash(&bytes),
            normalize_extension("image/png")
        );
        let image_ref = ImageReference {
            file_name,
            media_type: "image/png".to_string(),
            original_size: bytes.len(),
        };
        storage
            .save_image(conv_id, &image_ref, &bytes)
            .await
            .unwrap();

        // turn-1: user with image
        // turn-1: assistant with a tool call + result
        // turn-2: user with no image
        let mut assistant_with_tool = mk_chat_msg_assistant("calling tool");
        assistant_with_tool.tool_calls.push(ToolCallInfo {
            id: "tool-1".to_string(),
            name: "Read".to_string(),
            input: serde_json::json!({"path": "x"}),
            result: Some(ToolResultInfo {
                content: "file contents".to_string(),
                is_error: false,
            }),
            started_at_ms: None,
            completed_at_ms: None,
            status: None,
        });
        let conv = mk_conversation(
            conv_id,
            vec![
                mk_chat_msg_user_with_images("analyse this", vec![image_ref.clone()]),
                assistant_with_tool,
                mk_chat_msg_user_with_images("more?", vec![]),
            ],
        );

        let mut messages = message_builder::build_api_messages(&conv);
        // Tool results are merged into turn-2's User message (anthropic forbids consecutive User roles).
        // Layout: [User(analyse, image), Assistant(calling), User(more?, tool_results)]
        assert_eq!(
            messages.len(),
            3,
            "tool results merged into next User message"
        );

        rehydrate_historical_images(&conv, &mut messages, &storage);

        // Image lands on messages[0] (turn-1 user).
        assert_eq!(
            messages[0].images.len(),
            1,
            "image must rehydrate onto turn-1 user message"
        );
        // Tool results land on messages[2] (turn-2 user, merged).
        assert_eq!(
            messages[2].tool_results.len(),
            1,
            "tool results must appear on turn-2 user message"
        );
        assert!(
            messages[2].images.is_empty(),
            "turn-2 user message has no image ref"
        );
    }

    #[test]
    fn test_no_rx_await_in_event_loop_source() {
        let source = include_str!("event_loop.rs");
        let pat = "\x72\x78\x2E\x61\x77\x61\x69\x74";
        for (i, line) in source.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("//") || trimmed.starts_with("//!") {
                continue;
            }
            if trimmed.starts_with("fn test_no_rx_await") {
                break;
            }
            assert!(
                !trimmed.contains(pat),
                "event_loop.rs line {}: event loop invariant violation",
                i + 1,
            );
        }
    }
}
