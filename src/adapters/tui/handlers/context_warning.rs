//! Context-warning feedback-block handlers — Story 7.4.
//!
//! Both handlers return `HandlerOutcome::Quiet` per Task 0 bucketing — pure
//! state mutation on `state.feedback_blocks` + `state.active_feedback_id`,
//! no event emission, no spawn.
//!
//! Called from `event_loop.rs` dispatch arms when context-window percent
//! crosses thresholds (≥85% upsert, <85% clear). NOT bound to a specific
//! AppEvent variant — these are state-mutation helpers triggered by chunk
//! processing in `ChunkAction::TurnComplete` paths.

#![allow(dead_code)]

use crate::adapters::tui::state::TuiState;
use crate::domain::models::{FeedbackAction, FeedbackBlock, FeedbackLevel};

use super::HandlerOutcome;

/// Story 7.4: upsert a context-warning feedback block.
///
/// Removes any existing `ctxwarn-*` block first (at-most-one invariant),
/// then inserts a new one. `pct >= 95` emits Error level with Compact +
/// StartFresh actions; lower emits Warning level with same actions.
pub fn handle_upsert_context_warning(state: &mut TuiState, pct: u32) -> HandlerOutcome {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static CTXWARN_COUNTER: AtomicUsize = AtomicUsize::new(0);

    // Remove any existing ctxwarn-* block
    state
        .feedback_blocks
        .retain(|id, _| !id.starts_with("ctxwarn-"));
    if state
        .active_feedback_id
        .as_deref()
        .is_some_and(|id| id.starts_with("ctxwarn-"))
    {
        state.active_feedback_id = None;
    }

    let fb_id = format!(
        "ctxwarn-{}",
        CTXWARN_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let (level, actions) = if pct >= 95 {
        (
            FeedbackLevel::Error,
            vec![FeedbackAction::Compact, FeedbackAction::StartFresh],
        )
    } else {
        (
            FeedbackLevel::Warning,
            vec![FeedbackAction::Compact, FeedbackAction::StartFresh],
        )
    };
    let fb = FeedbackBlock {
        id: fb_id.clone(),
        level,
        message: format!("Running low on context ({}%).", pct),
        actions,
    };
    state.feedback_blocks.insert(fb_id.clone(), fb);
    state.active_feedback_id = Some(fb_id);
    HandlerOutcome::Quiet
}

/// Story 7.4: clear all context-warning feedback blocks.
pub fn handle_clear_context_warning(state: &mut TuiState) -> HandlerOutcome {
    state
        .feedback_blocks
        .retain(|id, _| !id.starts_with("ctxwarn-"));
    if state
        .active_feedback_id
        .as_deref()
        .is_some_and(|id| id.starts_with("ctxwarn-"))
    {
        state.active_feedback_id = None;
    }
    state.needs_redraw = true;
    HandlerOutcome::Quiet
}
