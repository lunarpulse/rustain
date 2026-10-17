//! Task-editor parsing and reorder state mutations. The caller retains continue ownership.

use super::event_bus::EventBus;
use crate::adapters::tui::state::{Direction, TuiState};
use crate::domain::events::AppEvent;
use crate::domain::models::{Conversation, NoticeLevel, PlanTaskStatus};

pub(crate) fn parse_task_edit(content: &str) -> (String, Vec<String>) {
    let mut title = String::new();
    let mut description = Vec::new();
    let mut reading_desc = false;
    for line in content.lines() {
        if line.starts_with('#') && !reading_desc {
            continue;
        }
        if line == "description: |" {
            reading_desc = true;
        } else if reading_desc {
            if let Some(rest) = line.strip_prefix("  ") {
                description.push(rest.to_string());
            } else if line.is_empty() {
                description.push(String::new());
            } else {
                reading_desc = false;
            }
        } else if let Some(rest) = line.strip_prefix("title: ") {
            title = rest.to_string();
        } else if !line.trim().is_empty() && title.is_empty() {
            title = line.to_string();
        }
    }
    (title, description)
}

/// False means the original root arm must continue without its trailing work.
pub(crate) fn enter_reorder(
    state: &mut TuiState,
    conversation: &Conversation,
    number: u32,
    event_bus: &EventBus,
) -> bool {
    let Some(plan_id) = state.task_panel_state.last_executed_plan_id.as_ref() else {
        let _ = event_bus.emit_domain(AppEvent::SystemNotice {
            conversation_id: Some(conversation.id.clone()),
            level: NoticeLevel::Warning,
            message: "No active plan.".to_string(),
        });
        return false;
    };
    if let Some(plan) = conversation.plans.get(plan_id) {
        let index = number.saturating_sub(1) as usize;
        if index < plan.tasks.len() && plan.tasks[index].status == PlanTaskStatus::Pending {
            state.task_panel_state.reorder_mode_for = Some(number);
            state.task_panel_state.reorder_original_order =
                Some(plan.tasks.iter().map(|task| task.number).collect());
        } else {
            let _ = event_bus.emit_domain(AppEvent::SystemNotice {
                conversation_id: Some(conversation.id.clone()),
                level: NoticeLevel::Warning,
                message: "Reorder requires a pending task selected.".to_string(),
            });
        }
    }
    state.needs_redraw = true;
    true
}

pub(crate) fn move_reorder(
    state: &mut TuiState,
    conversation: &mut Conversation,
    direction: Direction,
    event_bus: &EventBus,
) -> bool {
    let Some(plan_id) = state.task_panel_state.last_executed_plan_id.as_ref() else {
        return false;
    };
    let Some(number) = state.task_panel_state.reorder_mode_for else {
        return false;
    };
    if let Some(plan) = conversation.plans.get_mut(plan_id)
        && let Some(index) = plan.tasks.iter().position(|task| task.number == number)
    {
        let new_index = match direction {
            Direction::Up => index.saturating_sub(1),
            Direction::Down => {
                if index + 1 < plan.tasks.len() {
                    index + 1
                } else {
                    index
                }
            }
            Direction::Left | Direction::Right => index,
        };
        if index == new_index {
            return false;
        }
        if let Err(reason) =
            crate::domain::services::plan_runtime::validate_reorder(plan, number, new_index)
        {
            let _ = event_bus.emit_domain(AppEvent::SystemNotice {
                conversation_id: Some(conversation.id.clone()),
                level: NoticeLevel::Warning,
                message: format!("Reorder violates dependencies: {}.", reason),
            });
        } else {
            let task = plan.tasks.remove(index);
            plan.tasks.insert(new_index, task);
        }
    }
    state.needs_redraw = true;
    true
}

pub(crate) fn commit_reorder(
    state: &mut TuiState,
    conversation: &Conversation,
    event_bus: &EventBus,
) {
    if let Some(plan_id) = state.task_panel_state.last_executed_plan_id.as_ref()
        && let Some(plan) = conversation.plans.get(plan_id)
    {
        for task in &plan.tasks {
            let _ = event_bus.emit_domain(AppEvent::PlanTaskStatusChanged {
                conversation_id: conversation.id.clone(),
                plan_id: plan_id.clone(),
                task_number: task.number,
                status: PlanTaskStatus::Pending,
            });
        }
    }
    state.task_panel_state.reorder_mode_for = None;
    state.task_panel_state.reorder_original_order = None;
    let _ = event_bus.emit_domain(AppEvent::SystemNotice {
        conversation_id: Some(conversation.id.clone()),
        level: NoticeLevel::Info,
        message: "Plan reordered.".to_string(),
    });
    state.needs_redraw = true;
}

pub(crate) fn cancel_reorder(state: &mut TuiState, conversation: &mut Conversation) {
    if let (Some(plan_id), Some(original)) = (
        state.task_panel_state.last_executed_plan_id.as_ref(),
        state.task_panel_state.reorder_original_order.take(),
    ) && let Some(plan) = conversation.plans.get_mut(plan_id)
    {
        let mut restored = Vec::with_capacity(original.len());
        for number in original {
            if let Some(index) = plan.tasks.iter().position(|task| task.number == number) {
                restored.push(plan.tasks.remove(index));
            }
        }
        plan.tasks = restored;
    }
    state.task_panel_state.reorder_mode_for = None;
    state.task_panel_state.reorder_original_order = None;
    state.needs_redraw = true;
}
