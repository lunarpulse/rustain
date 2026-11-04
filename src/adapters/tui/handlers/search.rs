//! Stage-navigation Search effects. The active tab's ViewState is authoritative;
//! TUI scroll fields are render mirrors, never the source of a saved view.

use crate::adapters::tui::app::{self, InputAction};
use crate::adapters::tui::notifications::{self, NoticeSource};
use crate::adapters::tui::presentation::{self, Surface, reducer};
use crate::adapters::tui::state::{SearchSubstate, TuiState};
use crate::domain::models::tab::TabManager;
use crate::domain::models::{AnchorMode, Conversation};
use crate::domain::services::search::find_matches;

use super::HandlerOutcome;

/// Borrow the real local paste before event conversion can copy its text.
/// Other query owners have no Stage effect; Search uses its owning manager.
pub fn handle_query_paste(
    tabs: &mut TabManager,
    state: &mut TuiState,
    conversation: &Conversation,
    text: &str,
) -> Option<InputAction> {
    let action =
        presentation::route::paste(state, text, crate::adapters::tui::keymap::Host::Local)?;
    if matches!(
        action,
        InputAction::SearchQueryChanged | InputAction::SearchReturnToTyping
    ) {
        handle_search_action(tabs, state, conversation, &action);
    }
    Some(action)
}

/// The runtime's Search effect seam. User Back restores only the owning Stage;
/// reducer close/replacement/discard merely releases the snapshot without restore.
/// Effects run after input routing, so capture ownership before the actual close
/// and settle deferred cards through the same boundary as the input dispatcher.
pub fn handle_search_action(
    tabs: &mut TabManager,
    state: &mut TuiState,
    conversation: &Conversation,
    action: &InputAction,
) -> HandlerOutcome {
    app::with_card_focus_reconcile(state, |state| {
        search_action_inner(tabs, state, conversation, action)
    })
}

fn search_action_inner(
    tabs: &mut TabManager,
    state: &mut TuiState,
    conversation: &Conversation,
    action: &InputAction,
) -> HandlerOutcome {
    if matches!(action, InputAction::OpenSearch) {
        if reducer::initialize_surface(state, Surface::Search) {
            state.search_state.pre_search_view = Some(tabs.active_tab().view_state.clone());
            state.search_state.owning_tab = Some(tabs.active_tab_id());
            reducer::bind_stage(state, Surface::Search, &conversation.id);
            sync_view(tabs, state);
            state.needs_redraw = true;
        }
        return HandlerOutcome::Quiet;
    }
    if !state
        .modal_stack
        .top()
        .is_some_and(|frame| frame.surface == Surface::Search)
    {
        return HandlerOutcome::Quiet;
    }
    // The runtime's active Conversation proxy is current; Tab.conversation is
    // saved on switching and can still carry the pre-/new id. Bind the actual
    // owning tab AND current proxy id, never that stale switch-time model mirror.
    let owns_stage = state.search_state.owning_tab == Some(tabs.active_tab_id())
        && state.modal_stack.top().is_some_and(|frame| {
            frame.conversation_id.as_deref() == Some(conversation.id.as_str())
        });
    if !owns_stage {
        // A replaced/switched Stage can never receive an old snapshot or jump.
        reducer::close(state, Surface::Search);
        return HandlerOutcome::Quiet;
    }
    match action {
        InputAction::CloseSearch => {
            if let Some(view) = state.search_state.pre_search_view.take() {
                tabs.active_tab_mut().view_state = view;
                sync_view(tabs, state);
            }
            reducer::close(state, Surface::Search);
        }
        InputAction::SearchCommit => {
            if state.search_state.substate == SearchSubstate::Navigating {
                // Explicit Keep position: discard the snapshot, not the actual view.
                sync_view(tabs, state);
                reducer::close(state, Surface::Search);
            } else if state.search_state.matches.is_empty() {
                notifications::info(state, NoticeSource::Search, "No matches found");
            } else {
                state.search_state.substate = SearchSubstate::Navigating;
            }
            state.needs_redraw = true;
        }
        InputAction::SearchQueryChanged | InputAction::SearchReturnToTyping => {
            rescan(tabs, state, conversation)
        }
        InputAction::SearchNext => navigate(tabs, state, 1),
        InputAction::SearchPrev => navigate(tabs, state, -1),
        _ => {}
    }
    HandlerOutcome::Quiet
}

fn sync_view(tabs: &TabManager, state: &mut TuiState) {
    let view = &tabs.active_tab().view_state;
    state.scroll_snapshot = view.scroll_offset;
    state.auto_snapshot = matches!(view.mode, AnchorMode::Following);
}

fn jump(tabs: &mut TabManager, state: &mut TuiState, message_index: usize) {
    let offset = crate::adapters::tui::widgets::chat_pane::find_scroll_offset_for_message(
        message_index,
        &state.message_boundaries,
        state.total_content_height,
        state.viewport_height as usize,
    );
    let view = &mut tabs.active_tab_mut().view_state;
    view.scroll_offset = offset;
    view.mode = if offset == 0 {
        AnchorMode::Following
    } else {
        AnchorMode::Reading
    };
    sync_view(tabs, state);
}

/// Preserve calm jumps: move only on 0 -> matches or loss of the focused match.
/// Skip rescans within 30 ms when the query's character length is unchanged.
fn rescan(tabs: &mut TabManager, state: &mut TuiState, conversation: &Conversation) {
    let prev_query_len = state.search_state.last_query_len;
    let cur_query_len = state.search_state.query.as_str().chars().count();
    if let Some(last) = state.search_state.last_search_instant
        && last.elapsed() < std::time::Duration::from_millis(30)
        && prev_query_len == cur_query_len
    {
        return;
    }
    state.search_state.last_query_len = cur_query_len;
    let prev_focused = state
        .search_state
        .matches
        .get(state.search_state.focused_match_index)
        .cloned();
    let prev_was_empty = state.search_state.matches.is_empty();
    let new_matches = find_matches(conversation, state.search_state.query.as_str());
    let new_is_empty = new_matches.is_empty();
    let prev_focused_still_valid = prev_focused
        .as_ref()
        .is_some_and(|focused| new_matches.contains(focused));
    state.search_state.matches = new_matches;
    state.search_state.last_search_instant = Some(std::time::Instant::now());
    if ((prev_was_empty && !new_is_empty) || !prev_focused_still_valid) && !new_is_empty {
        state.search_state.focused_match_index = 0;
        let target = state.search_state.matches[0].message_index;
        jump(tabs, state, target);
    }
    state.needs_redraw = true;
}

/// Cycle matches with the existing wrap notices; a single match never flashes.
fn navigate(tabs: &mut TabManager, state: &mut TuiState, delta: i32) {
    if state.search_state.matches.is_empty() {
        state.needs_redraw = true;
        return;
    }
    let len = state.search_state.matches.len();
    let prev = state.search_state.focused_match_index;
    let new_idx = if delta > 0 {
        (prev + 1) % len
    } else {
        (prev + len - 1) % len
    };
    state.search_state.focused_match_index = new_idx;
    let target = state.search_state.matches[new_idx].message_index;
    jump(tabs, state, target);
    if len > 1 {
        if delta > 0 && prev == len - 1 && new_idx == 0 {
            notifications::info(state, NoticeSource::Search, "Wrapped to top");
        } else if delta < 0 && prev == 0 && new_idx == len - 1 {
            notifications::info(state, NoticeSource::Search, "Wrapped to bottom");
        }
    }
    state.needs_redraw = true;
}
