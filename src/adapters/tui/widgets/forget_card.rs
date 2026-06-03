//! Inline `/memory forget` disambiguation/confirm card — Story 11.4a (AC-R0).
//!
//! Reuses the consolidation-card grammar (a `pending_*_card` state field →
//! `render_*_lines` → key intercept → resolution event) — the same "helpful
//! review, never an interruption" inline list the user accepts/declines. Because
//! no `/memory show` exists, this card DOUBLES as the scoped "what's in memory"
//! view: each row shows the matched entry's preview + its stable key, so the user
//! can see exactly what permanent removal will purge from the search index.
//!
//! Monochrome-safe (the selection marker carries meaning, not just colour) and
//! uses the `[mem]` source badge, matching the consolidation card.

use ratatui::prelude::*;

use crate::adapters::tui::state::PendingForgetCard;
use crate::adapters::tui::theme::Theme;

/// Render the forget confirm card as a list of chat-pane lines.
pub fn render_forget_card_lines<'a>(
    card: &PendingForgetCard,
    theme: &Theme,
    width: u16,
) -> Vec<Line<'a>> {
    let mut lines: Vec<Line<'a>> = Vec::new();
    // Account for the marker "  [x] " (6 chars) + key suffix "  [deadbeef]" (~13 chars)
    // so the summary truncation doesn't overflow the pane width.
    let marker_width = 6usize;
    let key_suffix_width = 13usize;
    let inner = (width as usize)
        .saturating_sub(marker_width + key_suffix_width)
        .max(8);

    let count = card.candidates.len();
    // Header: [mem] badge + count. "forget" wording is honest (this purges).
    lines.push(Line::from(vec![Span::styled(
        format!(
            "  [mem] Forget memory — {} match{}",
            count,
            if count == 1 { "" } else { "es" }
        ),
        Style::default()
            .fg(theme.colors.accent)
            .add_modifier(Modifier::BOLD),
    )]));
    lines.push(Line::from(String::new()));

    // One row per matched entry: marker + preview + short key.
    for (idx, (key, entry, selected)) in card.candidates.iter().enumerate() {
        let is_focused = idx == card.focused_index;
        let marker = if *selected { "  [x] " } else { "  [ ] " };
        let focus_indicator = if is_focused { "> " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(
                focus_indicator.to_string(),
                Style::default().fg(if is_focused {
                    theme.colors.accent
                } else {
                    theme.colors.fg_muted
                }),
            ),
            Span::styled(
                marker.to_string(),
                Style::default().fg(if *selected {
                    theme.colors.tool_status_success
                } else {
                    theme.colors.fg_muted
                }),
            ),
            Span::styled(
                truncate(&entry.summary, inner),
                Style::default().fg(theme.colors.fg_primary),
            ),
            Span::styled(
                format!("  [{:016x}]", *key),
                Style::default().fg(theme.colors.fg_muted),
            ),
        ]));
    }

    lines.push(Line::from(String::new()));
    // Key-hint footer. Honest, destructive-action wording.
    lines.push(Line::from(vec![Span::styled(
        "    [↑/↓] navigate  [Space] toggle  [y] forget selected  [n] cancel".to_string(),
        Style::default().fg(theme.colors.fg_muted),
    )]));

    lines
}

/// Char-safe truncation with an ellipsis marker.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::theme::Theme;
    use crate::domain::models::MemoryEntry;
    use chrono::Local;

    fn entry(summary: &str) -> MemoryEntry {
        MemoryEntry {
            timestamp: Local::now(),
            summary: summary.to_string(),
            context: None,
        }
    }

    fn sample_card() -> PendingForgetCard {
        PendingForgetCard {
            conversation_id: "c1".to_string(),
            candidates: vec![
                (0xdead_beef, entry("the secret password is hunter2"), true),
                (0x0000_0001, entry("secret handshake ritual"), true),
            ],
            focused_index: 0,
        }
    }

    fn render_text(card: &PendingForgetCard) -> String {
        let theme = Theme::dark();
        render_forget_card_lines(card, &theme, 80)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_a_row_per_match_with_preview_and_key() {
        let text = render_text(&sample_card());
        assert!(text.contains("the secret password is hunter2"));
        assert!(text.contains("secret handshake ritual"));
        assert!(text.contains("[mem]"));
        assert!(text.contains("2 matches"));
        // honest, destructive wording (AC-R4 — not "hidden")
        assert!(text.contains("forget selected"));
        assert!(text.contains("[n]"));
        // each row shows its stable key (the scoped-view role)
        assert!(text.contains("deadbeef"));
    }

    #[test]
    fn singular_header_for_one_match() {
        let card = PendingForgetCard {
            conversation_id: "c1".to_string(),
            candidates: vec![(7, entry("only one"), true)],
            focused_index: 0,
        };
        let text = render_text(&card);
        assert!(text.contains("1 match"));
        assert!(!text.contains("1 matches"));
    }
}
