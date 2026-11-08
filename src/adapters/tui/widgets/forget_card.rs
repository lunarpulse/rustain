//! Memory-forget decision frame body.

use ratatui::prelude::*;

use crate::adapters::tui::decision_queue::PendingForgetCard;
use crate::adapters::tui::theme::Theme;

/// Build the forget confirmation body; the decision frame wraps its rows.
pub fn lines(card: &PendingForgetCard, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();

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
        let key_suffix = format!("  [{:016x}]", *key);
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
                entry.summary.clone(),
                Style::default().fg(theme.colors.fg_primary),
            ),
            Span::styled(key_suffix, Style::default().fg(theme.colors.fg_muted)),
        ]));
    }

    lines
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
        lines(card, &theme)
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
        assert!(!text.contains("[n]"));
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
