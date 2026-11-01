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
use crate::adapters::tui::text_geometry as geom;
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

/// Word-wrap one logical line to `width` display cells; a word longer than the
/// line (a 35-character item id on a narrow pane) is split at grapheme
/// boundaries rather than clipped. A blank line stays one blank line.
fn wrap_chars(raw: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut current_cells = 0usize;
    for word in raw.split(' ') {
        let word_cells = geom::cells(word);
        if current_cells > 0 && current_cells + 1 + word_cells > width {
            out.push(std::mem::take(&mut current));
            current_cells = 0;
        }
        if current_cells > 0 {
            current.push(' ');
            current_cells += 1;
        }
        let mut rest = word;
        while current_cells + geom::cells(rest) > width {
            let mut piece = geom::clip(rest, width.saturating_sub(current_cells));
            if piece.is_empty() {
                if current_cells > 0 {
                    out.push(std::mem::take(&mut current));
                    current_cells = 0;
                    continue;
                }
                // One grapheme wider than the whole row: keep it intact on its
                // own row so the loop always makes progress.
                let first = geom::byte_of_scalar(rest, geom::next_boundary(rest, 0));
                piece = &rest[..first];
            }
            current.push_str(piece);
            out.push(std::mem::take(&mut current));
            current_cells = 0;
            rest = &rest[piece.len()..];
        }
        current.push_str(rest);
        current_cells += geom::cells(rest);
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
             Awaiting your decision.  [y] Retract  [n] Cancel",
        );
        let width = 40;
        let lines = texts(&render_team_retract_lines(
            &card,
            &crate::adapters::tui::theme::Theme::dark(),
            width,
        ));
        for line in &lines {
            assert!(
                geom::cells(line) <= usize::from(width) - 2,
                "{line:?} overflows {width}"
            );
        }
        let joined = lines.join(" ");
        assert!(joined.contains("ri_V1StGXR8Z5jdHi6BmyTqWnEcZ"), "{lines:?}");
        assert!(
            lines.last().is_some_and(|last| last.contains("[n] Cancel")),
            "{lines:?}"
        );

        // Long unbroken wide words: every row fits the pane in display cells,
        // no cluster is split, and no source character is lost.
        const G: &str = "👨\u{200d}👩\u{200d}👧";
        const C: &str = "界";
        let keys = "Awaiting your decision.  [y] Retract  [n] Cancel";
        for word in [
            C.repeat(40),
            G.repeat(30),
            format!("{}{}", "x".repeat(35), G.repeat(10)),
        ] {
            let source = format!("Retract on jun-dev's host\n\nitem         {word}\n\n{keys}");
            for width in [40u16, 60] {
                let lines = texts(&render_team_retract_lines(
                    &pending(&source),
                    &crate::adapters::tui::theme::Theme::dark(),
                    width,
                ));
                for line in &lines {
                    assert!(
                        geom::cells(line) <= usize::from(width) - 2,
                        "{line:?} overflows {width}"
                    );
                    for g in
                        unicode_segmentation::UnicodeSegmentation::graphemes(line.as_str(), true)
                    {
                        if g.contains('👨') || g.contains('👩') || g.contains('👧') {
                            assert_eq!(g, G, "split cluster in {line:?}");
                        }
                    }
                }
                let kept: String = lines.concat().split_whitespace().collect();
                let want: String = source.split_whitespace().collect();
                assert_eq!(kept, want, "width {width}: source lost");
                assert!(
                    lines.last().is_some_and(|last| last.contains("[n] Cancel")),
                    "{lines:?}"
                );
            }
        }
    }
}
