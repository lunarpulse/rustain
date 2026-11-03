//! Shared frame, routed popup and pending-leader ownership (Story 21.4 A2b).
//!
//! Editable payloads never answer whether a frame is open. The local pane
//! carries its queue identity in the same stack used by both hosts.

mod compose;
pub mod geometry;
pub mod reducer;
pub mod route;
mod stack;

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
                    .all(|earlier| earlier.surface != frame.surface)
                && match &frame.decision {
                    Some(key) => {
                        frame.surface == Surface::DecisionPane && state.decisions.contains_key(key)
                    }
                    None => frame.surface != Surface::DecisionPane,
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
        || state.model_selector.active
        || state.login.active
        || state.profile_switcher.active
        || state.usage_panel.active
        || state.reverse_search.active
        || state.search_state.active
        || !matches!(
            state.focus,
            FocusState::Input | FocusState::Chat | FocusState::Sidebar { .. }
        )
    {
        return false;
    }
    state.modal_stack.top().is_none_or(|frame| {
        !matches!(frame.surface, Surface::DecisionPane | Surface::QuitConfirm)
            && !typing_context(frame.surface.context())
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
