use ratatui::prelude::*;

use crate::adapters::tui::decision_queue::ForkPayload;
use crate::adapters::tui::text_geometry as geom;
use crate::adapters::tui::theme::Theme;

/// Build the fork confirmation body without the frame or action rail.
pub fn lines(payload: &ForkPayload, theme: &Theme) -> Vec<Line<'static>> {
    let preview = geom::ellipsize(&payload.preview, 50, "…");
    vec![
        Line::from(Span::styled(
            "Fork conversation at this message?",
            Style::default()
                .fg(theme.colors.fg_primary)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("Message {}: \"{}\"", payload.message_index + 1, preview),
            Style::default().fg(theme.colors.fg_secondary),
        )),
        Line::from(Span::styled(
            "A new tab will be created with messages up to this point.",
            Style::default().fg(theme.colors.fg_muted),
        )),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::theme::Theme;

    fn payload(preview: &str, message_index: usize) -> ForkPayload {
        ForkPayload {
            conversation_id: "c1".into(),
            message_index,
            target_hash: 0,
            preview: preview.into(),
        }
    }

    #[test]
    fn fork_body_has_no_border_or_action_rail() {
        let text: String = lines(&payload("Hello world", 0), &Theme::dark())
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains("Fork conversation at this message?"));
        assert!(!text.contains('║'));
        assert!(!text.contains("[y]"));
    }

    #[test]
    fn test_fork_confirmation_shows_message_index() {
        let theme = Theme::dark();
        let lines = lines(&payload("Some message content", 2), &theme);
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
    fn fork_body_leaves_actions_to_frame() {
        let text: String = lines(&payload("msg", 0), &Theme::dark())
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect();
        assert!(!text.contains("[y] Fork"));
        assert!(!text.contains("[n] Cancel"));
    }

    #[test]
    fn test_fork_confirmation_truncates_long_message_safely() {
        // 200-char ASCII string
        let long_msg: String = "a".repeat(200);
        let theme = Theme::dark();
        let lines = lines(&payload(&long_msg, 0), &theme);
        // Should not panic and should produce a valid output
        assert!(!lines.is_empty());
    }

    #[test]
    fn test_fork_confirmation_truncates_utf8_safely() {
        // Multi-byte UTF-8: each '中' is 3 bytes
        let utf8_msg: String = "中文字符测试消息".repeat(10); // 80 chars
        let theme = Theme::dark();
        // This must not panic
        let lines = lines(&payload(&utf8_msg, 0), &theme);
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
