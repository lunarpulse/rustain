//! Sidebar and periodic display state mutations; dispatch and rendering stay in run.

use super::room_bridge;
use crate::adapters::tui::{layout, state::TuiState, task_panel_handlers};
use crate::domain::models::visual::PanelType;
use crate::domain::models::{Conversation, FocusState, NoticeLevel, StatusState};

pub(crate) enum PanelOpenOutcome {
    Opened,
    Notice(task_panel_handlers::PendingNotice),
    Handled,
}

pub(crate) fn open_panel(
    state: &mut TuiState,
    conversation: &Conversation,
    panel_type: PanelType,
) -> PanelOpenOutcome {
    if panel_type == PanelType::Tasks {
        let terminal_width = state.terminal_width;
        let outcome = task_panel_handlers::handle_open_panel_tasks(
            state,
            conversation,
            terminal_width,
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
            PanelOpenOutcome::Notice(notice)
        } else {
            PanelOpenOutcome::Handled
        }
    } else if state.sidebar_visible && state.sidebar_panel == Some(panel_type) {
        state.sidebar_visible = false;
        state.sidebar_panel = None;
        state.sidebar_entry_count = 0;
        state.focus = FocusState::Chat;
        state.needs_redraw = true;
        PanelOpenOutcome::Handled
    } else if state.terminal_width >= layout::SIDEBAR_MIN_WIDTH {
        state.sidebar_visible = true;
        state.sidebar_panel = Some(panel_type);
        if panel_type == PanelType::Adapters {
            state.sidebar_entry_count =
                crate::adapters::tui::widgets::adapter_status_panel::port_count();
            if state.sidebar_selected >= state.sidebar_entry_count {
                state.sidebar_selected = 0;
            }
        } else if panel_type == PanelType::Agents {
            state.sidebar_entry_count = state.agent_panel_state.cached_entries.len();
            if state.sidebar_selected >= state.sidebar_entry_count && state.sidebar_entry_count > 0
            {
                state.sidebar_selected = state.sidebar_entry_count - 1;
            }
        }
        PanelOpenOutcome::Opened
    } else {
        let message = if panel_type == PanelType::Adapters {
            "Adapter Status: terminal too narrow for sidebar — widen to ≥120 cols or use /memory|/persona|... slash commands"
        } else if panel_type == PanelType::Agents {
            "Subagent panel: terminal too narrow for sidebar — widen to ≥120 cols."
        } else {
            "Panel requires terminal width >= 120 cols."
        };
        PanelOpenOutcome::Notice(task_panel_handlers::PendingNotice {
            conversation_id: conversation.id.clone(),
            level: NoticeLevel::Warning,
            message: message.to_string(),
        })
    }
}

/// Advance only poll state; the root loop owns the durable-head read.
pub(crate) fn room_head_poll_due(state: &mut TuiState, tick_ms: u64) -> bool {
    if state.sidebar_visible && state.sidebar_panel == Some(PanelType::Room) {
        state
            .room_panel
            .head_poll_due(tick_ms, room_bridge::ROOM_HEAD_POLL_INTERVAL_MS)
    } else {
        state.room_panel.reset_head_poll();
        false
    }
}

pub(crate) fn tick_status(state: &mut TuiState, conversation: &Conversation, tick_ms: u64) {
    if let StatusState::Executing { elapsed_ms, .. } = &mut state.status {
        *elapsed_ms += tick_ms;
        state.needs_redraw = true;
    }
    // Running-task elapsed display must remain live between domain events.
    if state.sidebar_visible
        && state.sidebar_panel == Some(PanelType::Tasks)
        && task_panel_handlers::any_plan_has_running_task(conversation)
    {
        state.needs_redraw = true;
    }
    // u64::MAX is the sticky YOLO warning, replaced only by a mode change/cancel.
    if let StatusState::Flash { remaining_ms, .. } = &mut state.status
        && *remaining_ms != u64::MAX
    {
        if *remaining_ms <= tick_ms {
            state.status = state
                .status_before_flash
                .take()
                .unwrap_or(StatusState::Idle);
            state.needs_redraw = true;
        } else {
            *remaining_ms -= tick_ms;
        }
    }
    if state.which_key.active
        && state
            .which_key
            .is_timed_out(state.theme.timing.which_key_timeout_ms)
    {
        if let Some(focus) = state.which_key.dismiss() {
            state.focus = focus;
        }
        state.needs_redraw = true;
    }
}
