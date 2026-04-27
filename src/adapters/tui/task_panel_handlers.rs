//! Task panel event-arm helpers — Story 6.3 (PD5 conformance extraction).
//!
//! Pure-ish state mutators extracted from `infrastructure/runtime/event_loop.rs`
//! so the AC1/AC2/AC8 acceptance criteria can be exercised by `tests/conformance_task_panel.rs`
//! without needing the full `tokio::select!` event loop scaffolding.
//!
//! Each helper:
//! - takes `&mut TuiState` and a `&Conversation` (read-only)
//! - mutates panel-related fields on the state
//! - returns a structured outcome describing any `SystemNotice`s the caller
//!   should emit via `event_bus.emit_domain(...)`
//!
//! The event-loop arms call these helpers and then dispatch the returned
//! notices, so behavior is identical to inlined code.

use crate::adapters::tui::state::TuiState;
use crate::domain::models::plan::{PlanStatus, PlanTaskStatus};
use crate::domain::models::visual::PanelType;
use crate::domain::models::{Conversation, NoticeLevel};

/// Notice the caller should emit via the event bus, scoped to a target
/// conversation id (set on `AppEvent::SystemNotice.conversation_id`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingNotice {
    pub level: NoticeLevel,
    pub message: String,
    pub conversation_id: String,
}

/// Outcome of `handle_open_panel`. `opened == true` means the visibility flip
/// landed; `opened == false` with a `notice` means the chord was rejected
/// (narrow terminal); `opened == false` without a notice means the chord
/// toggled an already-open panel closed.
#[derive(Debug, Default, Clone)]
pub struct OpenPanelOutcome {
    pub opened: bool,
    pub closed: bool,
    pub notice: Option<PendingNotice>,
}

/// Mirrors the `InputAction::OpenPanel(Tasks)` arm for the **Tasks** panel
/// only (history/agents/adapters route through other handlers). Returns
/// the visibility transition + any rejection notice.
pub fn handle_open_panel_tasks(
    state: &mut TuiState,
    conversation: &Conversation,
    terminal_width: u16,
    sidebar_min_width: u16,
) -> OpenPanelOutcome {
    if state.sidebar_visible && state.sidebar_panel == Some(PanelType::Tasks) {
        // Toggle close.
        state.sidebar_visible = false;
        state.sidebar_panel = None;
        state.task_panel_state.drill_down_task = None;
        state.task_panel_state.expanded_detail = false;
        state.task_panel_state.detail_scroll_offset = 0;
        // PD1: explicit close suppresses future auto-open for this conversation.
        state
            .task_panel_state
            .auto_open_suppressed_conversations
            .insert(conversation.id.clone());
        state.needs_redraw = true;
        OpenPanelOutcome {
            opened: false,
            closed: true,
            notice: None,
        }
    } else if terminal_width >= sidebar_min_width {
        state.sidebar_visible = true;
        state.sidebar_panel = Some(PanelType::Tasks);
        state.sidebar_selected = state.task_panel_state.selected_index;
        // PD1: explicit re-open clears suppression so the next plan can auto-open again.
        state
            .task_panel_state
            .auto_open_suppressed_conversations
            .remove(&conversation.id);
        let resolved = crate::adapters::tui::widgets::task_panel::resolve_panel_plan(
            conversation,
            state.task_panel_state.last_executed_plan_id.as_deref(),
        );
        state.task_panel_state.task_count = resolved.map(|p| p.tasks.len()).unwrap_or(0);
        state.needs_redraw = true;
        OpenPanelOutcome {
            opened: true,
            closed: false,
            notice: None,
        }
    } else {
        OpenPanelOutcome {
            opened: false,
            closed: false,
            notice: Some(PendingNotice {
                level: NoticeLevel::Warning,
                message: "Task panel requires terminal width >= 120 cols.".to_string(),
                conversation_id: conversation.id.clone(),
            }),
        }
    }
}

/// Outcome of `handle_plan_execution_started`. The notices vec is in
/// dispatch order. `auto_opened == true` means visibility flipped to open;
/// `suppressed == true` means user-suppression honored (Sally Option 1).
#[derive(Debug, Default, Clone)]
pub struct PlanExecutionStartedOutcome {
    pub auto_opened: bool,
    pub suppressed: bool,
    pub narrow_skipped: bool,
    pub notices: Vec<PendingNotice>,
}

/// Mirrors the `AppEvent::PlanExecutionStarted` arm. `auto_open_setting`
/// is the resolved value of `auto_panels.on_task_plan` (currently
/// hard-coded `"tasks"` until PD4's schema lands; the helper accepts it as
/// a parameter so AC1's `auto_open_suppressed_by_config` test can pass
/// `"none"` and verify the no-op branch).
pub fn handle_plan_execution_started(
    state: &mut TuiState,
    conversation: &Conversation,
    event_conversation_id: &str,
    plan_id: &str,
    terminal_width: u16,
    sidebar_min_width: u16,
    auto_open_setting: &str,
) -> PlanExecutionStartedOutcome {
    let mut out = PlanExecutionStartedOutcome::default();
    if event_conversation_id != conversation.id {
        return out;
    }
    let user_suppressed = state
        .task_panel_state
        .auto_open_suppressed_conversations
        .contains(&conversation.id);
    let task_count = conversation
        .plans
        .get(plan_id)
        .map(|p| p.tasks.len())
        .unwrap_or(0);

    if user_suppressed
        && terminal_width >= sidebar_min_width
        && auto_open_setting != "none"
    {
        // PD1 (Sally Option 1): honor user close. Update plan pointer + cursor
        // so a manual reopen renders correctly.
        state.task_panel_state.last_executed_plan_id = Some(plan_id.to_string());
        state.task_panel_state.selected_index = 0;
        state.task_panel_state.drill_down_task = None;
        state.task_panel_state.expanded_detail = false;
        state.task_panel_state.detail_scroll_offset = 0;
        state.task_panel_state.auto_open_skipped_for_plan = None;
        state.task_panel_state.task_count = task_count;
        out.suppressed = true;
        // PD1 (Sally A2): one-time hint toast per conversation.
        if state
            .task_panel_state
            .auto_open_hint_shown_for
            .insert(conversation.id.clone())
        {
            out.notices.push(PendingNotice {
                level: NoticeLevel::Info,
                message: "Tasks panel hidden for this session — Ctrl+X, T to reopen."
                    .to_string(),
                conversation_id: conversation.id.clone(),
            });
        }
    } else if auto_open_setting != "none" && terminal_width >= sidebar_min_width {
        let was_closed = !state.sidebar_visible
            || state.sidebar_panel != Some(PanelType::Tasks);
        state.sidebar_visible = true;
        state.sidebar_panel = Some(PanelType::Tasks);
        state.task_panel_state.last_executed_plan_id = Some(plan_id.to_string());
        state.task_panel_state.selected_index = 0;
        state.task_panel_state.drill_down_task = None;
        state.task_panel_state.expanded_detail = false;
        state.task_panel_state.detail_scroll_offset = 0;
        state.task_panel_state.auto_open_skipped_for_plan = None;
        state.task_panel_state.task_count = task_count;
        state.needs_redraw = true;
        out.auto_opened = true;
        if was_closed {
            out.notices.push(PendingNotice {
                level: NoticeLevel::Info,
                message: "Task panel opened. Press Esc to dismiss.".to_string(),
                conversation_id: conversation.id.clone(),
            });
        }
    } else if terminal_width < sidebar_min_width
        && state.task_panel_state.auto_open_skipped_for_plan.as_deref() != Some(plan_id)
    {
        state.task_panel_state.auto_open_skipped_for_plan = Some(plan_id.to_string());
        out.narrow_skipped = true;
        out.notices.push(PendingNotice {
            level: NoticeLevel::Info,
            message:
                "Task panel auto-open skipped \u{2014} terminal too narrow (<120 cols). \
                 Press Ctrl+X, T after resize."
                    .to_string(),
            conversation_id: conversation.id.clone(),
        });
    }
    out
}

/// Mirrors the `AppEvent::PlanTaskStatusChanged` arm. Returns true iff
/// `needs_redraw` was bumped (i.e. the panel is open and the event
/// matched the active conversation + a known plan).
pub fn handle_plan_task_status_changed(
    state: &mut TuiState,
    conversation: &Conversation,
    event_conversation_id: &str,
    plan_id: &str,
) -> bool {
    if event_conversation_id != conversation.id {
        return false;
    }
    if !conversation.plans.contains_key(plan_id) {
        return false;
    }
    state.task_panel_state.last_executed_plan_id = Some(plan_id.to_string());
    if state.sidebar_panel == Some(PanelType::Tasks) {
        state.needs_redraw = true;
        true
    } else {
        false
    }
}

/// `true` iff the user is currently drilled into a task whose status is `Failed`.
/// Used to gate `InputAction::ReservedKey6_4` so the "Coming in 6.4" notice
/// only fires when the action row actually advertises `r/s/e`.
pub fn is_failed_drill_down(state: &TuiState, conversation: &Conversation) -> bool {
    let Some(n) = state.task_panel_state.drill_down_task else {
        return false;
    };
    let Some(plan_id) = state.task_panel_state.last_executed_plan_id.as_ref() else {
        return false;
    };
    let Some(plan) = conversation.plans.get(plan_id) else {
        return false;
    };
    plan.tasks
        .get(n.saturating_sub(1) as usize)
        .map(|t| t.status == PlanTaskStatus::Failed)
        .unwrap_or(false)
}

/// Resolves the clipboard payload for `InputAction::CopyTaskResult`. Returns
/// the (text, status_flash_message) pair, or `None` if the plan/task could
/// not be resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyTaskOutcome {
    pub text: String,
    pub flash_message: String,
}

pub fn resolve_copy_task_payload(
    conversation: &Conversation,
    plan_id: Option<&str>,
    task_number: u32,
    last_executed_plan_id: Option<&str>,
) -> Option<CopyTaskOutcome> {
    let resolved_plan_id = plan_id.or(last_executed_plan_id)?;
    let plan = conversation.plans.get(resolved_plan_id)?;
    let idx = (task_number.saturating_sub(1)) as usize;
    let task = plan.tasks.get(idx)?;
    let (text, flash_message) = match task.status {
        PlanTaskStatus::Completed => (
            task.result
                .as_ref()
                .map(|r| r.text.clone())
                .unwrap_or_else(|| format!("Task {}: (no result captured)", task.number)),
            "Copied result to clipboard.".to_string(),
        ),
        PlanTaskStatus::Failed | PlanTaskStatus::Skipped | PlanTaskStatus::Cancelled => (
            task.error
                .clone()
                .unwrap_or_else(|| format!("Task {}: (no error message)", task.number)),
            match task.status {
                PlanTaskStatus::Failed => "Copied error to clipboard.".to_string(),
                PlanTaskStatus::Skipped => "Copied skip reason to clipboard.".to_string(),
                _ => "Copied to clipboard.".to_string(),
            },
        ),
        _ => (
            format!(
                "Task {}: {} — {:?}",
                task.number, task.title, task.status
            ),
            "Copied task info to clipboard.".to_string(),
        ),
    };
    Some(CopyTaskOutcome {
        text,
        flash_message,
    })
}

/// Convenience: any plan in the conversation with at least one Running task.
/// Used by the 4Hz tick branch (PD2) to decide whether to bump `needs_redraw`.
pub fn any_plan_has_running_task(conversation: &Conversation) -> bool {
    conversation.plans.values().any(|p| {
        p.status == PlanStatus::Executing
            && p.tasks.iter().any(|t| t.status == PlanTaskStatus::Running)
    })
}
