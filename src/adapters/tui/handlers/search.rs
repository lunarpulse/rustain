//! Search handlers — Epic 4 (Story 4-4).
//!
//! - `handle_apply_search_rescan` (formerly `apply_search_rescan`) — `Quiet`
//! - `handle_apply_search_navigate` (formerly `apply_search_navigate`) — `Quiet`
//!
//! **NOT extracted in Phase 4:** `apply_cross_search_scan` was DELETED in Task 0
//! cleanup per DGI-B (verified DEAD CODE — Story 4-4 replaced it with the
//! async-spawned path delivering via `AppEvent::CrossSearchResultsReady`).
//!
//! **DEFERRED to follow-up DF:** `apply_open_cross_search_result` (spawn-bearing,
//! `SpawnRequest::ScheduledEvent` for peek-highlight expiry) stays in
//! `event_loop.rs` for Phase 4. Rationale: it calls `save_active_tab` +
//! `load_active_tab` (heavily-used cross-cutting tab-persistence helpers — 30+
//! call sites in event_loop.rs). Extracting would require moving those helpers
//! to a domain port (proper home: future `domain/ports/tab_persistence.rs`),
//! which is out-of-scope for Story 8.0a. Acceptance Criteria amended: 22→21
//! in-scope (20→19 `handle_*` + 2 helpers). `SpawnRequest::ScheduledEvent`
//! variant stays specified + type-checked but unexercised — per Winston
//! Decision Gate "speculative exercise would be theatre."

#![allow(dead_code)]

use crate::adapters::tui::state::TuiState;
use crate::domain::models::{Conversation, StatusState};
use crate::domain::services::search::find_matches;

use super::HandlerOutcome;

/// Story 4-4 AC3: refresh `state.search_state.matches` against the current
/// `state.search_state.query`. Calm-jump rule:
///   1. Jump to match 0 only if match count transitioned 0 → ≥1
///   2. Jump to match 0 only if the previously focused match no longer exists
///   3. Otherwise preserve the viewport (no yo-yo)
///
/// Debounce: skip if last scan <30ms ago AND query length unchanged.
pub fn handle_apply_search_rescan(
    conversation: &Conversation,
    state: &mut TuiState,
) -> HandlerOutcome {
    use crate::adapters::tui::widgets::chat_pane;

    let prev_query_len = state.search_state.last_query_len;
    let cur_query_len = state.search_state.query.chars().count();
    if let Some(last) = state.search_state.last_search_instant {
        if last.elapsed() < std::time::Duration::from_millis(30) && prev_query_len == cur_query_len
        {
            return HandlerOutcome::Quiet;
        }
    }
    state.search_state.last_query_len = cur_query_len;

    let prev_focused = state
        .search_state
        .matches
        .get(state.search_state.focused_match_index)
        .cloned();
    let prev_was_empty = state.search_state.matches.is_empty();

    let new_matches = find_matches(conversation, &state.search_state.query);
    let new_is_empty = new_matches.is_empty();
    let prev_focused_still_valid = match &prev_focused {
        Some(f) => new_matches.contains(f),
        None => false,
    };
    state.search_state.matches = new_matches;
    state.search_state.last_search_instant = Some(std::time::Instant::now());

    let should_jump = (prev_was_empty && !new_is_empty) || !prev_focused_still_valid;
    if should_jump && !state.search_state.matches.is_empty() {
        state.search_state.focused_match_index = 0;
        let target_msg = state.search_state.matches[0].message_index;
        state.scroll_snapshot = chat_pane::find_scroll_offset_for_message(
            target_msg,
            &state.message_boundaries,
            state.total_content_height,
            state.viewport_height as usize,
        );
        state.auto_snapshot = state.scroll_snapshot == 0;
    }
    state.needs_redraw = true;
    HandlerOutcome::Quiet
}

/// Advance or reverse the focused search match, wrapping at boundaries.
/// Emits a "Wrapped to top" / "Wrapped to bottom" flash (800 ms) when the
/// index wraps past 0 or `matches.len() - 1` (Story 4-4 AC3 amendment Fix 3).
///
/// `delta`: +1 for `n` (next), -1 for `N` (previous).
pub fn handle_apply_search_navigate(state: &mut TuiState, delta: i32) -> HandlerOutcome {
    use crate::adapters::tui::widgets::chat_pane;
    if state.search_state.matches.is_empty() {
        state.needs_redraw = true;
        return HandlerOutcome::Quiet;
    }
    let len = state.search_state.matches.len();
    let prev = state.search_state.focused_match_index;
    let new_idx = if delta > 0 {
        (prev + 1) % len
    } else {
        (prev + len - 1) % len
    };
    state.search_state.focused_match_index = new_idx;
    let target_msg = state.search_state.matches[new_idx].message_index;
    state.scroll_snapshot = chat_pane::find_scroll_offset_for_message(
        target_msg,
        &state.message_boundaries,
        state.total_content_height,
        state.viewport_height as usize,
    );
    state.auto_snapshot = state.scroll_snapshot == 0;

    // Wrap-around flash (only fires when len > 1 — a single match never wraps visibly).
    if len > 1 {
        let wrapped_forward = delta > 0 && prev == len - 1 && new_idx == 0;
        let wrapped_backward = delta < 0 && prev == 0 && new_idx == len - 1;
        if wrapped_forward {
            state.status_before_flash = Some(state.status.clone());
            state.status = StatusState::Flash {
                message: "Wrapped to top".to_string(),
                remaining_ms: 800,
            };
        } else if wrapped_backward {
            state.status_before_flash = Some(state.status.clone());
            state.status = StatusState::Flash {
                message: "Wrapped to bottom".to_string(),
                remaining_ms: 800,
            };
        }
    }
    state.needs_redraw = true;
    HandlerOutcome::Quiet
}
