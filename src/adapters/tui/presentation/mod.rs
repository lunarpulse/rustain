//! Shared frame, routed popup and pending-leader ownership (Story 21.4 Ba).
//!
//! Editable payloads never answer whether a frame is open. The local pane
//! carries its queue identity in the same stack used by both hosts.

mod compose;
pub mod geometry;
pub mod inspector_search;
pub mod markdown_body;
pub mod occlusion;
pub mod reducer;
pub mod route;
mod stack;
pub mod stage_search;
pub mod text_field;
pub mod viewer;

pub use compose::{Shell, compose, paint_too_small};
pub use route::{RouteEffect, route, too_small};
pub use stack::{Frame, ModalStack, Opened, Surface};

use super::state::TuiState;

#[must_use]
pub fn is_open(state: &TuiState, surface: Surface) -> bool {
    state
        .modal_stack
        .all_frames()
        .any(|frame| frame.surface == surface)
}

/// Search changes key context without changing frame identity or base focus.
pub fn frame_context(state: &TuiState, surface: Surface) -> super::keymap::Context {
    use super::{keymap::Context, state::SearchSubstate};
    match surface {
        Surface::Search => match state.search_state.substate {
            SearchSubstate::Typing => Context::SearchTyping,
            SearchSubstate::Navigating => Context::SearchNavigating,
        },
        Surface::Help if state.help_overlay.search.editing => Context::InspectorSearch,
        Surface::NotificationLog
            if state
                .notification_log
                .as_ref()
                .is_some_and(|log| log.search.editing) =>
        {
            Context::InspectorSearch
        }
        Surface::ToolInspector(key)
            if state
                .tool_inspectors
                .get(&key.0)
                .is_some_and(|tool| !tool.missing && tool.search.editor.editing) =>
        {
            Context::InspectorSearch
        }
        Surface::BlockViewer(key)
            if state
                .block_viewers
                .get(&key.0)
                .is_some_and(|block| !block.missing && block.search.editor.editing) =>
        {
            Context::InspectorSearch
        }
        Surface::WaveOverlay => super::handlers::wave_overlay::context(state),
        Surface::ThreadInspector => super::thread_inspector::frame_context(state),
        Surface::MissionControl if super::mission_control::search_editing(state) => {
            super::keymap::Context::InspectorSearch
        }
        Surface::ArtifactWindow if super::artifact_window::search_editing(state) => {
            super::keymap::Context::InspectorSearch
        }
        Surface::AgentWindow if super::agent_window::search_editing(state) => {
            super::keymap::Context::InspectorSearch
        }
        Surface::Decision => super::decision_queue::frame_context(state),
        _ => surface.context(),
    }
}

/// Only the current inspector frame may lend its child field to the common editor.
pub(crate) fn inspector_search_mut(
    state: &mut TuiState,
) -> Option<&mut inspector_search::InspectorSearch> {
    match state.modal_stack.top()?.surface {
        Surface::Help => Some(&mut state.help_overlay.search),
        Surface::NotificationLog => state.notification_log.as_mut().map(|log| &mut log.search),
        Surface::ToolInspector(key) => state
            .tool_inspectors
            .get_mut(&key.0)
            .filter(|tool| !tool.missing)
            .map(|tool| &mut tool.search.editor),
        Surface::BlockViewer(key) => state
            .block_viewers
            .get_mut(&key.0)
            .filter(|block| !block.missing)
            .map(|block| &mut block.search.editor),
        Surface::ThreadInspector => state
            .thread_inspector
            .as_mut()
            .map(|thread| &mut thread.search),
        Surface::MissionControl => state.mission_control.search_mut(),
        Surface::ArtifactWindow => state
            .artifact_window
            .as_mut()
            .map(|window| &mut window.search),
        Surface::AgentWindow => state
            .agent_window
            .as_mut()
            .map(|window| &mut window.text.search),
        _ => None,
    }
}

/// The frame inventory and payload slots must describe the same open surfaces.
#[must_use]
pub fn consistent(state: &TuiState) -> bool {
    let stack = &state.modal_stack;
    stack.len() <= 4
        && stack.all_frames().enumerate().all(|(index, frame)| {
            frame.id > 0
                && frame.id <= stack.last_id
                && stack
                    .all_frames()
                    .take(index)
                    .all(|earlier| earlier.id != frame.id && earlier.surface != frame.surface)
                && match &frame.decision {
                    Some(key) => {
                        frame.surface == Surface::Decision && state.decisions.contains_key(key)
                    }
                    None => frame.surface != Surface::Decision,
                }
                && match frame.surface {
                    Surface::ToolInspector(key) => {
                        key.0 == frame.id
                            && frame.tool_title.is_some()
                            && state.tool_inspectors.get(&key.0).is_some_and(|tool| {
                                frame.conversation_id.as_deref()
                                    == Some(tool.target.conversation_id.as_str())
                            })
                    }
                    Surface::BlockViewer(key) => {
                        key.0 == frame.id
                            && frame.tool_title.is_some()
                            && state.block_viewers.get(&key.0).is_some_and(|block| {
                                frame.conversation_id.as_deref()
                                    == Some(block.target.conversation_id.as_str())
                            })
                    }
                    Surface::ThreadInspector
                    | Surface::MissionControl
                    | Surface::ArtifactWindow
                    | Surface::AgentWindow => {
                        frame.tool_title.is_some()
                    }
                    _ => frame.tool_title.is_none(),
                }
        })
        && stack.continuations.len() <= 5
        && stack
            .continuations
            .iter()
            .enumerate()
            .all(|(index, continuation)| {
                continuation.owner.keeps_continuation()
                    && continuation.owner != continuation.frame.surface
                    && stack.all_frames().any(|f| f.surface == continuation.owner)
                    && stack.continuations[..index]
                        .iter()
                        .all(|c| c.owner != continuation.owner)
            })
        && is_open(state, Surface::NotificationLog) == state.notification_log.is_some()
        && is_open(state, Surface::QuitConfirm) == state.quit_confirm.is_some()
        && is_open(state, Surface::ThreadInspector) == state.thread_inspector.is_some()
        && is_open(state, Surface::ArtifactWindow) == state.artifact_window.is_some()
        && is_open(state, Surface::AgentWindow) == state.agent_window.is_some()
        && is_open(state, Surface::Adapters) == state.adapters_window.is_some()
        // In-place stepping may coincide targets; each frame still owns one payload.
        && state.tool_inspectors.keys().all(|id| {
            stack
                .all_frames()
                .filter(
                    |frame| matches!(frame.surface, Surface::ToolInspector(key) if key.0 == *id),
                )
                .count()
                == 1
        })
        && state.block_viewers.keys().all(|id| {
            stack
                .all_frames()
                .filter(
                    |frame| matches!(frame.surface, Surface::BlockViewer(key) if key.0 == *id),
                )
                .count()
                == 1
        })
}

/// RU-6 eligibility is evaluated BEFORE adding the incoming queue item.
/// Both inherited text contexts protect a frame independently of recent typing.
pub fn arrival_may_preempt(state: &TuiState, now: std::time::Instant) -> bool {
    use super::decision_queue;
    use crate::domain::models::FocusState;
    if !state.decisions.is_empty()
        || state
            .decisions
            .last_defer_at()
            .is_some_and(|at| now.saturating_duration_since(at) < decision_queue::DEFER_COOLDOWN)
        || (state.focus == FocusState::Input && !state.input_buffer.is_empty())
        || state
            .last_printable_at
            .is_some_and(|at| now.saturating_duration_since(at) < decision_queue::TYPING_GRACE)
        || state.pending_leader.is_some()
        || state.reverse_search.is_open()
        || !matches!(
            state.focus,
            FocusState::Input | FocusState::Chat | FocusState::Sidebar { .. }
        )
    {
        return false;
    }
    state.modal_stack.top().is_none_or(|frame| {
        !matches!(
            frame.surface,
            Surface::Decision | Surface::DecisionQueue | Surface::QuitConfirm | Surface::Login
        ) && !typing_context(frame_context(state, frame.surface))
    })
}

/// True when the top frame is a text field: a Requested decision queues behind it
/// instead of taking the keyboard (A5).
pub(crate) fn typing_top(state: &TuiState) -> bool {
    state
        .modal_stack
        .top()
        .is_some_and(|frame| typing_context(frame_context(state, frame.surface)))
}

fn typing_context(context: super::keymap::Context) -> bool {
    use super::keymap::Context;
    matches!(context, Context::TextEntry | Context::FieldEdit)
        || context
            .parents()
            .iter()
            .any(|parent| typing_context(*parent))
}
