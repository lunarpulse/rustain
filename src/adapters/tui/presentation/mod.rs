//! Shared Log/Quit presentation ownership (Story 21.4 Task 1-A1).
//!
//! Frames decide whether these surfaces are open; their existing state slots
//! hold editable payloads only. The reducer alone constructs/clears the slots
//! and restores their saved caller focus. Opening never changes legacy focus,
//! so cancelling Quit exposes the exact Log or legacy surface underneath.
//! Help, palette, the decision pane, WhichKey and autocomplete remain legacy.
//! Depth-four replacement and continuations are not part of this cutover.

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
        .iter()
        .any(|frame| frame.surface == surface)
}

/// The frame inventory and payload slots must describe the same open surfaces.
#[must_use]
pub fn consistent(state: &TuiState) -> bool {
    let stack = &state.modal_stack;
    stack.len() <= 4
        && stack.iter().enumerate().all(|(index, frame)| {
            frame.id > 0
                && frame.id <= stack.last_id
                && stack.frames[..index]
                    .iter()
                    .all(|earlier| earlier.surface != frame.surface && earlier.id < frame.id)
        })
        && is_open(state, Surface::NotificationLog) == state.notification_log.is_some()
        && is_open(state, Surface::QuitConfirm) == state.quit_confirm.is_some()
}
