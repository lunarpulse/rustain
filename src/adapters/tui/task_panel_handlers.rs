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
use crate::domain::models::{Conversation, NoticeLevel};

/// Notice the caller should emit via the event bus, scoped to a target
/// conversation id (set on `AppEvent::SystemNotice.conversation_id`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingNotice {
    pub level: NoticeLevel,
    pub message: String,
    pub conversation_id: String,
}

/// Mirrors the `AppEvent::PlanExecutionStarted` arm. A plan that starts opens nothing (Story
/// 21.7 A19, OD-2: the Tasks sidebar it used to auto-open is gone, and the operator opens
/// Mission Control himself); the plan the Tasks tab resolves first follows it. Returns whether
/// the event concerned this conversation.
pub fn handle_plan_execution_started(
    state: &mut TuiState,
    conversation: &Conversation,
    event_conversation_id: &str,
    plan_id: &str,
) -> bool {
    if event_conversation_id != conversation.id || !conversation.plans.contains_key(plan_id) {
        return false;
    }
    state.task_panel_state.last_executed_plan_id = Some(plan_id.to_string());
    state.needs_redraw = true;
    true
}

/// Mirrors the `AppEvent::PlanTaskStatusChanged` arm. Returns true iff the event matched the
/// active conversation and a known plan; the screen is repainted when a Tasks surface is
/// open.
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
    if crate::adapters::tui::mission_control::tasks_surface_open(state) {
        state.needs_redraw = true;
    }
    true
}

/// The clipboard payload of one task and the status-specific success notice the task window's
/// `c` shows (DF-277: a `Cancelled` task has its own wording). `None` if the plan/task could
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
    let task = plan.tasks.iter().find(|t| t.number == task_number)?;
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
                _ => "Copied cancel reason to clipboard.".to_string(),
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

/// Story 6.4: handle pause/resume of one task of `plan_id` (the plan a `TaskRef` names, never
/// the "last executed" one: Story 21.7 A11). Returns notices the caller should emit.
#[derive(Debug, Clone)]
pub struct PauseOutcome {
    pub notices: Vec<PendingNotice>,
    pub should_resume_advance: bool,
    /// True if a Running task was paused (requires token cancel from caller).
    pub running_task_paused: Option<u32>,
}

pub fn handle_task_pause(
    conversation: &mut Conversation,
    plan_id: &str,
    task_number: u32,
) -> PauseOutcome {
    let conversation_id = conversation.id.clone();
    let Some(plan) = conversation.plans.get_mut(plan_id) else {
        return PauseOutcome {
            notices: vec![PendingNotice {
                level: NoticeLevel::Info,
                message: "No active plan.".to_string(),
                conversation_id,
            }],
            should_resume_advance: false,
            running_task_paused: None,
        };
    };
    let Some(idx) = plan.tasks.iter().position(|t| t.number == task_number) else {
        return PauseOutcome {
            notices: vec![PendingNotice {
                level: NoticeLevel::Info,
                message: format!("Task {} not found.", task_number),
                conversation_id,
            }],
            should_resume_advance: false,
            running_task_paused: None,
        };
    };

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

/// Story 6.4: handle skip of one task of `plan_id` (the plan a `TaskRef` names).
pub fn handle_task_skip(
    state: &mut TuiState,
    conversation: &mut Conversation,
    plan_id: &str,
    task_number: u32,
) -> Vec<PendingNotice> {
    let Some(plan) = conversation.plans.get_mut(plan_id) else {
        return vec![PendingNotice {
            level: NoticeLevel::Info,
            message: "No active plan.".to_string(),
            conversation_id: conversation.id.clone(),
        }];
    };

    let Some(idx) = plan.tasks.iter().position(|t| t.number == task_number) else {
        return vec![PendingNotice {
            level: NoticeLevel::Info,
            message: format!("Task {} not found.", task_number),
            conversation_id: conversation.id.clone(),
        }];
    };

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
                // Raise the cascade decision (Requested arrival)
                crate::adapters::tui::handlers::plan_decisions::arrive_skip_cascade(
                    state,
                    &conversation.id,
                    crate::adapters::tui::decision_queue::SkipCascadePending {
                        plan_id: plan_id.to_string(),
                        source_task: task_number,
                        source_prior_status: prior_status,
                        source_prior_error: plan.tasks[idx].error.clone(),
                        downstream,
                    },
                );
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
                delegated_to: None,
                sub_tasks: vec![],
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
            peer_row_controls: Default::default(),
            peer_row_revision: 0,
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
