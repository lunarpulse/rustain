use ratatui::prelude::*;

use super::plan_card::wrap_description;
use crate::adapters::tui::text_geometry as geom;
use crate::adapters::tui::theme::Theme;

/// Render a fork confirmation card as styled lines.
///
/// Uses double border `╔═╗` with fork prompt and `[y] Fork  [n] Cancel`.
/// Grapheme-safe, cell-bounded truncation for the message preview.
pub fn render_fork_confirmation_lines(
    message_preview: &str,
    message_index: usize,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let w = width as usize;
    let inner_width = w.saturating_sub(4); // ║ + space + content + space + ║

    let mut lines = Vec::new();

    // Top border: ╔═══════╗
    let top = format!("╔{}╗", "═".repeat(w.saturating_sub(2)));
    lines.push(Line::from(Span::styled(
        top,
        Style::default()
            .fg(theme.colors.accent)
            .add_modifier(Modifier::BOLD),
    )));

    // Title line
    let title = "Fork conversation at this message?";
    let padded_title = format!("║ {} ║", geom::pad_right(title, inner_width));
    lines.push(Line::from(Span::styled(
        padded_title,
        Style::default()
            .fg(theme.colors.fg_primary)
            .add_modifier(Modifier::BOLD),
    )));

    // Message preview line (cell-bounded, with message index): the label's
    // own cells come out of the row budget, the marker is inside the preview's.
    let label_head = format!("Message {}: \"", message_index + 1);
    let preview_budget = 50.min(inner_width.saturating_sub(geom::cells(&label_head) + 1));
    let preview = geom::ellipsize(message_preview, preview_budget, "…");
    let preview_label = format!("{label_head}{preview}\"");
    let padded_preview = format!("║ {} ║", geom::pad_right(&preview_label, inner_width));
    lines.push(Line::from(Span::styled(
        padded_preview,
        Style::default().fg(theme.colors.fg_secondary),
    )));

    // Info line
    let info = "A new tab will be created with messages up to this point.";
    // Wrap if needed
    let info_chunks = wrap_description(info, inner_width);
    for chunk in info_chunks {
        let padded = format!("║ {} ║", geom::pad_right(&chunk, inner_width));
        lines.push(Line::from(Span::styled(
            padded,
            Style::default().fg(theme.colors.fg_muted),
        )));
    }

    // Action line
    let actions = "[y] Fork  [n] Cancel";
    let padded_actions = format!("║ {} ║", geom::pad_right(actions, inner_width));
    lines.push(Line::from(Span::styled(
        padded_actions,
        Style::default()
            .fg(theme.colors.accent)
            .add_modifier(Modifier::BOLD),
    )));

    // Bottom border: ╚═══════╝
    let bottom = format!("╚{}╝", "═".repeat(w.saturating_sub(2)));
    lines.push(Line::from(Span::styled(
        bottom,
        Style::default()
            .fg(theme.colors.accent)
            .add_modifier(Modifier::BOLD),
    )));

    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::theme::Theme;

    #[test]
    fn test_fork_confirmation_renders_double_border() {
        let theme = Theme::dark();
        let lines = render_fork_confirmation_lines("Hello world", 0, 60, &theme);

        // Should have at minimum: top border, title, preview, info, actions, bottom border
        assert!(lines.len() >= 5);

        let first: String = lines[0]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(
            first.starts_with('╔'),
            "Expected '╔' at start, got: {}",
            first
        );
        assert!(first.ends_with('╗'), "Expected '╗' at end, got: {}", first);

        let last: String = lines
            .last()
            .unwrap()
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(
            last.starts_with('╚'),
            "Expected '╚' at start, got: {}",
            last
        );
        assert!(last.ends_with('╝'), "Expected '╝' at end, got: {}", last);
    }

    #[test]
    fn test_fork_confirmation_shows_message_index() {
        let theme = Theme::dark();
        let lines = render_fork_confirmation_lines("Some message content", 2, 60, &theme);
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.to_string())
            .collect();
        // message_index=2 → displayed as "Message 3:"
        assert!(
            all_text.contains("Message 3:"),
            "Expected 'Message 3:' in output"
        );
    }

    #[test]
    fn test_fork_confirmation_shows_actions() {
        let theme = Theme::dark();
        let lines = render_fork_confirmation_lines("msg", 0, 60, &theme);
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.to_string())
            .collect();
        assert!(
            all_text.contains("[y] Fork"),
            "Expected '[y] Fork' in output"
        );
        assert!(
            all_text.contains("[n] Cancel"),
            "Expected '[n] Cancel' in output"
        );
    }

    #[test]
    fn test_fork_confirmation_truncates_long_message_safely() {
        // 200-char ASCII string
        let long_msg: String = "a".repeat(200);
        let theme = Theme::dark();
        let lines = render_fork_confirmation_lines(&long_msg, 0, 60, &theme);
        // Should not panic and should produce a valid output
        assert!(!lines.is_empty());
    }

    #[test]
    fn test_fork_confirmation_truncates_utf8_safely() {
        // Multi-byte UTF-8: each '中' is 3 bytes
        let utf8_msg: String = "中文字符测试消息".repeat(10); // 80 chars
        let theme = Theme::dark();
        // This must not panic
        let lines = render_fork_confirmation_lines(&utf8_msg, 0, 60, &theme);
        assert!(!lines.is_empty());
        // Verify all rendered content is valid UTF-8 (no byte slicing panics)
        for line in &lines {
            for span in &line.spans {
                // Simply accessing the content validates it's valid UTF-8
                let _ = span.content.to_string();
            }
        }
    }
}
