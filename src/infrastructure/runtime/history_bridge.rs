//! The History window's host effects (Story 21.7 A21, AC20): what the event loop does for
//! `Ctrl+X h`, `d` and a finished `Enter`. The window's keys only name the conversation by
//! **id** ([`InputAction::OpenHistoryEntry`], [`InputAction::DeleteHistoryEntry`]); the id is
//! looked up in the live session index here, so a conversation that left the list since the
//! window was painted is refused instead of acting on whatever sits at its old row.

use crate::adapters::tui::history_window;
use crate::adapters::tui::notifications::{self, NoticeSource};
use crate::adapters::tui::presentation::{Surface, reducer};
use crate::adapters::tui::state::TuiState;
use crate::domain::models::FocusState;
use crate::domain::models::visual::DeleteConfirmTarget;
use crate::domain::services::session_index::SessionIndex;

/// What an action on a conversation that is no longer in the index says.
pub const GONE: &str = "That conversation is no longer in the history.";

/// `Ctrl+X h` / the palette: the window on the index as it is now.
pub fn open(state: &mut TuiState, index: &SessionIndex) {
    history_window::open(
        state,
        index,
        crate::domain::models::session_meta::now_unix(),
    );
}

/// `d`: ask to delete the conversation `id` (a decision; nothing is deleted here). The only
/// open conversation is refused; an id the index no longer holds is refused with [`GONE`].
pub fn delete_entry(
    state: &mut TuiState,
    index: &SessionIndex,
    active_conversation_id: &str,
    open_tabs: usize,
    id: &str,
) {
    state.needs_redraw = true;
    let Some(entry) = index.get(id) else {
        notifications::warning(state, NoticeSource::Session, GONE);
        return;
    };
    if id == active_conversation_id && open_tabs == 1 {
        notifications::info(
            state,
            NoticeSource::Session,
            "Cannot delete the only open conversation",
        );
        return;
    }
    let title = if entry.title.is_empty() {
        "Untitled".to_owned()
    } else {
        entry.title.clone()
    };
    super::event_loop::begin_delete_confirmation(
        state,
        DeleteConfirmTarget::Single {
            id: id.to_owned(),
            title,
        },
    );
}

/// `Enter` opened (or switched to) a conversation: the window is done, the Stage owns focus.
pub fn finish_open(state: &mut TuiState) {
    reducer::close(state, Surface::History);
    reducer::settle_stage_base(state, FocusState::Chat);
    state.needs_redraw = true;
}

/// `Enter` named a conversation the index no longer holds (or storage no longer has): say so
/// and leave the window open on the refreshed list.
pub fn gone(state: &mut TuiState) {
    notifications::warning(state, NoticeSource::Session, GONE);
    state.needs_redraw = true;
}

/// The confirmed bulk delete clears the index before it spawns the deletions (7E-08).
pub fn clear_all(state: &mut TuiState, index: &mut SessionIndex) {
    index.clear();
    // The selection named a conversation that is gone with all the others: an explicit bulk
    // clear shows the empty state, not a "no longer available" placeholder (A3 keeps that for
    // an ordinary single removal).
    if let Some(window) = state.history_window.as_mut() {
        window.forget_selection();
    }
    state.needs_redraw = true;
}
