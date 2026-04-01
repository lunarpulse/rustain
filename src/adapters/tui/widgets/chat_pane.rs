use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::adapters::tui::theme::Theme;
use crate::domain::models::{ContentBlockType, Conversation, MessageRole, StreamingState};

use super::empty_state;

/// Render the chat pane with full message list, streaming text, and typing indicator.
///
/// Returns `total_content_height` (in lines) — the caller writes this to `TuiState`
/// after `terminal.draw()` completes.
///
/// Individual scalar fields are passed instead of `&TuiState` to avoid borrow conflicts
/// inside the `terminal.draw()` closure.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    conversation: &Conversation,
    streaming: &StreamingState,
    scroll_offset: usize,
    auto_scroll: bool,
    theme: &Theme,
) -> usize {
    // Empty state: no messages and not streaming
    if conversation.messages.is_empty() && !streaming.is_streaming {
        empty_state::render(frame, area, theme);
        return 0;
    }

    let width = area.width as usize;
    if width == 0 {
        return 0;
    }

    // Build all lines for the chat content
    let mut lines: Vec<Line> = Vec::new();
    let spacing = theme.spacing.normal as usize;

    for (i, msg) in conversation.messages.iter().enumerate() {
        if i > 0 {
            // Add spacing between messages
            for _ in 0..spacing {
                lines.push(Line::from(""));
            }
        }

        // Role indicator
        let has_error = msg.content_blocks.contains(&ContentBlockType::Error);
        let role_line = match msg.role {
            MessageRole::User => Line::from(Span::styled(
                "You:",
                Style::default()
                    .fg(theme.colors.accent)
                    .add_modifier(Modifier::BOLD),
            )),
            MessageRole::Assistant => Line::from(Span::styled(
                "Assistant:",
                Style::default()
                    .fg(theme.colors.fg_secondary)
                    .add_modifier(Modifier::BOLD),
            )),
        };
        lines.push(role_line);

        // Message content with inline code span parsing
        if has_error {
            // Error messages render with error color
            let content_lines = wrap_text(&msg.content, width);
            for text in content_lines {
                lines.push(Line::from(Span::styled(
                    text,
                    Style::default().fg(theme.colors.error),
                )));
            }
        } else {
            let parsed_lines = parse_inline_code(&msg.content, width, theme);
            lines.extend(parsed_lines);
        }
    }

    // Streaming content (in-progress assistant message)
    if streaming.is_streaming {
        // Add spacing before streaming content
        if !conversation.messages.is_empty() {
            for _ in 0..spacing {
                lines.push(Line::from(""));
            }
        }

        if streaming.current_text_buffer.is_empty() {
            // Check if streaming has an error block
            let has_streaming_error = streaming.current_blocks.contains(&ContentBlockType::Error);

            if has_streaming_error {
                // Render error text styled
                lines.push(Line::from(Span::styled(
                    "Assistant:",
                    Style::default()
                        .fg(theme.colors.fg_secondary)
                        .add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::from(Span::styled(
                    "Error occurred during streaming",
                    Style::default().fg(theme.colors.error),
                )));
            } else {
                // Typing indicator
                lines.push(Line::from(Span::styled(
                    "···",
                    Style::default().fg(theme.colors.fg_muted),
                )));
            }
        } else {
            // In-progress assistant message
            lines.push(Line::from(Span::styled(
                "Assistant:",
                Style::default()
                    .fg(theme.colors.fg_secondary)
                    .add_modifier(Modifier::BOLD),
            )));

            // Check if streaming has error blocks
            let has_streaming_error = streaming.current_blocks.contains(&ContentBlockType::Error);

            if has_streaming_error {
                let content_lines = wrap_text(&streaming.current_text_buffer, width);
                for text in content_lines {
                    lines.push(Line::from(Span::styled(
                        text,
                        Style::default().fg(theme.colors.error),
                    )));
                }
            } else {
                let parsed_lines = parse_inline_code(&streaming.current_text_buffer, width, theme);
                lines.extend(parsed_lines);
            }
        }
    }

    let total_content_height = lines.len();
    let viewport_height = area.height as usize;

    // Apply scroll offset
    let effective_offset = if auto_scroll {
        0
    } else {
        scroll_offset.min(total_content_height.saturating_sub(viewport_height))
    };

    // Offset-from-bottom: skip lines at the end to scroll up
    let visible_start = if total_content_height > viewport_height {
        total_content_height
            .saturating_sub(viewport_height)
            .saturating_sub(effective_offset)
    } else {
        0
    };

    let visible_lines: Vec<Line> = lines
        .into_iter()
        .skip(visible_start)
        .take(viewport_height)
        .collect();

    let widget = Paragraph::new(Text::from(visible_lines));
    frame.render_widget(widget, area);

    total_content_height
}

/// Simple text wrapping by width.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 || text.is_empty() {
        return vec![];
    }
    let mut result = Vec::new();
    for line in text.split('\n') {
        if line.is_empty() {
            result.push(String::new());
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        for chunk in chars.chunks(width) {
            result.push(chunk.iter().collect());
        }
    }
    result
}

/// Parse inline code spans (backtick-delimited) and produce styled Lines.
/// Unclosed backtick = render as literal backtick character with no style change.
fn parse_inline_code<'a>(text: &str, width: usize, theme: &Theme) -> Vec<Line<'a>> {
    let mut lines = Vec::new();

    for text_line in text.split('\n') {
        // Use char count (not byte length) for wrap trigger to handle multi-byte correctly
        let char_count = text_line.chars().count();
        if char_count > width && width > 0 {
            // Wrap first, then parse code spans per wrapped line
            let wrapped = wrap_text(text_line, width);
            for w in wrapped {
                let line_spans = parse_code_spans(&w, theme);
                lines.push(Line::from(line_spans));
            }
        } else {
            let spans = parse_code_spans(text_line, theme);
            lines.push(Line::from(spans));
        }
    }

    lines
}

/// Parse a single line for backtick-delimited code spans, returning styled Spans.
fn parse_code_spans(text: &str, theme: &Theme) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut chars = text.char_indices().peekable();
    let mut segment_start = 0;

    let normal_style = Style::default().fg(theme.colors.fg_primary);
    let code_style = Style::default()
        .fg(theme.colors.code_span)
        .bg(theme.colors.code_block_bg);

    while let Some(&(i, c)) = chars.peek() {
        if c == '`' {
            // Found opening backtick — look for closing
            let before = &text[segment_start..i];
            let open_pos = i;
            chars.next(); // consume opening backtick

            // Find closing backtick
            let mut found_close = false;
            let mut close_pos = open_pos;
            while let Some(&(j, c2)) = chars.peek() {
                chars.next();
                if c2 == '`' {
                    close_pos = j;
                    found_close = true;
                    break;
                }
            }

            if found_close {
                // Emit text before the backtick
                if !before.is_empty() {
                    spans.push(Span::styled(before.to_string(), normal_style));
                }
                // Emit code span (content between backticks)
                let code_text = &text[open_pos + 1..close_pos];
                spans.push(Span::styled(code_text.to_string(), code_style));
                segment_start = close_pos + 1;
            } else {
                // Unclosed backtick — render as literal
                // Continue from after opening backtick, segment_start unchanged
                // The whole remaining text will be emitted at the end
            }
        } else {
            chars.next();
        }
    }

    // Emit remaining text
    if segment_start < text.len() {
        spans.push(Span::styled(
            text[segment_start..].to_string(),
            normal_style,
        ));
    }

    if spans.is_empty() {
        spans.push(Span::styled(String::new(), normal_style));
    }

    spans
}
