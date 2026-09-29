//! Inline `/team retract` decision card — Story 19.16f (`AC4(f)`, ratified
//! `retract-confirm-card`).
//!
//! The text layout is the `/peer add` card's anatomy (title bold, blank, the
//! aligned rows, blank, the paragraphs, blank, decision line last); the chrome
//! is the **double-border** decision card
//! (`inline_card::render_bottom_anchored_decision_card`), whose tail-preserving
//! scroll keeps the key line on screen. This module owns only the styling and
//! the pre-wrap: the `Paragraph` it feeds does not wrap, so a line wider than
//! the pane would be clipped.
//!
//! # It binds no key of its own
//!
//! The keys it paints resolve through `InputAction::TeamRetractConfirm` /
//! `TeamRetractDecline`, gated on `ConfirmationType::TeamRetract` **and** the
//! card's `armed` flag. A disarmed card paints no `[y]`.

use ratatui::prelude::*;

use crate::adapters::tui::state::PendingTeamRetract;
use crate::adapters::tui::theme::Theme;

/// Render the card as chat-pane lines, pre-wrapped to `width`.
pub fn render_team_retract_lines<'a>(
    pending: &PendingTeamRetract,
    theme: &Theme,
    width: u16,
) -> Vec<Line<'a>> {
    // Borders (2) + the 2-cell row indent.
    let inner = (width as usize).saturating_sub(4).max(20);
    let mut lines: Vec<Line<'a>> = Vec::new();
    for (index, raw) in pending.card.lines().enumerate() {
        let style = if raw.starts_with("Awaiting your decision")
            || raw.starts_with("Cannot verify on")
            || raw.starts_with("Already removed on")
        {
            Style::default()
                .fg(theme.colors.accent)
                .add_modifier(Modifier::BOLD)
        } else if index == 0 {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.colors.fg_secondary)
        };
        for chunk in wrap_chars(raw, inner) {
            lines.push(Line::from(vec![Span::styled(format!("  {chunk}"), style)]));
        }
    }
    lines
}

/// Word-wrap one logical line to `width` characters; a word longer than the
/// line (a 35-character item id on a narrow pane) is split rather than
/// clipped. A blank line stays one blank line.
fn wrap_chars(raw: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;
    for word in raw.split(' ') {
        let word_len = word.chars().count();
        if current_len > 0 && current_len + 1 + word_len > width {
            out.push(std::mem::take(&mut current));
            current_len = 0;
        }
        if current_len > 0 {
            current.push(' ');
            current_len += 1;
        }
        let mut rest = word;
        while current_len + rest.chars().count() > width {
            let take = width - current_len;
            let split = rest
                .char_indices()
                .nth(take)
                .map_or(rest.len(), |(at, _)| at);
            current.push_str(&rest[..split]);
            out.push(std::mem::take(&mut current));
            current_len = 0;
            rest = &rest[split..];
        }
        current.push_str(rest);
        current_len += rest.chars().count();
    }
    out.push(current);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(card: &str) -> PendingTeamRetract {
        PendingTeamRetract {
            conversation_id: "conv".to_owned(),
            peer: "jun-dev".to_owned(),
            item_id: "ri_x".to_owned(),
            task: None,
            card: card.to_owned(),
            armed: true,
            prior_focus: crate::domain::models::FocusState::Chat,
        }
    }

    fn texts(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.to_string()).collect())
            .collect()
    }

    /// The `Paragraph` the card feeds does not wrap: a line wider than the
    /// pane must be pre-wrapped, ⛔ never clipped — and the key line stays the
    /// LAST line (tail-preserving overflow keeps it on screen).
    #[test]
    fn every_line_fits_the_pane_and_the_key_line_stays_last() {
        let card = pending(
            "Retract on jun-dev's host\n\nitem         ri_V1StGXR8Z5jdHi6BmyTqWnEcZvPkLd8\n\n\
             This marks the item on their host. It does not delete it, and there is no \
             un-retract. What they already read, they already read.\n\n\
             Awaiting your decision.  [y] Retract  [n] Cancel (Esc)",
        );
        let width = 40;
        let lines = texts(&render_team_retract_lines(
            &card,
            &crate::adapters::tui::theme::Theme::dark(),
            width,
        ));
        for line in &lines {
            assert!(
                line.chars().count() <= usize::from(width) - 2,
                "{line:?} overflows {width}"
            );
        }
        let joined = lines.join(" ");
        assert!(joined.contains("ri_V1StGXR8Z5jdHi6BmyTqWnEcZ"), "{lines:?}");
        assert!(
            lines
                .last()
                .is_some_and(|last| last.contains("Cancel (Esc)")),
            "{lines:?}"
        );
    }
}
