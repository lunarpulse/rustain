use ratatui::prelude::*;

use crate::adapters::tui::theme::Theme;
use crate::domain::models::{FeedbackBlock, FeedbackLevel};

/// Render a FeedbackBlock as styled lines for the chat pane.
///
/// - Error: bold red border `┃`, `✗` symbol, red text, action keys
/// - Warning: yellow thin border `│`, `⚠` symbol, yellow text, action keys (max 3 lines)
/// - Info: blue thin border `│`, `ℹ` symbol, blue text, no action keys
pub fn render_feedback_lines(
    block: &FeedbackBlock,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let (border_char, border_color, symbol, text_color) = match block.level {
        FeedbackLevel::Error => ("┃", theme.colors.error_border, "✗", theme.colors.error),
        FeedbackLevel::Warning => ("│", theme.colors.warning_border, "⚠", theme.colors.warning),
        FeedbackLevel::Info => ("│", theme.colors.info_border, "ℹ", theme.colors.info),
    };

    let border_style = Style::default().fg(border_color);
    let bold_border_style = if block.level == FeedbackLevel::Error {
        border_style.add_modifier(Modifier::BOLD)
    } else {
        border_style
    };

    // Build action string
    let action_text = if block.actions.is_empty() {
        String::new()
    } else {
        let labels: Vec<&str> = block.actions.iter().map(|a| a.key_label()).collect();
        format!(" {}", labels.join("  "))
    };

    // Available width for message text: total - borders - symbol - spacing
    // Layout: "┃ ✗ message text  [r] Retry ┃"
    let overhead = 2 /* left border + space */ + 2 /* symbol + space */ + action_text.len() + 2 /* space + right border */;
    let max_text_width = (width as usize).saturating_sub(overhead);

    // Wrap message text if needed
    let message_lines = wrap_text(&block.message, max_text_width);
    let max_lines = match block.level {
        FeedbackLevel::Error | FeedbackLevel::Info => message_lines.len(),
        FeedbackLevel::Warning => message_lines.len().min(3), // AC2: warning max 3 lines
    };

    let mut lines = Vec::new();
    for (i, msg_line) in message_lines.iter().take(max_lines).enumerate() {
        let mut spans: Vec<Span<'static>> = Vec::new();

        // Left border
        spans.push(Span::styled(format!("{} ", border_char), bold_border_style));

        // Symbol on first line
        if i == 0 {
            spans.push(Span::styled(
                format!("{} ", symbol),
                Style::default().fg(text_color),
            ));
        } else {
            spans.push(Span::raw("  ")); // indent continuation lines
        }

        // Message text
        spans.push(Span::styled(
            msg_line.clone(),
            Style::default().fg(text_color),
        ));

        // Action keys on last line only
        if i == max_lines - 1 && !block.actions.is_empty() {
            // Pad to push actions to the right
            let current_len = 2 + 2 + msg_line.len();
            let actions_len = action_text.len();
            let pad = (width as usize).saturating_sub(current_len + actions_len + 2);
            spans.push(Span::raw(" ".repeat(pad)));

            // Render each action key with green highlight
            for action in &block.actions {
                let label = action.key_label();
                // Split "[x]" from rest
                if let Some(bracket_end) = label.find(']') {
                    spans.push(Span::styled(
                        label[..=bracket_end].to_string(),
                        Style::default().fg(theme.colors.success),
                    ));
                    spans.push(Span::styled(
                        label[bracket_end + 1..].to_string(),
                        Style::default().fg(text_color),
                    ));
                } else {
                    spans.push(Span::styled(
                        label.to_string(),
                        Style::default().fg(text_color),
                    ));
                }
                spans.push(Span::raw(" "));
            }
        }

        // Right border
        spans.push(Span::styled(border_char.to_string(), bold_border_style));

        lines.push(Line::from(spans));
    }

    lines
}

/// Simple word-wrapping for message text.
fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![text.to_string()];
    }
    let mut lines = Vec::new();
    let mut current_line = String::new();

    for word in text.split_whitespace() {
        if current_line.is_empty() {
            current_line = word.to_string();
        } else if current_line.len() + 1 + word.len() > max_width {
            lines.push(current_line);
            current_line = word.to_string();
        } else {
            current_line.push(' ');
            current_line.push_str(word);
        }
    }
    if !current_line.is_empty() {
        lines.push(current_line);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::theme::Theme;
    use crate::domain::models::{FeedbackAction, FeedbackBlock, FeedbackLevel};

    fn test_theme() -> Theme {
        Theme::dark()
    }

    #[test]
    fn test_error_feedback_has_bold_border_and_symbol() {
        let block = FeedbackBlock {
            id: "err-1".to_string(),
            level: FeedbackLevel::Error,
            message: "Couldn't reach Anthropic API".to_string(),
            actions: vec![FeedbackAction::Retry],
        };
        let lines = render_feedback_lines(&block, 80, &test_theme());
        assert!(!lines.is_empty());
        // First line should contain error symbol and border
        let first_line_text: String = lines[0]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(first_line_text.contains('✗'));
        assert!(first_line_text.contains('┃'));
        assert!(first_line_text.contains("[Ctrl+K r]"));
    }

    #[test]
    fn test_warning_feedback_has_thin_border() {
        let block = FeedbackBlock {
            id: "warn-1".to_string(),
            level: FeedbackLevel::Warning,
            message: "Running low on context (92%)".to_string(),
            actions: vec![FeedbackAction::Compact, FeedbackAction::StartFresh],
        };
        let lines = render_feedback_lines(&block, 80, &test_theme());
        assert!(!lines.is_empty());
        let first_line_text: String = lines[0]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(first_line_text.contains('⚠'));
        assert!(first_line_text.contains('│'));
    }

    #[test]
    fn test_info_feedback_no_actions() {
        let block = FeedbackBlock {
            id: "info-1".to_string(),
            level: FeedbackLevel::Info,
            message: "Session restarted".to_string(),
            actions: vec![],
        };
        let lines = render_feedback_lines(&block, 80, &test_theme());
        assert!(!lines.is_empty());
        let first_line_text: String = lines[0]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(first_line_text.contains('ℹ'));
        assert!(!first_line_text.contains("[r]"));
    }

    #[test]
    fn test_warning_max_3_lines() {
        let block = FeedbackBlock {
            id: "warn-2".to_string(),
            level: FeedbackLevel::Warning,
            message: "This is a very long warning message that should be wrapped across multiple lines to test the max 3 line limit for warnings and info blocks".to_string(),
            actions: vec![],
        };
        let lines = render_feedback_lines(&block, 40, &test_theme());
        assert!(lines.len() <= 3);
    }

    #[test]
    fn test_backoff_calculation() {
        use crate::domain::models::next_delay;
        assert_eq!(next_delay(0), 1000);
        assert_eq!(next_delay(1), 2000);
        assert_eq!(next_delay(2), 4000);
        assert_eq!(next_delay(3), 8000);
        assert_eq!(next_delay(4), 16000);
        // Capped at 4 shifts
        assert_eq!(next_delay(5), 16000);
    }

    #[test]
    fn feedback_block_renders_chord_prefix_chips() {
        let block = FeedbackBlock {
            id: "fb-1".to_string(),
            level: FeedbackLevel::Warning,
            message: "Auto-skipped task 3".to_string(),
            actions: vec![FeedbackAction::Retry, FeedbackAction::Compact, FeedbackAction::Dismiss],
        };
        let lines = render_feedback_lines(&block, 80, &test_theme());
        assert!(!lines.is_empty());
        // Actions appear on the last rendered line
        let last_line_text: String = lines
            .last()
            .unwrap()
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(
            last_line_text.contains("[Ctrl+K r]") && last_line_text.contains("[Ctrl+K c]") && last_line_text.contains("[Ctrl+K x]"),
            "all action chips should use chord-prefix: {}",
            last_line_text
        );
    }
}
