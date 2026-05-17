//! Warning-notice helper — cross-epic utility.
//!
//! `apply_warning_notice` is a `pub(crate)` helper per DGI-A (returns `String`
//! — the feedback-block ID — NOT `HandlerOutcome`). Called by warning paths
//! from other handlers + event_loop.rs dispatch arms.
//!
//! DGI-E: the inline `#[cfg(test)] mod tests` from `event_loop.rs::warning_notice_does_not_transfer_focus`
//! was moved alongside the helper during this extraction per Winston Task 1 sign-off.

#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::adapters::tui::state::TuiState;
use crate::domain::models::{FeedbackAction, FeedbackBlock, FeedbackLevel};

/// Apply a warning-level notice to TUI state. Creates a FeedbackBlock,
/// sets `active_feedback_id`, and returns the block ID. Does NOT mutate
/// focus — regression guard for AC6.
pub(crate) fn apply_warning_notice(state: &mut TuiState, msg: String) -> String {
    static WFB_COUNTER: AtomicUsize = AtomicUsize::new(0);
    let fb_id = format!("wfb-{}", WFB_COUNTER.fetch_add(1, Ordering::Relaxed));
    let fb = FeedbackBlock {
        id: fb_id.clone(),
        level: FeedbackLevel::Warning,
        message: msg,
        actions: vec![FeedbackAction::Dismiss],
    };
    state.feedback_blocks.insert(fb_id.clone(), fb);
    state.active_feedback_id = Some(fb_id.clone());
    fb_id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::FocusState;

    #[test]
    fn warning_notice_does_not_transfer_focus() {
        let mut state = TuiState::new(80, 24);
        state.focus = FocusState::Input;
        let fb_id = apply_warning_notice(&mut state, "Auto-skipped task 3".to_string());
        assert_eq!(
            state.focus,
            FocusState::Input,
            "Warning notice must not transfer focus from Input"
        );
        assert_eq!(
            state.active_feedback_id,
            Some(fb_id.clone()),
            "Warning notice must set active_feedback_id"
        );
        assert!(
            state.feedback_blocks.contains_key(&fb_id),
            "Warning notice must insert feedback block"
        );
    }
}
