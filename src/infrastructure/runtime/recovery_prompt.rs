//! Startup crash-recovery prompt: producer copy and key routing (Story 21.2 AC11, RU-5).
//!
//! Extracted from `event_loop::run` so the producer and its interceptor are
//! reachable by tests exactly as the runtime calls them. The block carries NO
//! severity glyph of its own: `feedback_block::render_feedback_lines*` owns the
//! one level icon (D10 — the producer used to prepend U+2139 and the renderer
//! added a second one).

use crate::domain::events::{DomainInputEvent, DomainKey};
use crate::domain::models::{FeedbackAction, FeedbackBlock, FeedbackLevel};

/// Feedback-block id the runtime interceptor keys on.
pub(crate) const RECOVERY_BLOCK_ID: &str = "recovery";

/// The recovery prompt block: title and token count in the body, the real
/// response keys as its action row (`[Enter/Esc] Continue  [n] New session`).
pub(crate) fn recovery_block(title: &str, token_count: u32) -> FeedbackBlock {
    FeedbackBlock {
        id: RECOVERY_BLOCK_ID.to_string(),
        level: FeedbackLevel::Info,
        message: format!("Recovered: '{title}' (partial response, {token_count} tokens)."),
        actions: vec![
            FeedbackAction::Custom("[Enter/Esc] Continue".to_string()),
            FeedbackAction::Custom("[n] New session".to_string()),
        ],
    }
}

/// What a keystroke means while the recovery prompt is the active feedback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryChoice {
    /// Keep the recovered conversation (Enter / Esc / `y`).
    Continue,
    /// Start a fresh session (`n`).
    NewSession,
    /// The existing Ctrl+C / Ctrl+Q escape hatch: nothing is running yet.
    Quit,
    /// Every other key is swallowed while the prompt is up.
    Swallow,
}

/// Classify a key for the recovery interceptor. Esc is Continue, never New and
/// never quit (RU-5).
pub(crate) fn recovery_choice(event: &DomainInputEvent) -> RecoveryChoice {
    match event {
        DomainInputEvent::SpecialKey(DomainKey::Enter | DomainKey::Esc)
        | DomainInputEvent::KeyPress('y') => RecoveryChoice::Continue,
        DomainInputEvent::KeyPress('n') => RecoveryChoice::NewSession,
        DomainInputEvent::SpecialKey(DomainKey::CtrlC | DomainKey::CtrlQ) => RecoveryChoice::Quit,
        _ => RecoveryChoice::Swallow,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::theme::Theme;
    use crate::adapters::tui::widgets::feedback_block::render_feedback_lines;

    fn rendered(block: &FeedbackBlock, width: u16) -> String {
        render_feedback_lines(block, width, &Theme::dark())
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn story21_2_recovery_prompt_paints_exactly_one_level_glyph() {
        let block = recovery_block("Fix the parser", 1234);
        let text = rendered(&block, 200);
        assert_eq!(text.matches('ℹ').count(), 1, "one level icon: {text}");
        assert!(text.contains("Fix the parser") && text.contains("1234 tokens"));
        assert!(text.contains("[Enter/Esc] Continue"));
        assert!(text.contains("[n] New session"));
        // Positive control: an ordinary unprefixed Info block keeps its renderer icon.
        let plain = FeedbackBlock {
            id: "plain".into(),
            level: FeedbackLevel::Info,
            message: "i is a literal body letter".into(),
            actions: Vec::new(),
        };
        let plain_text = rendered(&plain, 80);
        assert_eq!(plain_text.matches('ℹ').count(), 1);
        assert!(plain_text.contains("i is a literal body letter"));
    }

    #[test]
    fn story21_2_recovery_esc_continues_and_n_starts_new() {
        let esc = DomainInputEvent::SpecialKey(DomainKey::Esc);
        let enter = DomainInputEvent::SpecialKey(DomainKey::Enter);
        assert_eq!(recovery_choice(&esc), RecoveryChoice::Continue);
        assert_eq!(recovery_choice(&enter), RecoveryChoice::Continue);
        assert_eq!(
            recovery_choice(&DomainInputEvent::KeyPress('n')),
            RecoveryChoice::NewSession
        );
        assert_eq!(
            recovery_choice(&DomainInputEvent::SpecialKey(DomainKey::CtrlC)),
            RecoveryChoice::Quit
        );
        assert_eq!(
            recovery_choice(&DomainInputEvent::KeyPress('x')),
            RecoveryChoice::Swallow
        );
    }
}
