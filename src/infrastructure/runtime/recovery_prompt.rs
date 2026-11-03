//! Startup crash-recovery prompt copy (Story 21.2 AC11, RU-5; Story 21.1 S2 AC9).
//!
//! The prompt is a `Recovery` item of the interim decision pane: this module
//! owns only its producer copy, reachable by tests exactly as the runtime calls
//! it. The keys (`[Enter/Esc] Continue  [n] New session`) belong to the pane's
//! rail, and its `Esc` is Continue — never New, never quit (RU-5).

/// The recovery prompt body: the recovered title and token count. The pane
/// frame carries the title and the real response keys.
pub(crate) fn recovery_detail(title: &str, token_count: u32) -> String {
    format!("Recovered: '{title}' (partial response, {token_count} tokens).")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::decision_pane::{
        Arrival, DecisionDraft, DecisionEffect, DecisionKind, PaneKey, PaneOutcome, handle_key,
        note_painted, raise,
    };
    use crate::adapters::tui::notification_log::LogFacts;
    use crate::adapters::tui::presentation::{self, Surface, is_open};
    use crate::adapters::tui::state::TuiState;
    use crate::domain::clock::MockClock;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn pane_state() -> (TuiState, Arc<MockClock>) {
        let clock = Arc::new(MockClock::new(Instant::now()));
        let mut state = TuiState::new(80, 24);
        state.notifications = crate::adapters::tui::notifications::NotificationStore::from_config(
            &crate::domain::models::NotificationsConfig::default(),
            clock.clone(),
        );
        (state, clock)
    }

    #[test]
    fn story21_2_recovery_detail_names_the_title_and_token_count() {
        let text = recovery_detail("Fix the parser", 1234);
        assert!(
            text.contains("Fix the parser") && text.contains("1234 tokens"),
            "{text}"
        );
    }

    #[test]
    fn story21_1_recovery_esc_continues_and_n_starts_new_through_the_pane() {
        // RU-5: Esc is Continue, `n` is New, an unrelated key never swallows the
        // pane forever (it is consumed once, then Esc still works).
        for (key, expected) in [
            (PaneKey::Esc, Some(DecisionEffect::RecoveryContinue)),
            (PaneKey::Enter, Some(DecisionEffect::RecoveryContinue)),
            (PaneKey::Char('n'), Some(DecisionEffect::RecoveryNewSession)),
            (PaneKey::Char('x'), None),
        ] {
            let (mut state, clock) = pane_state();
            let facts = LogFacts::default();
            raise(
                &mut state,
                DecisionDraft::new(DecisionKind::Recovery, recovery_detail("t", 1)),
                Arrival::Automatic,
            );
            assert!(
                is_open(&state, Surface::DecisionPane),
                "startup recovery takes focus"
            );
            assert!(presentation::consistent(&state));
            assert_eq!(state.modal_stack.len(), 1);
            let frame = state.modal_stack.top().unwrap().clone();
            assert_eq!(frame.surface, Surface::DecisionPane);
            note_painted(&mut state);
            clock.advance(Duration::from_millis(500));
            let outcome = handle_key(&mut state, key, &facts);
            match expected {
                Some(effect) => {
                    assert_eq!(outcome, PaneOutcome::Effect(effect), "{key:?}");
                    assert!(state.decisions.is_empty(), "resolved exactly once");
                    assert!(
                        state.modal_stack.is_empty(),
                        "resolved frame closes exactly once"
                    );
                }
                None => {
                    assert_eq!(outcome, PaneOutcome::Consumed, "{key:?}");
                    assert_eq!(
                        state.decisions.len(),
                        1,
                        "an unrelated key never resolves it"
                    );
                    assert_eq!(state.modal_stack.top(), Some(&frame));
                    let again = handle_key(&mut state, PaneKey::Esc, &facts);
                    assert_eq!(again, PaneOutcome::Effect(DecisionEffect::RecoveryContinue));
                    assert!(state.modal_stack.is_empty());
                }
            }
            assert!(presentation::consistent(&state));
        }
    }
}
