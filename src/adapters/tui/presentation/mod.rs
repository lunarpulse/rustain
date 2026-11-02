//! Shared frame and routed-leader ownership (Story 21.4 A2a).
//!
//! Editable payloads do not answer whether a frame is open. Help, palette,
//! Log and Quit use this reducer/boundary/compositor in both hosts.
//! Autocomplete and the local pane remain wholly legacy until A2b.

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
