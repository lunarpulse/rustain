use ratatui::prelude::*;

use super::transform::{StyledBlock, StyledListItem, StyledSpan};
use crate::adapters::tui::text_geometry as geom;
use crate::adapters::tui::theme::Theme;

/// Right-arrow indicator for clipped long lines in code blocks.
const CLIP_INDICATOR: &str = "→";
/// Bullet character for unordered lists.
const BULLET: &str = "\u{2022} "; // "• "

/// Stage 5: Convert `StyledBlock` list into `Vec<Line<'static>>` for ratatui rendering.
pub fn layout(blocks: Vec<StyledBlock>, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![];
    }

    let mut lines: Vec<Line<'static>> = Vec::new();

    let indent_width = theme.spacing.indent_list as usize;
    let muted_style = Style::default().fg(theme.colors.fg_muted);
    let code_bg_style = Style::default().bg(theme.colors.code_block_bg);
    let code_border_style = Style::default().fg(theme.colors.code_block_border);

    for block in blocks {
        match block {
            StyledBlock::Paragraph(spans) => {
                let para_lines = wrap_spans(spans, width);
                lines.extend(para_lines);
                // 1 blank line after paragraph
                lines.push(Line::from(""));
            }
            StyledBlock::Heading { level: _, spans } => {
                let heading_lines = wrap_spans(spans, width);
                lines.extend(heading_lines);
                // 1 blank line after heading
                lines.push(Line::from(""));
            }
            StyledBlock::List { ordered, items } => {
                render_list_items(&items, ordered, indent_width, width, &mut lines);
            }
            StyledBlock::CodeBlock { language, content } => {
                render_code_block(
                    language.as_deref(),
                    &content,
                    width,
                    &mut lines,
                    code_bg_style,
                    code_border_style,
                );
                // 1 blank line after code block
                lines.push(Line::from(""));
            }
            StyledBlock::ThematicBreak => {
                // Horizontal rule spanning width
                let rule: String = "─".repeat(width);
                lines.push(Line::from(Span::styled(rule, muted_style)));
            }
            StyledBlock::BlankLine => {
                lines.push(Line::from(""));
            }
        }
    }

    lines
}

/// Render list items (unordered or ordered) with proper indentation and hanging indent.
fn render_list_items(
    items: &[StyledListItem],
    ordered: bool,
    indent_width: usize,
    width: usize,
    lines: &mut Vec<Line<'static>>,
) {
    for (idx, item) in items.iter().enumerate() {
        let depth_indent = " ".repeat(item.depth * indent_width);
        let prefix = if ordered {
            format!("{}{}. ", depth_indent, idx + 1)
        } else {
            format!("{}{}", depth_indent, BULLET)
        };
        let prefix_width = display_width(&prefix);

        // Hanging indent string for continuation lines
        let hanging = " ".repeat(prefix_width);

        // Effective content width after prefix
        let content_width = if width > prefix_width {
            width - prefix_width
        } else {
            1
        };

        let wrapped = wrap_spans(item.spans.clone(), content_width);

        for (i, wrapped_line) in wrapped.into_iter().enumerate() {
            let lead = if i == 0 {
                prefix.clone()
            } else {
                hanging.clone()
            };
            let mut spans: Vec<Span<'static>> = vec![Span::raw(lead)];
            spans.extend(wrapped_line.spans);
            lines.push(Line::from(spans));
        }
    }
}

/// Render a fenced code block with a bordered container.
///
/// - Header line: `┌─ <lang> ──────┐` (or just dashes if no lang)
/// - Content lines: clipped to `width - 2` with `→` indicator for long lines
/// - Footer line: `└─────────────────┘`
fn render_code_block(
    language: Option<&str>,
    content: &str,
    width: usize,
    lines: &mut Vec<Line<'static>>,
    bg_style: Style,
    border_style: Style,
) {
    // Minimum usable width: border chars (2) + at least 1 content char
    let inner_width = if width > 2 { width - 2 } else { 1 };

    // ── Header ───────────────────────────────────────────────────────────────
    let header = build_code_header(language, width, border_style);
    lines.push(header);

    // ── Content lines ─────────────────────────────────────────────────────
    let code_lines: Vec<&str> = if content.is_empty() {
        vec![]
    } else {
        content.split('\n').collect()
    };

    for code_line in code_lines {
        let line_width = display_width(code_line);
        if line_width <= inner_width {
            // Pad to inner_width with spaces so background fills the block
            let padding = inner_width - line_width;
            let padded = format!("{}{}", code_line, " ".repeat(padding));
            lines.push(Line::from(vec![
                Span::styled("│", border_style),
                Span::styled(padded, bg_style),
                Span::styled("│", border_style),
            ]));
        } else {
            // Clip with right-arrow indicator
            // Reserve 1 char for the indicator
            let clip_width = if inner_width > 1 { inner_width - 1 } else { 1 };
            let clipped = clip_str(code_line, clip_width);
            lines.push(Line::from(vec![
                Span::styled("│", border_style),
                Span::styled(clipped, bg_style),
                Span::styled(CLIP_INDICATOR, border_style),
                Span::styled("│", border_style),
            ]));
        }
    }

    // ── Footer ────────────────────────────────────────────────────────────────
    let footer_inner = "─".repeat(inner_width);
    let footer = Line::from(vec![Span::styled(
        format!("└{}┘", footer_inner),
        border_style,
    )]);
    lines.push(footer);
}

/// Build the header line for a code block.
///
/// Format: `┌─ <lang> ──────┐` or `┌──────────────┐`
fn build_code_header(language: Option<&str>, width: usize, border_style: Style) -> Line<'static> {
    let inner_width = if width > 2 { width - 2 } else { 1 };

    let header_inner = if let Some(lang) = language {
        let label = format!(" {} ", lang);
        let label_width = display_width(&label);
        if label_width + 2 <= inner_width {
            // "─ lang ─────"
            let remaining = inner_width - label_width - 1; // 1 for leading "─"
            format!("─{}{}", label, "─".repeat(remaining))
        } else {
            // lang too long — just fill with dashes
            "─".repeat(inner_width)
        }
    } else {
        "─".repeat(inner_width)
    };

    Line::from(Span::styled(format!("┌{}┐", header_inner), border_style))
}

/// Clip a string to at most `max_cols` display cells on whole-grapheme
/// boundaries and pad the remainder with spaces to exactly `max_cols` (a wide
/// grapheme that does not fit leaves a padded gap, never a fragment).
fn clip_str(s: &str, max_cols: usize) -> String {
    geom::fit_exact(s, max_cols)
}

/// Display cells of a string under the shared string-width policy.
fn display_width(s: &str) -> usize {
    geom::cells(s)
}

/// Word-wrap a list of `StyledSpan` to `width` columns, returning one `Line` per visual row.
///
/// - Words are separated by a single space (inter-word spaces are normalized).
/// - Spans are kept atomic when possible (never broken mid-span if avoidable).
/// - Uses `unicode-width` for display column calculations (fixes DF-062 for CJK).
#[allow(unused_assignments)] // current_width reset in flush_line! macro is read on the next iteration
pub fn wrap_spans(spans: Vec<StyledSpan>, width: usize) -> Vec<Line<'static>> {
    if width == 0 || spans.is_empty() {
        return vec![Line::from("")];
    }

    // Collect tokens: each is a word + its style, or a hard-newline marker.
    // Space separators are NOT pushed as tokens; they're accounted for as +1
    // when fitting words onto a line.
    struct Token {
        text: String,
        style: Style,
        hard_newline: bool,
    }

    let mut tokens: Vec<Token> = Vec::new();

    for span in &spans {
        let parts: Vec<&str> = span.content.split('\n').collect();
        for (i, part) in parts.iter().enumerate() {
            if i > 0 {
                tokens.push(Token {
                    text: String::new(),
                    style: span.style,
                    hard_newline: true,
                });
            }
            for word in part.split_whitespace() {
                tokens.push(Token {
                    text: word.to_owned(),
                    style: span.style,
                    hard_newline: false,
                });
            }
        }
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut current_width: usize = 0;

    macro_rules! flush_line {
        () => {{
            let line_spans = std::mem::take(&mut current_spans);
            lines.push(Line::from(line_spans));
            current_width = 0;
        }};
    }

    for token in tokens {
        if token.hard_newline {
            flush_line!();
            continue;
        }

        let word = token.text;
        let style = token.style;
        let word_w = display_width(&word);

        if current_width == 0 {
            // First word on the line
            if word_w <= width {
                current_spans.push(Span::styled(word, style));
                current_width = word_w;
            } else {
                // Word wider than full line — clip it
                let clipped = clip_str(&word, width);
                current_spans.push(Span::styled(clipped, style));
                flush_line!();
            }
        } else {
            // Subsequent word: needs a space separator (+1)
            let needed = current_width + 1 + word_w;
            if needed <= width {
                // Append " word" to last span if same style, else new span
                if current_spans.last().map(|s| s.style) == Some(style) {
                    if let Some(last) = current_spans.last_mut() {
                        let new_content = format!("{} {}", last.content, word);
                        *last = Span::styled(new_content, style);
                    }
                } else {
                    current_spans.push(Span::styled(format!(" {}", word), style));
                }
                current_width = needed;
            } else {
                // Wrap to new line
                flush_line!();
                if word_w <= width {
                    current_spans.push(Span::styled(word, style));
                    current_width = word_w;
                } else {
                    let clipped = clip_str(&word, width);
                    current_spans.push(Span::styled(clipped, style));
                    flush_line!();
                }
            }
        }
    }

    // Flush remaining content
    if !current_spans.is_empty() {
        flush_line!();
    }

    if lines.is_empty() {
        lines.push(Line::from(""));
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::theme::Theme;

    fn theme() -> Theme {
        Theme::dark()
    }

    fn plain_span(s: &str) -> StyledSpan {
        StyledSpan {
            content: s.to_owned(),
            style: Style::default(),
        }
    }

    #[test]
    fn test_empty_input_returns_empty() {
        let t = theme();
        let result = layout(vec![], 80, &t);
        assert!(result.is_empty());
    }

    #[test]
    fn test_zero_width_returns_empty() {
        let t = theme();
        let result = layout(
            vec![StyledBlock::Paragraph(vec![plain_span("hello")])],
            0,
            &t,
        );
        assert!(result.is_empty());
    }

    #[test]
    fn test_paragraph_has_blank_line_after() {
        let t = theme();
        let result = layout(
            vec![StyledBlock::Paragraph(vec![plain_span("hello")])],
            80,
            &t,
        );
        // 1 content line + 1 blank line
        assert_eq!(result.len(), 2);
        assert!(result[1].spans.is_empty() || result[1].spans[0].content.is_empty());
    }

    #[test]
    fn test_heading_has_blank_line_after() {
        let t = theme();
        let result = layout(
            vec![StyledBlock::Heading {
                level: 1,
                spans: vec![plain_span("Title")],
            }],
            80,
            &t,
        );
        assert_eq!(result.len(), 2); // heading + blank
    }

    #[test]
    fn test_wrap_at_width() {
        let t = theme();
        // "hello world" at width 5 → "hello" + "world"
        let result = layout(
            vec![StyledBlock::Paragraph(vec![plain_span("hello world")])],
            5,
            &t,
        );
        // 2 content lines + 1 blank = 3
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn test_code_block_has_header_and_footer() {
        let t = theme();
        let result = layout(
            vec![StyledBlock::CodeBlock {
                language: Some("rust".to_owned()),
                content: "fn main() {}".to_owned(),
            }],
            40,
            &t,
        );
        // header + 1 code line + footer + blank = 4
        assert_eq!(result.len(), 4);
        // Header contains ┌
        let header_text: String = result[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(header_text.contains('┌'));
        // Footer contains └
        let footer_text: String = result[2].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(footer_text.contains('└'));
    }

    #[test]
    fn test_code_block_with_language_in_header() {
        let t = theme();
        let result = layout(
            vec![StyledBlock::CodeBlock {
                language: Some("python".to_owned()),
                content: "print('hi')".to_owned(),
            }],
            40,
            &t,
        );
        let header_text: String = result[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(header_text.contains("python"));
    }

    #[test]
    fn test_code_block_long_line_clipped() {
        let t = theme();
        let long_line = "x".repeat(100);
        let result = layout(
            vec![StyledBlock::CodeBlock {
                language: None,
                content: long_line,
            }],
            20,
            &t,
        );
        // Content line should have clip indicator
        let content_line = &result[1];
        let text: String = content_line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains(CLIP_INDICATOR));
    }

    #[test]
    fn test_bullet_list_has_bullet_prefix() {
        let t = theme();
        let result = layout(
            vec![StyledBlock::List {
                ordered: false,
                items: vec![crate::adapters::tui::markdown::transform::StyledListItem {
                    spans: vec![plain_span("item one")],
                    depth: 0,
                }],
            }],
            40,
            &t,
        );
        let text: String = result[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains('•'), "Expected bullet in: {text:?}");
    }

    #[test]
    fn test_ordered_list_has_number_prefix() {
        let t = theme();
        let result = layout(
            vec![StyledBlock::List {
                ordered: true,
                items: vec![
                    crate::adapters::tui::markdown::transform::StyledListItem {
                        spans: vec![plain_span("first")],
                        depth: 0,
                    },
                    crate::adapters::tui::markdown::transform::StyledListItem {
                        spans: vec![plain_span("second")],
                        depth: 0,
                    },
                ],
            }],
            40,
            &t,
        );
        let line0: String = result[0].spans.iter().map(|s| s.content.as_ref()).collect();
        let line1: String = result[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(line0.contains("1."));
        assert!(line1.contains("2."));
    }

    #[test]
    fn test_nested_list_indented() {
        let t = theme();
        let indent_w = t.spacing.indent_list as usize;
        let result = layout(
            vec![StyledBlock::List {
                ordered: false,
                items: vec![
                    crate::adapters::tui::markdown::transform::StyledListItem {
                        spans: vec![plain_span("top")],
                        depth: 0,
                    },
                    crate::adapters::tui::markdown::transform::StyledListItem {
                        spans: vec![plain_span("nested")],
                        depth: 1,
                    },
                ],
            }],
            40,
            &t,
        );
        let line0: String = result[0].spans.iter().map(|s| s.content.as_ref()).collect();
        let line1: String = result[1].spans.iter().map(|s| s.content.as_ref()).collect();
        // Nested item starts with more spaces
        assert!(line1.starts_with(&" ".repeat(indent_w)));
        // Top item does not start with spaces (starts with bullet)
        assert!(!line0.starts_with(' '));
    }

    #[test]
    fn test_thematic_break_spans_width() {
        let t = theme();
        let w = 20;
        let result = layout(vec![StyledBlock::ThematicBreak], w, &t);
        assert_eq!(result.len(), 1);
        let text: String = result[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(display_width(&text), w);
    }

    #[test]
    fn test_wrap_spans_single_line() {
        let spans = vec![plain_span("hello world")];
        let lines = wrap_spans(spans, 80);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn test_wrap_spans_wraps_at_boundary() {
        let spans = vec![plain_span("hello world")];
        let lines = wrap_spans(spans, 5);
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn test_wrap_spans_empty() {
        let lines = wrap_spans(vec![], 80);
        assert_eq!(lines.len(), 1); // returns one empty line
    }

    #[test]
    fn test_wrap_spans_unicode_cjk() {
        // CJK char is 2 display cols wide
        let text = "你好 world";
        let spans = vec![StyledSpan {
            content: text.to_owned(),
            style: Style::default(),
        }];
        let lines = wrap_spans(spans, 4);
        // "你好" = 4 cols → fits on line 1; " world" wraps
        // Actually "你好" is 4 cols, then " world" doesn't fit in 4 cols on same line
        assert!(!lines.is_empty());
    }

    #[test]
    fn test_list_hanging_indent() {
        let t = theme();
        // Single item that wraps → continuation should be indented
        let long_text = "a ".repeat(20); // will wrap
        let result = layout(
            vec![StyledBlock::List {
                ordered: false,
                items: vec![crate::adapters::tui::markdown::transform::StyledListItem {
                    spans: vec![plain_span(long_text.trim())],
                    depth: 0,
                }],
            }],
            20,
            &t,
        );
        // Should produce multiple lines due to wrapping
        if result.len() > 1 {
            let line1: String = result[1].spans.iter().map(|s| s.content.as_ref()).collect();
            let bullet_prefix_w = display_width(BULLET);
            // Hanging indent should match bullet prefix width
            let leading_spaces = line1.chars().take_while(|c| *c == ' ').count();
            assert_eq!(leading_spaces, bullet_prefix_w);
        }
    }

    // ── Story 21.11 display-width tests (entered through `markdown::render`) ──
    //
    // G = 2 cells / 1 grapheme / 6 scalars, C = 2 cells, A = 1 cell / 2 scalars.
    const G: &str = "👨\u{200d}👩\u{200d}👧";
    const C: &str = "界";
    const A: &str = "e\u{301}";

    fn md_lines(markdown: &str, width: usize) -> Vec<Line<'static>> {
        crate::adapters::tui::markdown::render(
            markdown,
            width,
            &theme(),
            &crate::adapters::tui::markdown::RenderOptions::completed(),
        )
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// Content row 1 of a fenced code block (row 0 is the header).
    fn code_row(content: &str, width: usize) -> String {
        let lines = md_lines(&format!("```\n{content}\n```"), width);
        line_text(&lines[1])
    }

    fn assert_row_cells(row: &str, width: usize) {
        assert_eq!(
            geom::cells(row),
            width,
            "row {row:?} must be exactly {width} cells"
        );
    }

    #[test]
    fn display_width_code_block_ascii_positive_control() {
        // 10-cell line: exact fit at width 12, clipped (marker budgeted) at 8.
        let fit = code_row("abcdefghij", 12);
        assert_eq!(fit, "│abcdefghij│");
        let clipped = code_row("abcdefghij", 8);
        assert_eq!(clipped, "│abcde→│");
        assert_row_cells(&clipped, 8);
    }

    #[test]
    fn display_width_code_block_clips_whole_family() {
        let content = G.repeat(4); // 8 cells
        let fit = code_row(&content, 10);
        assert_eq!(fit, format!("│{content}│"));
        assert_row_cells(&fit, 10);
        // Fit-minus-one: three whole families, then the indicator.
        let minus_one = code_row(&content, 9);
        assert_eq!(minus_one, format!("│{}→│", G.repeat(3)));
        assert_row_cells(&minus_one, 9);
        // Next narrower: the fourth family would be cut, so a padded gap
        // replaces it instead of a dangling `👨‍`.
        let narrower = code_row(&content, 8);
        assert_eq!(narrower, format!("│{} →│", G.repeat(2)));
        assert_row_cells(&narrower, 8);
    }

    #[test]
    fn display_width_code_block_clips_cjk_and_combining_whole() {
        let wide = C.repeat(4); // 8 cells
        assert_eq!(code_row(&wide, 10), format!("│{wide}│"));
        assert_eq!(code_row(&wide, 9), format!("│{}→│", C.repeat(3)));
        assert_eq!(code_row(&wide, 8), format!("│{} →│", C.repeat(2)));

        let accent = A.repeat(4); // 4 cells, 8 scalars
        assert_eq!(code_row(&accent, 6), format!("│{accent}│"));
        let clipped = code_row(&accent, 5);
        assert_eq!(clipped, format!("│{}→│", A.repeat(2)));
        assert_row_cells(&clipped, 5);
    }

    #[test]
    fn display_width_prose_oversized_token_ascii_positive_control() {
        let rows: Vec<String> = md_lines("abcdef", 4).iter().map(line_text).collect();
        assert_eq!(rows[0], "abcd");
        let rows: Vec<String> = md_lines("abcdef", 6).iter().map(line_text).collect();
        assert_eq!(rows[0], "abcdef");
    }

    #[test]
    fn display_width_prose_oversized_token_keeps_whole_graphemes() {
        let token = format!("{G}xx"); // 4 cells: exact fit at 4, cut at 3 and 2
        let at = |width: usize| line_text(&md_lines(&token, width)[0]);
        assert_eq!(at(4), token);
        assert_eq!(at(3), format!("{G}x"));
        assert_eq!(at(2), G);
        // Below one grapheme the glyph is omitted, never fragmented.
        let tiny = at(1);
        assert!(
            !tiny.contains('👨') && !tiny.contains('\u{200d}'),
            "fragment of G painted at width 1: {tiny:?}"
        );
        assert_eq!(geom::cells(&tiny), 1);

        let wide = C.repeat(3); // 6 cells
        let at = |width: usize| line_text(&md_lines(&wide, width)[0]);
        assert_eq!(at(6), wide);
        assert_eq!(at(5), format!("{} ", C.repeat(2)));

        let accent = A.repeat(4); // 4 cells
        let at = |width: usize| line_text(&md_lines(&accent, width)[0]);
        assert_eq!(at(4), accent);
        assert_eq!(at(3), A.repeat(3));
    }

    #[test]
    fn display_width_clipped_rows_paint_whole_glyphs_into_buffer() {
        use ratatui::widgets::{Paragraph, Widget};

        let content = G.repeat(4);
        let lines = md_lines(&format!("```\n{content}\n```"), 9);
        let area = Rect::new(0, 0, 9, lines.len() as u16);
        let mut buffer = Buffer::empty(area);
        Paragraph::new(lines).render(area, &mut buffer);
        let symbol = |x: u16| buffer.cell((x, 1)).map(|c| c.symbol().to_owned());
        assert_eq!(symbol(0).as_deref(), Some("│"));
        for x in [1u16, 3, 5] {
            assert_eq!(symbol(x).as_deref(), Some(G), "whole family at column {x}");
        }
        assert_eq!(symbol(7).as_deref(), Some("→"));
        assert_eq!(symbol(8).as_deref(), Some("│"));
    }
}
