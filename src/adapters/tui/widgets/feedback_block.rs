use ratatui::prelude::*;
use unicode_width::UnicodeWidthStr as _;

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
            let current_len = 2 + 2 + msg_line.width();
            let actions_len = action_text.width();
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

/// Word-wrapping for message text that keeps the message's line structure.
///
/// Each logical line (split on `'\n'`) is laid out independently: one that
/// fits is kept verbatim (its leading-space run and its column spacing are
/// layout — `/team board`'s peer and item rows are built from them); one
/// that does not fit word-wraps with its leading-space run on its first
/// visual line only. Continuation lines carry only the renderer's own 2-cell
/// indent. Widths count terminal cells.
///
/// A single word wider than a whole line (an alias, an id) is broken only
/// between styled grapheme clusters, as on the recipient surface, so accents
/// and joined emoji survive painting at the 60-column floor.
fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for logical in text.lines() {
        let logical = logical.trim_end();
        if max_width == 0 || logical.width() <= max_width {
            lines.push(logical.to_string());
            continue;
        }
        let body = logical.trim_start_matches(' ');
        let indent = &logical[..logical.len() - body.len()];
        // The first visual line carries the leading-space run.
        let mut current = String::from(indent);
        let mut current_width = indent.width();
        // Nothing but that run is on the line yet.
        let mut fresh = true;
        for word in body.split_whitespace() {
            let mut rest = word;
            let mut remaining_width = word.width();
            loop {
                let separator = usize::from(!fresh);
                let room = max_width.saturating_sub(current_width + separator);
                if remaining_width <= room {
                    if !fresh {
                        current.push(' ');
                    }
                    current.push_str(rest);
                    fresh = false;
                    current_width += separator + remaining_width;
                    break;
                }
                if !fresh {
                    // Try the word again on a line of its own.
                    lines.push(std::mem::take(&mut current));
                    current_width = 0;
                    fresh = true;
                    continue;
                }
                // Preserve whole graphemes; a standalone combining mark is
                // otherwise lost when ratatui paints the next message span.
                let span = Span::raw(rest);
                let mut cut = 0;
                let mut cut_width = 0;
                for grapheme in span.styled_graphemes(Style::default()) {
                    let cells = grapheme.symbol.width();
                    if cut_width + cells > room {
                        break;
                    }
                    cut += grapheme.symbol.len();
                    cut_width += cells;
                }
                if cut == 0 {
                    if !current.is_empty() {
                        lines.push(std::mem::take(&mut current));
                        current_width = 0;
                        continue;
                    }
                    // A terminal narrower than one grapheme cannot split it
                    // losslessly. Keep it intact rather than corrupting it.
                    let grapheme = span
                        .styled_graphemes(Style::default())
                        .next()
                        .expect("an over-wide word is nonempty");
                    cut = grapheme.symbol.len();
                    cut_width = grapheme.symbol.width();
                }
                current.push_str(&rest[..cut]);
                lines.push(std::mem::take(&mut current));
                current_width = 0;
                remaining_width -= cut_width;
                rest = &rest[cut..];
            }
        }
        lines.push(current);
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
            actions: vec![
                FeedbackAction::Retry,
                FeedbackAction::Compact,
                FeedbackAction::Dismiss,
            ],
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
            last_line_text.contains("[Ctrl+K r]")
                && last_line_text.contains("[Ctrl+K c]")
                && last_line_text.contains("[Ctrl+K x]"),
            "all action chips should use chord-prefix: {}",
            last_line_text
        );
    }

    fn message_texts(block: &FeedbackBlock, width: u16) -> Vec<String> {
        render_feedback_lines(block, width, &test_theme())
            .iter()
            .map(|line| {
                // Drop the border+symbol (or continuation indent) prefix spans
                // and the right border: keep only the message span.
                line.spans[2].content.to_string()
            })
            .collect()
    }

    fn info(message: &str) -> FeedbackBlock {
        FeedbackBlock {
            id: "info-structured".to_string(),
            level: FeedbackLevel::Info,
            message: message.to_string(),
            actions: vec![],
        }
    }

    #[test]
    fn a_multi_line_info_block_keeps_one_visual_line_per_logical_line() {
        let block = info("  ● jun-dev  delivered\n  ● ana  pending\n  ● bo  unknown");
        let texts = message_texts(&block, 120);
        assert_eq!(
            texts.len(),
            3,
            "each logical line is its own row: {texts:?}"
        );
        assert_eq!(texts[1], "  ● ana  pending");
    }

    #[test]
    fn each_logical_line_keeps_its_leading_space_run() {
        let block = info("  ● jun-dev  delivered\n    item ri_a · arrival 1 · task t · received");
        let texts = message_texts(&block, 120);
        assert_eq!(texts[1], "    item ri_a · arrival 1 · task t · received");
    }

    #[test]
    fn an_indented_line_that_wraps_keeps_its_indent_only_on_its_first_visual_line() {
        let block = info("    item ri_abcdef · arrival 1 · task task-7f2a · received");
        // overhead = 2 + 2 + 0 + 2 = 6 ⇒ 30 cells of text.
        let texts = message_texts(&block, 36);
        assert!(texts.len() > 1, "{texts:?}");
        assert!(texts[0].starts_with("    item"), "{texts:?}");
        assert!(
            !texts[1].starts_with(' '),
            "a continuation carries the renderer's 2-cell indent, never the row's: {texts:?}"
        );
    }

    #[test]
    fn a_single_line_message_still_renders_as_one_line() {
        let texts = message_texts(&info("Session restarted"), 120);
        assert_eq!(texts, vec!["Session restarted".to_string()]);
    }

    /// `TM-02`'s width rule: a token wider than the whole block (a long alias
    /// or id) is character-broken, never left to overflow and be clipped.
    /// Re-joining the visual lines yields the token byte-for-byte.
    ///
    /// **Mutant → RED:** append an over-wide word whole instead of breaking it.
    #[test]
    fn a_token_wider_than_the_block_is_character_broken_and_loses_nothing() {
        let token = "a".repeat(200);
        let block = info(&format!("  ● {token}  delivered"));
        // overhead = 6 ⇒ 24 cells of text.
        let texts = message_texts(&block, 30);
        assert!(
            texts.iter().all(|text| text.len() <= 24),
            "no visual line exceeds the block: {texts:?}"
        );
        assert_eq!(
            texts.concat().matches(&token).count(),
            1,
            "the whole token survives the break: {texts:?}"
        );
    }

    /// A break never splits a `char`, and the first visual line keeps the
    /// leading-space run while its continuations do not.
    #[test]
    fn breaking_an_overwide_token_is_utf8_safe_and_keeps_the_first_line_indent() {
        let token = "é".repeat(40);
        let block = info(&format!("    {token} tail"));
        let texts = message_texts(&block, 26);
        assert!(texts[0].starts_with("    "), "{texts:?}");
        assert!(
            texts[1..].iter().all(|text| !text.starts_with(' ')),
            "a continuation never repeats the row's indent: {texts:?}"
        );
        assert_eq!(texts.join("").replace(' ', "").matches(&token).count(), 1);
    }

    #[test]
    fn grapheme_fields_survive_actual_painting_at_sixty_columns() {
        use ratatui::{buffer::Buffer, widgets::Widget};

        for token in [
            format!("{}e\u{301}b", "x".repeat(53)),
            format!("{}👩‍🔬b", "x".repeat(53)),
            "界".repeat(40),
        ] {
            let lines = render_feedback_lines(&info(&token), 60, &test_theme());
            let mut buffer = Buffer::empty(Rect::new(0, 0, 60, lines.len() as u16));
            for (index, line) in lines.into_iter().enumerate() {
                assert!(
                    line.width() <= 60,
                    "a field must fit without terminal clipping"
                );
                line.render(Rect::new(0, index as u16, 60, 1), &mut buffer);
            }
            let painted: String = buffer
                .content
                .iter()
                .flat_map(|cell| cell.symbol().chars())
                .filter(|character| {
                    !character.is_whitespace() && *character != '│' && *character != 'ℹ'
                })
                .collect();
            assert_eq!(
                painted, token,
                "painted graphemes must retain the complete field"
            );
        }
    }
}
