use ratatui::prelude::*;

use crate::adapters::tui::theme::Theme;

/// Word-boundary text wrapping. Falls back to character wrap for single words
/// exceeding width.
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 || text.is_empty() {
        return vec![];
    }
    let mut result = Vec::new();
    for line in text.split('\n') {
        if line.is_empty() {
            result.push(String::new());
            continue;
        }
        wrap_line_words(line, width, &mut result);
    }
    result
}

/// Wrap a single line at word boundaries. Accumulate words until line exceeds
/// width, break at last whitespace. Single words exceeding width fall back to
/// character wrap.
fn wrap_line_words(line: &str, width: usize, result: &mut Vec<String>) {
    let words: Vec<&str> = line.split_whitespace().collect();
    if words.is_empty() {
        result.push(String::new());
        return;
    }

    let mut current_line = String::new();

    for word in &words {
        let word_len = word.chars().count();
        let current_len = current_line.chars().count();

        if current_line.is_empty() {
            // First word on line
            if word_len <= width {
                current_line.push_str(word);
            } else {
                // Single word exceeding width — character-break
                char_wrap_word(word, width, result);
            }
        } else if current_len + 1 + word_len <= width {
            // Word fits with a space
            current_line.push(' ');
            current_line.push_str(word);
        } else {
            // Word doesn't fit — emit current line, start new one
            result.push(std::mem::take(&mut current_line));
            if word_len <= width {
                current_line.push_str(word);
            } else {
                // Word exceeds full line — character-break
                char_wrap_word(word, width, result);
            }
        }
    }

    if !current_line.is_empty() {
        result.push(current_line);
    }
}

/// Character-break a single word that exceeds the line width.
fn char_wrap_word(word: &str, width: usize, result: &mut Vec<String>) {
    let chars: Vec<char> = word.chars().collect();
    for chunk in chars.chunks(width) {
        result.push(chunk.iter().collect());
    }
}

/// Parse inline code spans (backtick-delimited) and produce styled Lines.
/// Inline code spans should not break mid-span. If a code span exceeds
/// remaining line width, move the entire span to the next line. If it exceeds
/// full line width, character-break within the span.
pub fn parse_inline_code<'a>(text: &str, width: usize, theme: &Theme) -> Vec<Line<'a>> {
    let mut lines = Vec::new();

    for text_line in text.split('\n') {
        let char_count = text_line.chars().count();
        if char_count > width && width > 0 {
            // Word-wrap first, then parse code spans per wrapped line
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
pub fn parse_code_spans(text: &str, theme: &Theme) -> Vec<Span<'static>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_word_wrap_exact_boundary() {
        // Word fills line exactly
        let result = wrap_text("hello world", 11);
        assert_eq!(result, vec!["hello world"]);
    }

    #[test]
    fn test_word_wrap_breaks_at_word() {
        let result = wrap_text("hello world foo", 11);
        assert_eq!(result, vec!["hello world", "foo"]);
    }

    #[test]
    fn test_word_wrap_long_single_word() {
        // Single word exceeding width → character fallback
        let result = wrap_text("abcdefghijklmno", 5);
        assert_eq!(result, vec!["abcde", "fghij", "klmno"]);
    }

    #[test]
    fn test_word_wrap_empty_string() {
        let result = wrap_text("", 10);
        assert!(result.is_empty());
    }

    #[test]
    fn test_word_wrap_single_char() {
        let result = wrap_text("a", 10);
        assert_eq!(result, vec!["a"]);
    }

    #[test]
    fn test_word_wrap_consecutive_whitespace() {
        // split_whitespace handles multiple spaces
        let result = wrap_text("hello   world", 20);
        assert_eq!(result, vec!["hello world"]);
    }

    #[test]
    fn test_word_wrap_newlines() {
        let result = wrap_text("line1\nline2\nline3", 20);
        assert_eq!(result, vec!["line1", "line2", "line3"]);
    }

    #[test]
    fn test_word_wrap_empty_line() {
        let result = wrap_text("hello\n\nworld", 20);
        assert_eq!(result, vec!["hello", "", "world"]);
    }

    #[test]
    fn test_word_wrap_zero_width() {
        let result = wrap_text("hello", 0);
        assert!(result.is_empty());
    }

    #[test]
    fn test_word_wrap_mixed_short_long() {
        let result = wrap_text("hi superlongword ok", 5);
        assert_eq!(result, vec!["hi", "super", "longw", "ord", "ok"]);
    }
}
