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

    if user_suppressed && terminal_width >= sidebar_min_width && auto_open_setting != "none" {
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
                message: "Tasks panel hidden for this session — Ctrl+X, T to reopen.".to_string(),
                conversation_id: conversation.id.clone(),
            });
        }
    } else if auto_open_setting != "none" && terminal_width >= sidebar_min_width {
        let was_closed = !state.sidebar_visible || state.sidebar_panel != Some(PanelType::Tasks);
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
            message: "Task panel auto-open skipped \u{2014} terminal too narrow (<120 cols). \
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

/// Returns the status of the currently drilled-down task, or `None` if no
/// drill-down is active or the task/plan cannot be resolved.
/// Used to gate status-conditional dispatch for `TaskPause`, `TaskSkip`, `TaskRetry`,
/// and `TaskEdit` — Story 6.4 replaces the 6-3 `is_failed_drill_down` gate.
pub fn drill_down_task_status(
    state: &TuiState,
    conversation: &Conversation,
) -> Option<PlanTaskStatus> {
    let n = state.task_panel_state.drill_down_task?;
    let plan_id = state.task_panel_state.last_executed_plan_id.as_ref()?;
    let plan = conversation.plans.get(plan_id)?;
    plan.tasks
        .get(n.saturating_sub(1) as usize)
        .map(|t| t.status)
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
            format!("Task {}: {} — {:?}", task.number, task.title, task.status),
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

/// Story 6.4: compute transitively-dependent task numbers.
/// Performs a reverse `depends_on` walk starting from `task_number`.
/// Defensively guards against cycles (should never occur).
pub fn dependents(plan: &crate::domain::models::plan::Plan, task_number: u32) -> Vec<u32> {
    let mut result: Vec<u32> = Vec::new();
    let mut queue: Vec<u32> = vec![task_number];
    let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::new();
    seen.insert(task_number);
    while let Some(current) = queue.pop() {
        for task in &plan.tasks {
            if task.depends_on.contains(&current) && seen.insert(task.number) {
                result.push(task.number);
                queue.push(task.number);
            }
        }
    }
    result
}

/// Story 6.4: handle pause/resume from the task panel or drill-down view.
/// Returns notices the caller should emit via the event bus.
#[derive(Debug, Clone)]
pub struct PauseOutcome {
    pub notices: Vec<PendingNotice>,
    pub should_resume_advance: bool,
    /// True if a Running task was paused (requires token cancel from caller).
    pub running_task_paused: Option<u32>,
}

pub fn handle_task_pause(
    state: &mut TuiState,
    conversation: &mut Conversation,
    task_number: u32,
) -> PauseOutcome {
    let plan_id = match state.task_panel_state.last_executed_plan_id.as_ref() {
        Some(id) => id.clone(),
        None => {
            return PauseOutcome {
                notices: vec![PendingNotice {
                    level: NoticeLevel::Info,
                    message: "No active plan.".to_string(),
                    conversation_id: conversation.id.clone(),
                }],
                should_resume_advance: false,
                running_task_paused: None,
            };
        }
    };

    let plan = match conversation.plans.get_mut(&plan_id) {
        Some(p) => p,
        None => {
            return PauseOutcome {
                notices: vec![PendingNotice {
                    level: NoticeLevel::Info,
                    message: "No active plan.".to_string(),
                    conversation_id: conversation.id.clone(),
                }],
                should_resume_advance: false,
                running_task_paused: None,
            };
        }
    };

    let idx = (task_number.saturating_sub(1)) as usize;
    if idx >= plan.tasks.len() {
        return PauseOutcome {
            notices: vec![PendingNotice {
                level: NoticeLevel::Info,
                message: format!("Task {} not found.", task_number),
                conversation_id: conversation.id.clone(),
            }],
            should_resume_advance: false,
            running_task_paused: None,
        };
    }

    let task_status = plan.tasks[idx].status;

    match task_status {
        PlanTaskStatus::Paused => {
            // Resume: flip Paused back to Pending (and transitive dependents)
            let resume_set = {
                let mut set = vec![task_number];
                let deps = dependents(plan, task_number);
                // Only resume dependents that are also Paused
                for dep in &deps {
                    let dep_idx = (dep.saturating_sub(1)) as usize;
                    if dep_idx < plan.tasks.len()
                        && plan.tasks[dep_idx].status == PlanTaskStatus::Paused
                    {
                        set.push(*dep);
                    }
                }
                set
            };

            let mut count = 0u32;
            for tn in &resume_set {
                let t_idx = (tn.saturating_sub(1)) as usize;
                if t_idx >= plan.tasks.len() {
                    continue;
                }
                let was_running = plan.tasks[t_idx].started_at_ms.is_some()
                    && plan.tasks[t_idx].completed_at_ms.is_none();
                plan.tasks[t_idx].status = PlanTaskStatus::Pending;
                if was_running {
                    plan.tasks[t_idx].started_at_ms = None;
                }
                count += 1;
            }

            PauseOutcome {
                notices: vec![PendingNotice {
                    level: NoticeLevel::Info,
                    message: format!("Resumed {} task(s).", count),
                    conversation_id: conversation.id.clone(),
                }],
                should_resume_advance: true,
                running_task_paused: None,
            }
        }
        PlanTaskStatus::Pending => {
            // Pause: flip to Paused directly (no token cancel needed)
            plan.tasks[idx].status = PlanTaskStatus::Paused;
            let deps = dependents(plan, task_number);
            for dep in &deps {
                let dep_idx = (dep.saturating_sub(1)) as usize;
                if dep_idx < plan.tasks.len()
                    && !matches!(
                        plan.tasks[dep_idx].status,
                        PlanTaskStatus::Completed
                            | PlanTaskStatus::Failed
                            | PlanTaskStatus::Skipped
                            | PlanTaskStatus::Cancelled
                    )
                {
                    plan.tasks[dep_idx].status = PlanTaskStatus::Paused;
                }
            }
            PauseOutcome {
                notices: vec![],
                should_resume_advance: false,
                running_task_paused: None,
            }
        }
        PlanTaskStatus::Running => {
            // Pause: mark pause pending, then caller cancels the token
            let deps = dependents(plan, task_number);
            // Set dependents to Paused synchronously (they're not running)
            for dep in &deps {
                let dep_idx = (dep.saturating_sub(1)) as usize;
                if dep_idx < plan.tasks.len()
                    && !matches!(
                        plan.tasks[dep_idx].status,
                        PlanTaskStatus::Completed
                            | PlanTaskStatus::Failed
                            | PlanTaskStatus::Skipped
                            | PlanTaskStatus::Cancelled
                    )
                {
                    plan.tasks[dep_idx].status = PlanTaskStatus::Paused;
                }
            }
            PauseOutcome {
                notices: vec![PendingNotice {
                    level: NoticeLevel::Info,
                    message: format!("Pausing Task {}...", task_number),
                    conversation_id: conversation.id.clone(),
                }],
                should_resume_advance: false,
                running_task_paused: Some(task_number),
            }
        }
        _ => PauseOutcome {
            notices: vec![PendingNotice {
                level: NoticeLevel::Info,
                message: format!(
                    "Task {} cannot be paused (status: {:?}).",
                    task_number, task_status
                ),
                conversation_id: conversation.id.clone(),
            }],
            should_resume_advance: false,
            running_task_paused: None,
        },
    }
}

/// Story 6.4: handle skip from the task panel or drill-down view.
pub fn handle_task_skip(
    state: &mut TuiState,
    conversation: &mut Conversation,
    task_number: u32,
) -> Vec<PendingNotice> {
    let plan_id = match state.task_panel_state.last_executed_plan_id.as_ref() {
        Some(id) => id.clone(),
        None => {
            return vec![PendingNotice {
                level: NoticeLevel::Info,
                message: "No active plan.".to_string(),
                conversation_id: conversation.id.clone(),
            }];
        }
    };

    let plan = match conversation.plans.get_mut(&plan_id) {
        Some(p) => p,
        None => {
            return vec![PendingNotice {
                level: NoticeLevel::Info,
                message: "No active plan.".to_string(),
                conversation_id: conversation.id.clone(),
            }];
        }
    };

    let idx = (task_number.saturating_sub(1)) as usize;
    if idx >= plan.tasks.len() {
        return vec![PendingNotice {
            level: NoticeLevel::Info,
            message: format!("Task {} not found.", task_number),
            conversation_id: conversation.id.clone(),
        }];
    }

    let task_status = plan.tasks[idx].status;

    match task_status {
        PlanTaskStatus::Pending | PlanTaskStatus::Failed => {
            let downstream = dependents(plan, task_number);
            let prior_status = task_status;
            let now_ms = chrono::Utc::now().timestamp_millis();

            plan.tasks[idx].status = PlanTaskStatus::Skipped;
            plan.tasks[idx].completed_at_ms = Some(now_ms);
            plan.tasks[idx].error = Some("Skipped by user".to_string());

            // Emit status change for the source task
            let mut notices = vec![PendingNotice {
                level: NoticeLevel::Info,
                message: format!("Task {} skipped.", task_number),
                conversation_id: conversation.id.clone(),
            }];

            if downstream.is_empty() {
                // No dependents → skip is final
                notices.push(PendingNotice {
                    level: NoticeLevel::Info,
                    message: "Skipped — advancing.".to_string(),
                    conversation_id: conversation.id.clone(),
                });
            } else {
                // Show cascade card
                state.task_panel_state.skip_cascade_pending =
                    Some(crate::adapters::tui::state::SkipCascadePending {
                        plan_id: plan_id.clone(),
                        source_task: task_number,
                        source_prior_status: prior_status,
                        source_prior_error: plan.tasks[idx].error.clone(),
                        downstream,
                    });
            }

            notices
        }
        _ => {
            vec![PendingNotice {
                level: NoticeLevel::Info,
                message: format!(
                    "Task {} cannot be skipped (status: {:?}).",
                    task_number, task_status
                ),
                conversation_id: conversation.id.clone(),
            }]
        }
    }
}

/// Story 6.4: resolve a selected_index (0-based, from panel cursor) to a task number.
/// Returns None if no plan or index out of bounds.
pub fn resolve_panel_task_number(
    state: &TuiState,
    conversation: &Conversation,
    selected_index: u32,
) -> Option<u32> {
    let plan_id = state.task_panel_state.last_executed_plan_id.as_ref()?;
    let plan = conversation.plans.get(plan_id)?;
    let idx = selected_index as usize;
    plan.tasks.get(idx).map(|t| t.number)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::plan::{Plan, PlanTask, TaskResult};
    use std::collections::HashMap;

    fn conv_with_completed_task(text: String) -> (Conversation, &'static str) {
        let plan_id = "p1";
        let plan = Plan {
            id: plan_id.to_string(),
            title: "Plan".to_string(),
            tasks: vec![PlanTask {
                number: 1,
                title: "T".to_string(),
                description: String::new(),
                depends_on: vec![],
                status: PlanTaskStatus::Completed,
                started_at_ms: Some(0),
                completed_at_ms: Some(1),
                result: Some(TaskResult {
                    text,
                    tool_call_count: 0,
                    token_count: None,
                }),
                error: None,
                waiting_on: vec![],
            }],
            estimated_effort: None,
            status: PlanStatus::Completed,
            created_at: 0,
            resolved_at: None,
            host_message_id: None,
        };
        let mut conv = Conversation {
            id: "c1".to_string(),
            title: "T".to_string(),
            messages: vec![],
            turns: Vec::new(),
            created_at: 0,
            updated_at: 0,
            last_response_at: None,
            session_id: None,
            usage: None,
            plans: HashMap::new(),
            fork_source: None,
            compaction: None,
        };
        conv.plans.insert(plan_id.to_string(), plan);
        (conv, plan_id)
    }

    // Story 6.3-FU3 AC4: with storage no longer truncating at 4 KiB, the
    // [c] Copy result clipboard payload must equal the full result text.
    #[test]
    fn fu3_copy_result_emits_full_text() {
        let body = "z".repeat(20_000);
        let (conv, plan_id) = conv_with_completed_task(body.clone());

        let outcome =
            resolve_copy_task_payload(&conv, Some(plan_id), 1, None).expect("payload resolved");

        assert_eq!(outcome.text.len(), 20_000, "clipboard payload length");
        assert_eq!(outcome.text, body, "clipboard payload byte-equal");
        assert!(
            !outcome.text.contains("(truncated)"),
            "no truncation marker on clipboard"
        );
    }
}
