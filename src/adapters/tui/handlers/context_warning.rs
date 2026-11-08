//! Context-warning handlers — Story 7.4 (decision half moved to the interim
//! decision pane by Story 21.1 S2).
//!
//! Both handlers return `HandlerOutcome::Quiet` per Task 0 bucketing — pure
//! state mutation on `state.decisions` and the notification store, no event
//! emission, no spawn.
//!
//! Called from `event_loop.rs` dispatch arms when context-window percent
//! crosses thresholds (≥80% upsert, <80% clear). NOT bound to a specific
//! AppEvent variant — these are state-mutation helpers triggered by chunk
//! processing in `ChunkAction::TurnComplete` paths.

#![allow(dead_code)]

use crate::adapters::tui::decision_queue::{self, Arrival, DecisionDraft, DecisionKind};
use crate::adapters::tui::notifications::{
    self, NoticeAction, NoticeDraft, NoticeSeverity, NoticeSource,
};
use crate::adapters::tui::state::TuiState;

use super::HandlerOutcome;

/// Story 7.4: hold the context-warning decision for `conversation_id` and
/// record it in the Notification Log (`pct >= 95` is an Error, lower a
/// Warning), so S-LOG `K`/`n` can raise the decision again after a deferral.
pub fn handle_upsert_context_warning(
    state: &mut TuiState,
    conversation_id: &str,
    pct: u32,
) -> HandlerOutcome {
    let message = format!("Running low on context ({pct}%).");
    let severity = if pct >= 95 {
        NoticeSeverity::Error
    } else {
        NoticeSeverity::Warning
    };
    notifications::notify_draft(
        state,
        NoticeDraft::new(NoticeSource::Context, severity, message.clone())
            .conversation(conversation_id)
            .action(NoticeAction::ContextDecision),
    );
    decision_queue::raise(
        state,
        DecisionDraft::new(DecisionKind::ContextWarning, message).conversation(conversation_id),
        Arrival::Automatic,
    );
    HandlerOutcome::Quiet
}

/// Story 7.4: clear the context-warning decision of `conversation_id` only (a
/// different tab's pending warning is untouched).
pub fn handle_clear_context_warning(state: &mut TuiState, conversation_id: &str) -> HandlerOutcome {
    decision_queue::withdraw_for(state, DecisionKind::ContextWarning, conversation_id);
    state.needs_redraw = true;
    HandlerOutcome::Quiet
}
