//! Shared frame, routed popup and pending-leader ownership (Story 21.4 Ba).
//!
//! Editable payloads never answer whether a frame is open. The local pane
//! carries its queue identity in the same stack used by both hosts.

mod compose;
pub mod geometry;
pub mod inspector_search;
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
        Surface::WaveOverlay => super::handlers::wave_overlay::context(state),
        _ => surface.context(),
    }
}

/// Only the current Log/Help frame may lend its child field to the common editor.
pub(crate) fn inspector_search_mut(
    state: &mut TuiState,
) -> Option<&mut inspector_search::InspectorSearch> {
    match state.modal_stack.top()?.surface {
        Surface::Help => Some(&mut state.help_overlay.search),
        Surface::NotificationLog => state.notification_log.as_mut().map(|log| &mut log.search),
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
                        frame.surface == Surface::DecisionPane && state.decisions.contains_key(key)
                    }
                    None => frame.surface != Surface::DecisionPane,
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
        && state.tool_inspectors.iter().all(|(id, tool)| {
            stack
                .all_frames()
                .filter(
                    |frame| matches!(frame.surface, Surface::ToolInspector(key) if key.0 == *id),
                )
                .count()
                == 1
                && state
                    .tool_inspectors
                    .iter()
                    .all(|(other_id, other)| id == other_id || tool.target != other.target)
        })
}

/// RU-6 eligibility is evaluated BEFORE adding the incoming queue item.
/// Both inherited text contexts protect a frame independently of recent typing.
pub fn arrival_may_preempt(state: &TuiState, now: std::time::Instant) -> bool {
    use super::{app, decision_pane};
    use crate::domain::models::FocusState;
    if !state.decisions.is_empty()
        || state
            .decisions
            .last_defer_at()
            .is_some_and(|at| now.saturating_duration_since(at) < decision_pane::DEFER_COOLDOWN)
        || (state.focus == FocusState::Input && !state.input_buffer.is_empty())
        || state
            .last_printable_at
            .is_some_and(|at| now.saturating_duration_since(at) < decision_pane::TYPING_GRACE)
        || state.pending_leader.is_some()
        || state.ask_user_question.is_some()
        || app::proposal_higher_confirmation_active(state)
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
            Surface::DecisionPane | Surface::QuitConfirm | Surface::Login
        ) && !typing_context(frame_context(state, frame.surface))
    })
}

fn typing_context(context: super::keymap::Context) -> bool {
    use super::keymap::Context;
    matches!(context, Context::TextEntry | Context::FieldEdit)
        || context
            .parents()
            .iter()
            .any(|parent| typing_context(*parent))
}
