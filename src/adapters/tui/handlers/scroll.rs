//! Scroll-intent handler — Epic 4 (Story 16.8 AC15).
//!
//! `handle_apply_scroll_intent` (formerly `apply_scroll_intent`) returns
//! `HandlerOutcome::Notify(SystemNotice)` when the anchor-confirmation toast
//! trips (first scroll while Pinned), else `HandlerOutcome::Quiet`. Dispatch
//! arm routes the Notify event via `app_state.event_bus.emit_domain(...)`.
//!
//! `dispatch_view_scroll` is a `pub(crate)` helper called both internally and
//! from 2 event_loop.rs dispatch arms (for Top/Bottom scroll deltas that
//! bypass the anchor gate). DGI-A pattern (helper, not handler).

#![allow(dead_code)]

use crate::adapters::tui::state::TuiState;
use crate::domain::events::AppEvent;
use crate::domain::models::tab::TabManager;
use crate::domain::models::view_state::{LayoutMetrics, ScrollDelta};
use crate::domain::models::{AnchorMode, Conversation, NoticeLevel, ViewEvent};

use super::HandlerOutcome;

/// S16.8 AC15: Two-stage anchor-confirmation gate for scroll-intent events.
///
/// When the user is Pinned and emits a scroll-intent (j/k/wheel/page-scroll):
/// first tick shows a toast (returns `Notify(SystemNotice)`) and no-ops the
/// scroll; second tick within 2000ms drops the anchor via
/// `ViewEvent::DropAnchorAndScroll` and applies the scroll (returns `Quiet`).
///
/// Jump-intents (G, gg) and non-scroll inputs (BlockJump) call
/// `dispatch_view_scroll` directly per event_loop.rs dispatch arms.
pub fn handle_apply_scroll_intent(
    tab_manager: &mut TabManager,
    state: &mut TuiState,
    conversation: &Conversation,
    delta: ScrollDelta,
) -> HandlerOutcome {
    let is_pinned = matches!(
        tab_manager.active_tab().view_state.mode,
        AnchorMode::Pinned(_)
    );

    if !is_pinned {
        dispatch_view_scroll(tab_manager, state, conversation, delta);
        return HandlerOutcome::Quiet;
    }

    // Pinned: two-stage confirmation (AC15 clauses 1-3).
    let clock = tab_manager.active_tab().clock.clone();
    let now = clock.now();
    let needs_toast = match state.pending_anchor_drop {
        None => true,
        Some(t) => now.duration_since(t) > std::time::Duration::from_millis(2000),
    };

    if needs_toast {
        state.pending_anchor_drop = Some(now);
        state.needs_redraw = true;
        HandlerOutcome::Notify(AppEvent::SystemNotice {
            conversation_id: Some(conversation.id.clone()),
            level: NoticeLevel::Info,
            message: "Anchored to this turn. Scroll again to release, or press ]] .".to_string(),
        })
    } else {
        // Second tick within 2000ms: drop anchor via explicit ViewEvent.
        state.pending_anchor_drop = None;
        // Pre-flip mode to Reading so apply_scroll sees Reading not Pinned.
        tab_manager.active_tab_mut().view_state.mode = AnchorMode::Reading;
        dispatch_view_scroll(tab_manager, state, conversation, delta);
        HandlerOutcome::Quiet
    }
}

/// Dispatch a scroll delta through `view_state.reconcile()` — the single
/// write-path into ViewState.
///
/// D1 (2026-05-03): Rewritten from direct mutation to the reconcile pathway.
/// Uses a minimal LayoutMetrics built from state.total_content_height (the
/// render-derived height from `chat_pane::render`, which walks `messages`)
/// rather than from `build_layout_metrics` (which walks `conversation.turns`).
/// These can diverge; the renderer's value is what the user sees on screen.
///
/// Scroll math is exclusively in `view_state.rs::apply_scroll`.
///
/// Per DGI-A (Winston Task 1 sign-off): pub(crate) helper, NOT a handler.
/// Called by `handle_apply_scroll_intent` (internal) + 2 event_loop.rs
/// dispatch arms (for Top/Bottom jump-intents that bypass the anchor gate).
pub(crate) fn dispatch_view_scroll(
    tab_manager: &mut TabManager,
    state: &mut TuiState,
    _conversation: &Conversation,
    delta: ScrollDelta,
) {
    let layout = LayoutMetrics {
        viewport_height: state.viewport_height as usize,
        total_content_height: state.total_content_height,
        turn_top_offsets: vec![],
        focused_turn_top: None,
    };

    let resolved = tab_manager
        .active_tab_mut()
        .view_state
        .reconcile(Some(ViewEvent::Scroll(delta)), &layout);

    state.scroll_snapshot = resolved;
    state.auto_snapshot = matches!(
        tab_manager.active_tab().view_state.mode,
        AnchorMode::Following
    );
    state.needs_redraw = true;
}
