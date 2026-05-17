//! Bookmark handlers — Epic 4 (Story 4-4 AC8-AC10).
//!
//! All 5 handlers return `HandlerOutcome::Quiet` per Task 0 bucketing. Async
//! variants do `fs_storage.save_session_meta` I/O via `&dyn StoragePort`
//! (domain port — `FileSystemStorage` impls it). No spawn, no event emission.

#![allow(dead_code)]

use crate::adapters::tui::state::TuiState;
use crate::domain::models::tab::TabManager;
use crate::domain::models::{Conversation, FocusState, MessageRole, StatusState};
use crate::domain::ports::StoragePort;

use super::HandlerOutcome;

/// Story 4-4 AC8: toggle a bookmark on the currently focused message.
///
/// Optimistic UI: update in-memory first, persist to disk; rollback on failure
/// (AC8 optimistic-UI contract). Rejects tool-call / tool-result messages with
/// a flash — bookmarks only apply to user/assistant messages per Dev Notes
/// § Bookmarkable Message Types.
pub async fn handle_apply_bookmark_toggle(
    tab_manager: &mut TabManager,
    state: &mut TuiState,
    conversation: &Conversation,
    fs_storage: &dyn StoragePort,
) -> HandlerOutcome {
    use crate::adapters::tui::widgets::chat_pane::find_message_index_from_scroll_offset;

    if conversation.messages.is_empty() {
        state.status = StatusState::Flash {
            message: "No messages to bookmark".to_string(),
            remaining_ms: 2000,
        };
        state.needs_redraw = true;
        return HandlerOutcome::Quiet;
    }
    if state.message_boundaries.is_empty() {
        state.status = StatusState::Flash {
            message: "Chat pane not ready — try again after first render".to_string(),
            remaining_ms: 2000,
        };
        state.needs_redraw = true;
        return HandlerOutcome::Quiet;
    }

    let target_idx = find_message_index_from_scroll_offset(
        state.auto_snapshot,
        state.scroll_snapshot,
        &state.message_boundaries,
        state.total_content_height,
        state.viewport_height as usize,
        conversation.messages.len(),
    );

    // Guard: bookmark only user/assistant messages.
    let role = conversation.messages[target_idx].role;
    if !matches!(
        role,
        MessageRole::User | MessageRole::Assistant | MessageRole::System
    ) {
        state.status = StatusState::Flash {
            message: "Cannot bookmark tool message — target a user or assistant message"
                .to_string(),
            remaining_ms: 2000,
        };
        state.needs_redraw = true;
        return HandlerOutcome::Quiet;
    }

    // Clone-mutate-save pattern for rollback safety.
    let mut new_meta = tab_manager.active_tab().session_meta.clone();
    let was_bookmarked = new_meta.bookmarks.binary_search(&target_idx).is_ok();
    if was_bookmarked {
        new_meta.bookmarks.retain(|&i| i != target_idx);
    } else {
        match new_meta.bookmarks.binary_search(&target_idx) {
            Ok(_) => {}
            Err(pos) => new_meta.bookmarks.insert(pos, target_idx),
        }
    }

    // Optimistic UI: update in-memory first.
    let old_meta = tab_manager.active_tab().session_meta.clone();
    tab_manager.active_tab_mut().session_meta = new_meta.clone();

    // Persist to disk. Rollback on failure.
    match fs_storage
        .save_session_meta(&conversation.id, &new_meta)
        .await
    {
        Ok(()) => {
            let msg = if was_bookmarked {
                format!("Bookmark removed (msg {})", target_idx)
            } else {
                format!("Bookmark added (msg {})", target_idx)
            };
            state.status = StatusState::Flash {
                message: msg,
                remaining_ms: 2000,
            };
        }
        Err(e) => {
            tab_manager.active_tab_mut().session_meta = old_meta;
            state.status = StatusState::Flash {
                message: format!("Failed to save bookmark: {}", e),
                remaining_ms: 3000,
            };
        }
    }
    state.needs_redraw = true;
    HandlerOutcome::Quiet
}

/// Story 4-4 AC10: open the bookmark list panel.
pub fn handle_apply_open_bookmark_list(
    tab_manager: &TabManager,
    state: &mut TuiState,
) -> HandlerOutcome {
    let bookmarks = &tab_manager.active_tab().session_meta.bookmarks;
    if bookmarks.is_empty() {
        state.status = StatusState::Flash {
            message: "No bookmarks in this conversation — press 'm' on a message to add one"
                .to_string(),
            remaining_ms: 2000,
        };
        state.needs_redraw = true;
        return HandlerOutcome::Quiet;
    }
    state.focus = FocusState::Overlay(crate::domain::models::visual::OverlayType::BookmarkList);
    state.bookmark_list_selected = 0;
    state.bookmark_list_count = bookmarks.len();
    state.needs_redraw = true;
    HandlerOutcome::Quiet
}

/// Story 4-4 AC10: jump to the currently selected bookmark.
pub fn handle_apply_jump_bookmark(
    tab_manager: &TabManager,
    state: &mut TuiState,
) -> HandlerOutcome {
    use crate::adapters::tui::widgets::chat_pane;
    let bookmarks = &tab_manager.active_tab().session_meta.bookmarks;
    if bookmarks.is_empty() {
        state.focus = FocusState::Chat;
        state.needs_redraw = true;
        return HandlerOutcome::Quiet;
    }
    let sel = state.bookmark_list_selected.min(bookmarks.len() - 1);
    let target_msg = bookmarks[sel];
    state.scroll_snapshot = chat_pane::find_scroll_offset_for_message(
        target_msg,
        &state.message_boundaries,
        state.total_content_height,
        state.viewport_height as usize,
    );
    state.auto_snapshot = state.scroll_snapshot == 0;
    state.focus = FocusState::Chat;
    state.bookmark_list_selected = 0;
    state.needs_redraw = true;
    HandlerOutcome::Quiet
}

/// Story 4-4 AC10: delete the currently selected bookmark, stashing it in
/// the undo buffer for 5 s.
pub async fn handle_apply_delete_bookmark(
    tab_manager: &mut TabManager,
    state: &mut TuiState,
    conversation: &Conversation,
    fs_storage: &dyn StoragePort,
) -> HandlerOutcome {
    let mut new_meta = tab_manager.active_tab().session_meta.clone();
    if new_meta.bookmarks.is_empty() {
        return HandlerOutcome::Quiet;
    }
    let sel = state
        .bookmark_list_selected
        .min(new_meta.bookmarks.len() - 1);
    let removed_idx = new_meta.bookmarks.remove(sel);

    let old_meta = tab_manager.active_tab().session_meta.clone();
    tab_manager.active_tab_mut().session_meta = new_meta.clone();

    match fs_storage
        .save_session_meta(&conversation.id, &new_meta)
        .await
    {
        Ok(()) => {
            state.bookmark_undo_buffer = Some((removed_idx, std::time::Instant::now()));
            state.bookmark_list_count = new_meta.bookmarks.len();
            if new_meta.bookmarks.is_empty() {
                state.bookmark_list_selected = 0;
                state.focus = FocusState::Chat;
            } else {
                state.bookmark_list_selected = state
                    .bookmark_list_selected
                    .min(new_meta.bookmarks.len() - 1);
            }
            state.status = StatusState::Flash {
                message: format!("Bookmark removed (msg {}) — press u to undo", removed_idx),
                remaining_ms: 2000,
            };
        }
        Err(e) => {
            tab_manager.active_tab_mut().session_meta = old_meta;
            state.status = StatusState::Flash {
                message: format!("Failed to delete bookmark: {}", e),
                remaining_ms: 3000,
            };
        }
    }
    state.needs_redraw = true;
    HandlerOutcome::Quiet
}

/// Story 4-4 AC10: undo the most recent bookmark delete (if within 5 s).
pub async fn handle_apply_undo_bookmark_delete(
    tab_manager: &mut TabManager,
    state: &mut TuiState,
    conversation: &Conversation,
    fs_storage: &dyn StoragePort,
) -> HandlerOutcome {
    let Some((idx, when)) = state.bookmark_undo_buffer else {
        return HandlerOutcome::Quiet;
    };
    if when.elapsed() > std::time::Duration::from_secs(5) {
        state.bookmark_undo_buffer = None;
        return HandlerOutcome::Quiet;
    }

    let mut new_meta = tab_manager.active_tab().session_meta.clone();
    // Re-insert sorted.
    match new_meta.bookmarks.binary_search(&idx) {
        Ok(_) => {
            state.bookmark_undo_buffer = None;
            return HandlerOutcome::Quiet;
        }
        Err(pos) => new_meta.bookmarks.insert(pos, idx),
    }

    let old_meta = tab_manager.active_tab().session_meta.clone();
    tab_manager.active_tab_mut().session_meta = new_meta.clone();
    match fs_storage
        .save_session_meta(&conversation.id, &new_meta)
        .await
    {
        Ok(()) => {
            state.bookmark_undo_buffer = None;
            state.status = StatusState::Flash {
                message: format!("Bookmark restored (msg {})", idx),
                remaining_ms: 1500,
            };
        }
        Err(e) => {
            tab_manager.active_tab_mut().session_meta = old_meta;
            state.status = StatusState::Flash {
                message: format!("Failed to restore bookmark: {}", e),
                remaining_ms: 3000,
            };
        }
    }
    state.needs_redraw = true;
    HandlerOutcome::Quiet
}
