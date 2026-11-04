//! Bookmark effects and Stage-bound BookmarkList ownership (Story 4-4 AC8-AC10).
//!
//! Handlers return `HandlerOutcome::Quiet`. The owning tab's saved view is
//! authoritative; list count and TUI scroll fields are presentation mirrors.
//! Serial saves hold the exclusive Stage borrow and roll back on failure.

#![allow(dead_code)]

use crate::adapters::tui::app;
use crate::adapters::tui::notifications::{self, NoticeSource};
use crate::adapters::tui::presentation::{self, Surface, reducer};
use crate::adapters::tui::state::{BookmarkUndo, TuiState};
use crate::domain::models::tab::TabManager;
use crate::domain::models::{AnchorMode, Conversation, MessageRole};
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
        notifications::info(state, NoticeSource::Bookmark, "No messages to bookmark");
        state.needs_redraw = true;
        return HandlerOutcome::Quiet;
    }
    if state.message_boundaries.is_empty() {
        notifications::info(
            state,
            NoticeSource::Bookmark,
            "Chat pane not ready — try again after first render",
        );
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
    let Some(durable_idx) = conversation.durable_message_index(target_idx) else {
        notifications::info(
            state,
            NoticeSource::Bookmark,
            "Cannot bookmark a temporary display",
        );
        state.needs_redraw = true;
        return HandlerOutcome::Quiet;
    };

    // Guard: bookmark only user/assistant messages.
    let role = conversation.messages[target_idx].role;
    if !matches!(
        role,
        MessageRole::User | MessageRole::Assistant | MessageRole::System
    ) {
        notifications::info(
            state,
            NoticeSource::Bookmark,
            "Cannot bookmark tool message — target a user or assistant message",
        );
        state.needs_redraw = true;
        return HandlerOutcome::Quiet;
    }

    // Clone-mutate-save pattern for rollback safety.
    let mut new_meta = tab_manager.active_tab().session_meta.clone();
    let was_bookmarked = new_meta.bookmarks.binary_search(&durable_idx).is_ok();
    if was_bookmarked {
        new_meta.bookmarks.retain(|&i| i != durable_idx);
    } else {
        match new_meta.bookmarks.binary_search(&durable_idx) {
            Ok(_) => {}
            Err(pos) => new_meta.bookmarks.insert(pos, durable_idx),
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
            notifications::success(state, NoticeSource::Bookmark, msg);
        }
        Err(e) => {
            tab_manager.active_tab_mut().session_meta = old_meta;
            notifications::error(
                state,
                NoticeSource::Bookmark,
                format!("Failed to save bookmark: {}", e),
            );
        }
    }
    state.needs_redraw = true;
    HandlerOutcome::Quiet
}

/// Story 4-4 AC10: open the bookmark list panel.
pub fn handle_apply_open_bookmark_list(
    tab_manager: &TabManager,
    state: &mut TuiState,
    conversation: &Conversation,
) -> HandlerOutcome {
    app::with_card_focus_reconcile(state, |state| {
        let tab = tab_manager.active_tab();
        if tab.session_meta.bookmarks.is_empty() {
            reducer::close(state, Surface::BookmarkList);
            notifications::info(
                state,
                NoticeSource::Bookmark,
                "No bookmarks in this conversation — press 'm' on a message to add one",
            );
        } else {
            if state.modal_stack.all_frames().any(|frame| {
                frame.surface == Surface::BookmarkList
                    && (frame.conversation_id.as_deref() != Some(conversation.id.as_str())
                        || state.bookmark_owning_tab != Some(tab_manager.active_tab_id()))
            }) {
                reducer::close(state, Surface::BookmarkList);
            }
            reducer::open(state, Surface::BookmarkList);
            if reducer::initialize_surface(state, Surface::BookmarkList) {
                state.bookmark_list_selected = 0;
                state.bookmark_owning_tab = Some(tab_manager.active_tab_id());
                reducer::bind_stage(state, Surface::BookmarkList, &conversation.id);
            }
            state.bookmark_list_count = tab.session_meta.bookmarks.len();
        }
        state.needs_redraw = true;
        HandlerOutcome::Quiet
    })
}

/// Close the actual frame before settling any queued local approval.
pub fn close(state: &mut TuiState) {
    app::with_card_focus_reconcile(state, |state| {
        reducer::close(state, Surface::BookmarkList);
    });
}

/// Only the current Stage's foreground frame can act on its bookmarks.
fn owns_stage(tab_manager: &TabManager, state: &mut TuiState, conversation: &Conversation) -> bool {
    let Some(frame) = state
        .modal_stack
        .top()
        .filter(|frame| frame.surface == Surface::BookmarkList)
    else {
        return false;
    };
    if frame.conversation_id.as_deref() == Some(conversation.id.as_str())
        && state.bookmark_owning_tab == Some(tab_manager.active_tab_id())
    {
        true
    } else {
        reducer::close(state, Surface::BookmarkList);
        false
    }
}

/// Story 4-4 AC10: jump using the owning tab's authoritative saved view.
pub fn handle_apply_jump_bookmark(
    tab_manager: &mut TabManager,
    state: &mut TuiState,
    conversation: &Conversation,
) -> HandlerOutcome {
    use crate::adapters::tui::widgets::chat_pane;
    app::with_card_focus_reconcile(state, |state| {
        if !owns_stage(tab_manager, state, conversation) {
            return HandlerOutcome::Quiet;
        }
        let bookmarks = &tab_manager.active_tab().session_meta.bookmarks;
        if let Some(&durable_idx) = bookmarks.get(
            state
                .bookmark_list_selected
                .min(bookmarks.len().saturating_sub(1)),
        ) && let Some(target_msg) = conversation.live_message_index(durable_idx)
        {
            let offset = chat_pane::find_scroll_offset_for_message(
                target_msg,
                &state.message_boundaries,
                state.total_content_height,
                state.viewport_height as usize,
            );
            let view = &mut tab_manager.active_tab_mut().view_state;
            view.scroll_offset = offset;
            view.mode = if offset == 0 {
                AnchorMode::Following
            } else {
                AnchorMode::Reading
            };
            state.scroll_snapshot = offset;
            state.auto_snapshot = offset == 0;
        }
        reducer::close(state, Surface::BookmarkList);
        state.needs_redraw = true;
        HandlerOutcome::Quiet
    })
}

/// Story 4-4 AC10: delete the currently selected bookmark, stashing it in
/// the undo buffer for 5 s.
pub async fn handle_apply_delete_bookmark(
    tab_manager: &mut TabManager,
    state: &mut TuiState,
    conversation: &Conversation,
    fs_storage: &dyn StoragePort,
) -> HandlerOutcome {
    if !app::with_card_focus_reconcile(state, |state| {
        if !owns_stage(tab_manager, state, conversation) {
            return false;
        }
        if tab_manager.active_tab().session_meta.bookmarks.is_empty() {
            reducer::close(state, Surface::BookmarkList);
            return false;
        }
        true
    }) {
        return HandlerOutcome::Quiet;
    }
    let mut new_meta = tab_manager.active_tab().session_meta.clone();
    // The exclusive tab/state borrows remain held across this serial save, so
    // neither the owning Stage nor its frame can change before completion.
    let sel = state
        .bookmark_list_selected
        .min(new_meta.bookmarks.len() - 1);
    let removed_idx = new_meta.bookmarks.remove(sel);

    let old_meta = tab_manager.active_tab().session_meta.clone();
    tab_manager.active_tab_mut().session_meta = new_meta.clone();

    let result = fs_storage
        .save_session_meta(&conversation.id, &new_meta)
        .await;
    app::with_card_focus_reconcile(state, |state| match result {
        Ok(()) => {
            state.bookmark_undo_buffer = Some(BookmarkUndo {
                index: removed_idx,
                when: std::time::Instant::now(),
                owning_tab: tab_manager.active_tab_id(),
                conversation_id: conversation.id.clone(),
            });
            state.bookmark_list_count = new_meta.bookmarks.len();
            if new_meta.bookmarks.is_empty() {
                reducer::close(state, Surface::BookmarkList);
            } else {
                state.bookmark_list_selected = state
                    .bookmark_list_selected
                    .min(new_meta.bookmarks.len() - 1);
            }
            notifications::success(
                state,
                NoticeSource::Bookmark,
                format!("Bookmark removed (msg {}) — press u to undo", removed_idx),
            );
        }
        Err(e) => {
            tab_manager.active_tab_mut().session_meta = old_meta;
            notifications::error(
                state,
                NoticeSource::Bookmark,
                format!("Failed to delete bookmark: {}", e),
            );
        }
    });
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
    if !app::with_card_focus_reconcile(state, |state| {
        !presentation::is_open(state, Surface::BookmarkList)
            || owns_stage(tab_manager, state, conversation)
    }) {
        return HandlerOutcome::Quiet;
    }
    let Some(undo) = state.bookmark_undo_buffer.as_ref() else {
        return HandlerOutcome::Quiet;
    };
    if undo.owning_tab != tab_manager.active_tab_id() || undo.conversation_id != conversation.id {
        close(state);
        return HandlerOutcome::Quiet;
    }
    let (idx, when) = (undo.index, undo.when);
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
    let result = fs_storage
        .save_session_meta(&conversation.id, &new_meta)
        .await;
    app::with_card_focus_reconcile(state, |state| match result {
        Ok(()) => {
            state.bookmark_undo_buffer = None;
            state.bookmark_list_count = new_meta.bookmarks.len();
            notifications::success(
                state,
                NoticeSource::Bookmark,
                format!("Bookmark restored (msg {})", idx),
            );
        }
        Err(e) => {
            tab_manager.active_tab_mut().session_meta = old_meta;
            notifications::error(
                state,
                NoticeSource::Bookmark,
                format!("Failed to restore bookmark: {}", e),
            );
        }
    });
    state.needs_redraw = true;
    HandlerOutcome::Quiet
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::filesystem::FileSystemStorage;
    use crate::domain::models::{ChatMessage, ContentBlockType};

    fn stage() -> (TabManager, TuiState, Conversation) {
        let mut tabs = TabManager::new(Default::default());
        let conversation = Conversation {
            id: "bookmark-stage".into(),
            messages: (0..3)
                .map(|_| ChatMessage {
                    role: MessageRole::User,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        tabs.active_tab_mut().conversation = conversation.clone();
        tabs.active_tab_mut().session_meta.bookmarks = vec![0, 2];
        let mut state = TuiState::new(60, 16);
        state.message_boundaries = vec![0, 10, 20];
        state.total_content_height = 30;
        state.viewport_height = 10;
        (tabs, state, conversation)
    }

    #[test]
    fn story21_4_bookmark_frame_not_count_owns_presence_and_reopen_preserves_selection() {
        let (mut tabs, mut state, conversation) = stage();
        tabs.active_tab_mut().session_meta.bookmarks.clear();
        state.bookmark_list_count = 99;
        handle_apply_open_bookmark_list(&tabs, &mut state, &conversation);
        assert!(!presentation::is_open(&state, Surface::BookmarkList));
        tabs.active_tab_mut().session_meta.bookmarks = vec![0, 2];
        let focus = state.focus.clone();
        handle_apply_open_bookmark_list(&tabs, &mut state, &conversation);
        let frame = state.modal_stack.top().unwrap().clone();
        assert_eq!(
            frame.conversation_id.as_deref(),
            Some(conversation.id.as_str())
        );
        state.bookmark_list_selected = 1;
        state.bookmark_list_count = 0;
        handle_apply_open_bookmark_list(&tabs, &mut state, &conversation);
        assert_eq!(state.modal_stack.top().unwrap().id, frame.id);
        assert_eq!(state.bookmark_list_selected, 1);
        assert_eq!(state.bookmark_list_count, 2);
        assert_eq!(state.focus, focus);
        close(&mut state);
        handle_apply_open_bookmark_list(&tabs, &mut state, &conversation);
        assert_eq!(state.bookmark_list_selected, 0);
        assert_ne!(state.modal_stack.top().unwrap().id, frame.id);
    }

    #[test]
    fn story21_4_bookmark_jump_updates_actual_view_and_closes_for_reading_and_following() {
        for (selection, offset, mode) in
            [(0, 20, AnchorMode::Reading), (1, 0, AnchorMode::Following)]
        {
            let (mut tabs, mut state, conversation) = stage();
            handle_apply_open_bookmark_list(&tabs, &mut state, &conversation);
            state.bookmark_list_selected = selection;
            handle_apply_jump_bookmark(&mut tabs, &mut state, &conversation);
            assert_eq!(tabs.active_tab().view_state.scroll_offset, offset);
            assert_eq!(tabs.active_tab().view_state.mode, mode);
            assert_eq!(state.scroll_snapshot, offset);
            assert_eq!(state.auto_snapshot, offset == 0);
            assert!(!presentation::is_open(&state, Surface::BookmarkList));
        }
    }

    #[test]
    fn story21_4_bookmark_jump_empty_invalid_and_wrong_owner_close_without_changing_view() {
        for route in 0..3 {
            let (mut tabs, mut state, mut conversation) = stage();
            tabs.active_tab_mut().view_state.scroll_offset = 7;
            tabs.active_tab_mut().view_state.mode = AnchorMode::Reading;
            handle_apply_open_bookmark_list(&tabs, &mut state, &conversation);
            match route {
                0 => tabs.active_tab_mut().session_meta.bookmarks.clear(),
                1 => tabs.active_tab_mut().session_meta.bookmarks = vec![99],
                _ => conversation.id = "replacement-stage".into(),
            }
            handle_apply_jump_bookmark(&mut tabs, &mut state, &conversation);
            assert_eq!(tabs.active_tab().view_state.scroll_offset, 7);
            assert_eq!(tabs.active_tab().view_state.mode, AnchorMode::Reading);
            assert!(!presentation::is_open(&state, Surface::BookmarkList));
        }
    }

    #[tokio::test]
    async fn story21_4_bookmark_last_delete_closes_and_same_stage_undo_persists_without_reopening()
    {
        let tmp = tempfile::tempdir().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));
        let (mut tabs, mut state, conversation) = stage();
        tabs.active_tab_mut().session_meta.bookmarks = vec![2];
        handle_apply_open_bookmark_list(&tabs, &mut state, &conversation);
        handle_apply_delete_bookmark(&mut tabs, &mut state, &conversation, &storage).await;
        assert!(tabs.active_tab().session_meta.bookmarks.is_empty());
        assert!(!presentation::is_open(&state, Surface::BookmarkList));
        assert_eq!(state.bookmark_undo_buffer.as_ref().unwrap().index, 2);
        handle_apply_undo_bookmark_delete(&mut tabs, &mut state, &conversation, &storage).await;
        assert_eq!(tabs.active_tab().session_meta.bookmarks, [2]);
        assert!(state.bookmark_undo_buffer.is_none());
        assert!(!presentation::is_open(&state, Surface::BookmarkList));
        assert_eq!(
            storage
                .load_session_meta(&conversation.id)
                .await
                .unwrap()
                .unwrap()
                .bookmarks,
            [2]
        );
    }

    #[tokio::test]
    async fn story21_4_bookmark_delete_failure_rolls_back_and_keeps_frame_and_selection() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("not-a-directory");
        std::fs::write(&path, "occupied").unwrap();
        let storage = FileSystemStorage::new(path);
        let (mut tabs, mut state, conversation) = stage();
        handle_apply_open_bookmark_list(&tabs, &mut state, &conversation);
        let frame = state.modal_stack.top().unwrap().clone();
        state.bookmark_list_selected = 1;
        handle_apply_delete_bookmark(&mut tabs, &mut state, &conversation, &storage).await;
        assert_eq!(tabs.active_tab().session_meta.bookmarks, [0, 2]);
        assert_eq!(state.modal_stack.top(), Some(&frame));
        assert_eq!(state.bookmark_list_selected, 1);
        assert_eq!(state.bookmark_list_count, 2);
        assert!(state.bookmark_undo_buffer.is_none());
    }

    #[tokio::test]
    async fn story21_4_bookmark_delete_and_undo_reject_wrong_actual_stage_without_persisting() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));
        let (mut tabs, mut state, mut conversation) = stage();
        handle_apply_open_bookmark_list(&tabs, &mut state, &conversation);
        let previous_id = conversation.id.clone();
        conversation.id = "replacement-stage".into();
        handle_apply_delete_bookmark(&mut tabs, &mut state, &conversation, &storage).await;
        state.bookmark_undo_buffer = Some(BookmarkUndo {
            index: 1,
            when: std::time::Instant::now(),
            owning_tab: tabs.active_tab_id(),
            conversation_id: previous_id,
        });
        handle_apply_undo_bookmark_delete(&mut tabs, &mut state, &conversation, &storage).await;
        assert_eq!(tabs.active_tab().session_meta.bookmarks, [0, 2]);
        assert!(!presentation::is_open(&state, Surface::BookmarkList));
        assert!(
            storage
                .load_session_meta(&conversation.id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn story21_4_bookmark_undo_expires_after_five_seconds() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));
        let (mut tabs, mut state, conversation) = stage();
        state.bookmark_undo_buffer = Some(BookmarkUndo {
            index: 1,
            when: std::time::Instant::now() - std::time::Duration::from_secs(6),
            owning_tab: tabs.active_tab_id(),
            conversation_id: conversation.id.clone(),
        });
        handle_apply_undo_bookmark_delete(&mut tabs, &mut state, &conversation, &storage).await;
        assert_eq!(tabs.active_tab().session_meta.bookmarks, [0, 2]);
        assert!(state.bookmark_undo_buffer.is_none());
        assert!(
            storage
                .load_session_meta(&conversation.id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn ephemeral_bookmark_is_rejected_and_later_durable_row_is_remapped() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));
        let mut tabs = TabManager::new(Default::default());
        let mut state = TuiState::new(80, 24);
        let conversation = Conversation {
            id: "bookmark-policy".into(),
            messages: vec![
                ChatMessage {
                    id: "first".into(),
                    role: MessageRole::User,
                    ..Default::default()
                },
                ChatMessage {
                    id: "temporary".into(),
                    content_blocks: vec![ContentBlockType::EphemeralDisplay],
                    ..Default::default()
                },
                ChatMessage {
                    id: "last".into(),
                    role: MessageRole::User,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        tabs.active_tab_mut().conversation = conversation.clone();
        state.message_boundaries = vec![0, 10, 20];
        state.total_content_height = 30;
        state.viewport_height = 10;
        state.auto_snapshot = true;
        handle_apply_bookmark_toggle(&mut tabs, &mut state, &conversation, &storage).await;
        assert_eq!(tabs.active_tab().session_meta.bookmarks, [1]);
        assert_eq!(
            storage
                .load_session_meta(&conversation.id)
                .await
                .unwrap()
                .unwrap()
                .bookmarks,
            [1]
        );
        state.auto_snapshot = false;
        state.scroll_snapshot = 10;
        handle_apply_bookmark_toggle(&mut tabs, &mut state, &conversation, &storage).await;
        assert_eq!(tabs.active_tab().session_meta.bookmarks, [1]);
        assert_eq!(
            storage
                .load_session_meta(&conversation.id)
                .await
                .unwrap()
                .unwrap()
                .bookmarks,
            [1]
        );
        handle_apply_open_bookmark_list(&tabs, &mut state, &conversation);
        handle_apply_jump_bookmark(&mut tabs, &mut state, &conversation);
        assert_eq!(state.scroll_snapshot, 0);
    }

    #[tokio::test]
    async fn story21_4_bookmark_same_conversation_in_another_tab_cannot_take_frame_or_undo() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));
        let (mut tabs, mut state, conversation) = stage();
        let owner = tabs.active_tab_id();
        handle_apply_open_bookmark_list(&tabs, &mut state, &conversation);
        handle_apply_delete_bookmark(&mut tabs, &mut state, &conversation, &storage).await;
        let meta = tabs.active_tab().session_meta.clone();
        tabs.create_tab_with_conversation(conversation.clone());
        assert_ne!(
            tabs.active_tab_id(),
            owner,
            "same proxy id, distinct actual tab"
        );
        tabs.active_tab_mut().session_meta = meta;
        tabs.active_tab_mut().view_state.scroll_offset = 7;
        tabs.active_tab_mut().view_state.mode = AnchorMode::Reading;
        handle_apply_jump_bookmark(&mut tabs, &mut state, &conversation);
        handle_apply_undo_bookmark_delete(&mut tabs, &mut state, &conversation, &storage).await;
        assert_eq!(tabs.active_tab().view_state.scroll_offset, 7);
        assert_eq!(tabs.active_tab().view_state.mode, AnchorMode::Reading);
        assert_eq!(tabs.active_tab().session_meta.bookmarks, [2]);
        assert!(!presentation::is_open(&state, Surface::BookmarkList));
        assert_eq!(
            storage
                .load_session_meta(&conversation.id)
                .await
                .unwrap()
                .unwrap()
                .bookmarks,
            [2],
        );
    }
}
