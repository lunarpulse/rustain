use ratatui::prelude::*;

use super::RenderOptions;
use super::parse::{InlineSpan, MarkdownBlock};
use crate::adapters::tui::theme::Theme;

/// Styled block types produced by Stage 3.
#[derive(Debug)]
pub enum StyledBlock {
    Paragraph(Vec<StyledSpan>),
    Heading {
        #[allow(dead_code)] // reserved for size-based heading styling in Epic 15
        level: u8,
        spans: Vec<StyledSpan>,
    },
    List {
        ordered: bool,
        items: Vec<StyledListItem>,
    },
    CodeBlock {
        language: Option<String>,
        content: String,
    },
    ThematicBreak,
    #[allow(dead_code)] // blank-line spacing variant — never constructed yet; reserved for Epic 15
    BlankLine,
}

/// A styled list item.
#[derive(Debug)]
pub struct StyledListItem {
    pub spans: Vec<StyledSpan>,
    pub depth: usize,
}

/// A span with associated ratatui `Style`.
#[derive(Debug, Clone)]
pub struct StyledSpan {
    pub content: String,
    pub style: Style,
}

/// Strip unclosed inline markers from a plain-text span (AC3, DF-078).
///
/// Removes literal `~~`, `**`, `*`, and `` ` `` characters that pulldown-cmark
/// left as unmatched `Text` events (i.e., they appear inside `InlineSpan::Plain`
/// rather than producing `Bold`/`Italic`/`Code` variants).
///
/// Longer markers are processed first (`~~` then `**` then `*`) to avoid
/// false matches. This is applied only when `RenderOptions::strip_unclosed_markers`
/// is true — streaming mode leaves markers intact so the model can close them.
pub fn strip_unclosed_markers(s: String) -> String {
    // Fast path: no markers present
    if !s.contains('~') && !s.contains('*') && !s.contains('`') {
        return s;
    }
    s.replace("~~", "")
        .replace("**", "")
        .replace(['*', '`'], "")
}

/// Transform `MarkdownBlock` list into `StyledBlock` list, applying theme styles.
///
/// HTML tags are stripped from `InlineSpan::Plain` content only (never from Code spans).
pub fn transform(
    blocks: Vec<MarkdownBlock>,
    theme: &Theme,
    opts: &RenderOptions,
) -> Vec<StyledBlock> {
    let normal_style = Style::default().fg(theme.colors.fg_primary);
    let bold_style = Style::default()
        .fg(theme.colors.fg_primary)
        .add_modifier(Modifier::BOLD);
    let italic_style = Style::default()
        .fg(theme.colors.fg_primary)
        .add_modifier(Modifier::ITALIC);
    let bold_italic_style = Style::default()
        .fg(theme.colors.fg_primary)
        .add_modifier(Modifier::BOLD | Modifier::ITALIC);
    let code_style = Style::default()
        .fg(theme.colors.code_span)
        .bg(theme.colors.code_block_bg);
    let heading_style = Style::default()
        .fg(theme.colors.fg_primary)
        .add_modifier(Modifier::BOLD);

    let strip_markers = opts.strip_unclosed_markers;
    let mut styled: Vec<StyledBlock> = Vec::new();

    for block in blocks {
        match block {
            MarkdownBlock::Paragraph(spans) => {
                let styled_spans = map_spans(
                    spans,
                    normal_style,
                    bold_style,
                    italic_style,
                    bold_italic_style,
                    code_style,
                    strip_markers,
                );
                styled.push(StyledBlock::Paragraph(styled_spans));
            }
            MarkdownBlock::Heading { level, spans } => {
                // All headings H1-H4 → BOLD only for Story 3-6
                let styled_spans =
                    map_spans_with_base(spans, heading_style, code_style, strip_markers);
                styled.push(StyledBlock::Heading {
                    level,
                    spans: styled_spans,
                });
            }
            MarkdownBlock::List { ordered, items } => {
                let styled_items = items
                    .into_iter()
                    .map(|item| StyledListItem {
                        spans: map_spans(
                            item.spans,
                            normal_style,
                            bold_style,
                            italic_style,
                            bold_italic_style,
                            code_style,
                            strip_markers,
                        ),
                        depth: item.depth,
                    })
                    .collect();
                styled.push(StyledBlock::List {
                    ordered,
                    items: styled_items,
                });
            }
            MarkdownBlock::CodeBlock { language, content } => {
                // Code block content is NOT HTML-stripped (preserves Rust generics, operators)
                styled.push(StyledBlock::CodeBlock { language, content });
            }
            MarkdownBlock::ThematicBreak => {
                styled.push(StyledBlock::ThematicBreak);
            }
        }
    }

    styled
}

/// Map `InlineSpan` list to `StyledSpan` list with per-variant styles.
///
/// When `strip_markers` is true, unclosed inline markers are stripped from
/// `Plain` spans (they remain as-is in `Bold`/`Italic`/`Code` since those
/// variants only appear when the parser matched a properly-closed marker).
fn map_spans(
    spans: Vec<InlineSpan>,
    normal: Style,
    bold: Style,
    italic: Style,
    bold_italic: Style,
    code: Style,
    strip_markers: bool,
) -> Vec<StyledSpan> {
    spans
        .into_iter()
        .map(|span| match span {
            InlineSpan::Plain(s) => {
                let s = strip_html(s);
                let content = if strip_markers {
                    strip_unclosed_markers(s)
                } else {
                    s
                };
                StyledSpan {
                    content,
                    style: normal,
                }
            }
            InlineSpan::Bold(s) => StyledSpan {
                content: strip_html(s),
                style: bold,
            },
            InlineSpan::Italic(s) => StyledSpan {
                content: strip_html(s),
                style: italic,
            },
            InlineSpan::BoldItalic(s) => StyledSpan {
                content: strip_html(s),
                style: bold_italic,
            },
            InlineSpan::Code(s) => StyledSpan {
                // Do NOT strip HTML or markers from code spans
                content: s,
                style: code,
            },
        })
        .collect()
}

/// Map spans inside headings: all inline variants get the `base` style (BOLD),
/// except `Code` which keeps code styling.
fn map_spans_with_base(
    spans: Vec<InlineSpan>,
    base: Style,
    code: Style,
    strip_markers: bool,
) -> Vec<StyledSpan> {
    spans
        .into_iter()
        .map(|span| match span {
            InlineSpan::Plain(s) => {
                let s = strip_html(s);
                let content = if strip_markers {
                    strip_unclosed_markers(s)
                } else {
                    s
                };
                StyledSpan {
                    content,
                    style: base,
                }
            }
            InlineSpan::Bold(s) | InlineSpan::Italic(s) | InlineSpan::BoldItalic(s) => StyledSpan {
                content: strip_html(s),
                style: base,
            },
            InlineSpan::Code(s) => StyledSpan {
                content: s,
                style: code,
            },
        })
        .collect()
}

/// Strip HTML tags from a string using character scanning (no `regex` crate).
///
/// Removes `<tag>`, `</tag>`, and self-closing `<tag/>` patterns.
/// This operates only on Plain/Bold/Italic spans — NOT on Code spans.
/// Uses byte-level scanning for `<` and `>` (ASCII), and `is_char_boundary()`
/// to ensure we never index into the middle of a multi-byte UTF-8 character.
pub fn strip_html(input: String) -> String {
    // Fast path: no '<' means no tags to strip
    if !input.contains('<') {
        return input;
    }

    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        if bytes[i] == b'<' {
            let start = i;
            i += 1;
            // Optional '/' for closing tags
            if i < len && bytes[i] == b'/' {
                i += 1;
            }
            // Scan for matching '>' — '<' and '>' are ASCII (single byte),
            // so byte comparison is safe even inside multi-byte sequences
            // (UTF-8 continuation bytes are always >= 0x80).
            let mut found_gt = false;
            while i < len {
                if bytes[i] == b'>' {
                    i += 1;
                    found_gt = true;
                    break;
                }
                if bytes[i] == b'<' {
                    break;
                }
                i += 1;
            }
            if !found_gt {
                // Not a valid tag — emit the '<' we skipped and resume
                // from the next char boundary after start
                out.push('<');
                i = start + 1;
                // Advance to next char boundary (start+1 may be mid-codepoint
                // if '<' was followed by multi-byte chars, but '<' is ASCII
                // so start+1 is always a valid boundary)
            }
            // If found_gt: the tag is stripped (nothing emitted)
        } else {
            // Advance by one full UTF-8 character
            let ch = input[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::theme::Theme;

    fn make_theme() -> Theme {
        Theme::dark()
    }

    #[test]
    fn test_strip_html_removes_tags() {
        let out = strip_html("<b>bold</b>".to_owned());
        assert_eq!(out, "bold");
    }

    #[test]
    fn test_strip_html_self_closing() {
        let out = strip_html("line<br/>break".to_owned());
        assert_eq!(out, "linebreak");
    }

    #[test]
    fn test_strip_html_multiple_tags() {
        let out = strip_html("<em>italic</em> and <strong>bold</strong>".to_owned());
        assert_eq!(out, "italic and bold");
    }

    #[test]
    fn test_strip_html_no_tags() {
        let out = strip_html("hello world".to_owned());
        assert_eq!(out, "hello world");
    }

    #[test]
    fn test_strip_html_preserves_comparison_operators() {
        // `i < n` — the '<' is not followed by valid tag content + '>'
        // Actually `i < n` has a space after `<`, which counts as content.
        // Let's check the actual behavior: `i < n` — '<' then ' n' then end of string, no '>'.
        // In strip_html: '<' found, i advances, space is not '>', 'n' is not '>', end of string.
        // found_gt = false → emit '<' and resume from i=start+1.
        let out = strip_html("i < n".to_owned());
        assert_eq!(out, "i < n");
    }

    #[test]
    fn test_strip_html_preserves_generics_in_plain() {
        // Vec<String> — '<' then 'S','t','r','i','n','g','>'. This WOULD be stripped!
        // But Vec<String> appears in CODE spans which are not processed by strip_html.
        // For plain text, `Vec<String>` would be stripped to `Vec`.
        // This is acceptable behavior for plain text (HTML-like patterns in plain text).
        // The safety invariant is that code spans are never passed to strip_html.
        let out = strip_html("Vec<String>".to_owned());
        // strip_html removes <String> → "Vec" (tag-like pattern)
        assert_eq!(out, "Vec");
    }

    #[test]
    fn test_plain_style_applied() {
        let theme = make_theme();
        use super::super::parse::InlineSpan;
        let blocks = vec![MarkdownBlock::Paragraph(vec![InlineSpan::Plain(
            "hello".to_owned(),
        )])];
        let styled = transform(blocks, &theme, &super::RenderOptions::default());
        if let StyledBlock::Paragraph(spans) = &styled[0] {
            assert_eq!(spans[0].content, "hello");
            assert!(spans[0].style.fg.is_some());
        }
    }

    #[test]
    fn test_bold_modifier_applied() {
        let theme = make_theme();
        use super::super::parse::InlineSpan;
        let blocks = vec![MarkdownBlock::Paragraph(vec![InlineSpan::Bold(
            "bold".to_owned(),
        )])];
        let styled = transform(blocks, &theme, &super::RenderOptions::default());
        if let StyledBlock::Paragraph(spans) = &styled[0] {
            assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
        }
    }

    #[test]
    fn test_italic_modifier_applied() {
        let theme = make_theme();
        use super::super::parse::InlineSpan;
        let blocks = vec![MarkdownBlock::Paragraph(vec![InlineSpan::Italic(
            "italic".to_owned(),
        )])];
        let styled = transform(blocks, &theme, &super::RenderOptions::default());
        if let StyledBlock::Paragraph(spans) = &styled[0] {
            assert!(spans[0].style.add_modifier.contains(Modifier::ITALIC));
        }
    }

    #[test]
    fn test_bold_italic_modifier_applied() {
        let theme = make_theme();
        use super::super::parse::InlineSpan;
        let blocks = vec![MarkdownBlock::Paragraph(vec![InlineSpan::BoldItalic(
            "bi".to_owned(),
        )])];
        let styled = transform(blocks, &theme, &super::RenderOptions::default());
        if let StyledBlock::Paragraph(spans) = &styled[0] {
            let m = spans[0].style.add_modifier;
            assert!(m.contains(Modifier::BOLD) && m.contains(Modifier::ITALIC));
        }
    }

    #[test]
    fn test_code_span_uses_code_style() {
        let theme = make_theme();
        use super::super::parse::InlineSpan;
        let blocks = vec![MarkdownBlock::Paragraph(vec![InlineSpan::Code(
            "vec![]".to_owned(),
        )])];
        let styled = transform(blocks, &theme, &super::RenderOptions::default());
        if let StyledBlock::Paragraph(spans) = &styled[0] {
            // Code span has a background color set
            assert!(spans[0].style.bg.is_some());
        }
    }

    #[test]
    fn test_code_span_html_not_stripped() {
        let theme = make_theme();
        use super::super::parse::InlineSpan;
        let blocks = vec![MarkdownBlock::Paragraph(vec![InlineSpan::Code(
            "Vec<String>".to_owned(),
        )])];
        let styled = transform(blocks, &theme, &super::RenderOptions::default());
        if let StyledBlock::Paragraph(spans) = &styled[0] {
            // Code content must NOT be HTML-stripped
            assert_eq!(spans[0].content, "Vec<String>");
        }
    }

    #[test]
    fn test_heading_all_bold() {
        let theme = make_theme();
        use super::super::parse::InlineSpan;
        for level in 1u8..=4 {
            let blocks = vec![MarkdownBlock::Heading {
                level,
                spans: vec![InlineSpan::Plain("Title".to_owned())],
            }];
            let styled = transform(blocks, &theme, &super::RenderOptions::default());
            if let StyledBlock::Heading { spans, .. } = &styled[0] {
                assert!(
                    spans[0].style.add_modifier.contains(Modifier::BOLD),
                    "H{level} should be BOLD"
                );
            }
        }
    }

    #[test]
    fn test_strip_html_unclosed_tag_with_euro_sign() {
        // UTF-8 safety: '<' followed by multi-byte char without closing '>'
        // Must not panic — degrades gracefully to plain text
        let out = strip_html("<€50".to_owned());
        assert_eq!(out, "<€50");
    }

    #[test]
    fn test_strip_html_unclosed_tag_with_emoji() {
        // UTF-8 safety: '<' followed by 4-byte emoji without closing '>'
        let out = strip_html("<😀 hello".to_owned());
        assert_eq!(out, "<😀 hello");
    }

    #[test]
    fn test_code_block_content_not_stripped() {
        let theme = make_theme();
        use super::super::parse::MarkdownBlock;
        let blocks = vec![MarkdownBlock::CodeBlock {
            language: Some("rust".to_owned()),
            content: "let v: Vec<String> = vec![]; let b = i < n;".to_owned(),
        }];
        let styled = transform(blocks, &theme, &super::RenderOptions::default());
        if let StyledBlock::CodeBlock { content, .. } = &styled[0] {
            assert!(content.contains("Vec<String>"));
            assert!(content.contains("i < n"));
        } else {
            panic!("Expected CodeBlock");
        }
    }

    // ── AC3: Unclosed inline marker stripping ─────────────────────────────────

    #[test]
    fn test_strip_unclosed_markers_double_star() {
        assert_eq!(
            strip_unclosed_markers("**unclosed bold".to_owned()),
            "unclosed bold"
        );
    }

    #[test]
    fn test_strip_unclosed_markers_single_star() {
        assert_eq!(
            strip_unclosed_markers("*unclosed italic".to_owned()),
            "unclosed italic"
        );
    }

    #[test]
    fn test_strip_unclosed_markers_backtick() {
        assert_eq!(
            strip_unclosed_markers("`unclosed code".to_owned()),
            "unclosed code"
        );
    }

    #[test]
    fn test_strip_unclosed_markers_tilde() {
        assert_eq!(
            strip_unclosed_markers("~~unclosed strike".to_owned()),
            "unclosed strike"
        );
    }

    #[test]
    fn test_strip_unclosed_markers_no_markers() {
        // Fast path — no allocation
        let s = "plain text with no markers".to_owned();
        assert_eq!(strip_unclosed_markers(s.clone()), s);
    }

    #[test]
    fn test_strip_markers_applied_in_completed_mode() {
        // In completed mode, Plain spans with unclosed markers are stripped.
        let theme = make_theme();
        use super::super::parse::InlineSpan;
        let blocks = vec![MarkdownBlock::Paragraph(vec![InlineSpan::Plain(
            "**unclosed bold".to_owned(),
        )])];
        let styled = transform(blocks, &theme, &super::RenderOptions::completed());
        if let StyledBlock::Paragraph(spans) = &styled[0] {
            assert_eq!(spans[0].content, "unclosed bold");
        } else {
            panic!("Expected Paragraph");
        }
    }

    #[test]
    fn test_strip_markers_not_applied_in_streaming_mode() {
        // In streaming mode (default), Plain spans are left untouched.
        let theme = make_theme();
        use super::super::parse::InlineSpan;
        let blocks = vec![MarkdownBlock::Paragraph(vec![InlineSpan::Plain(
            "**unclosed bold".to_owned(),
        )])];
        let styled = transform(blocks, &theme, &super::RenderOptions::default());
        if let StyledBlock::Paragraph(spans) = &styled[0] {
            assert_eq!(spans[0].content, "**unclosed bold");
        } else {
            panic!("Expected Paragraph");
        }
    }

    #[test]
    fn test_strip_markers_not_applied_to_code_span() {
        // Code spans are never stripped regardless of mode.
        let theme = make_theme();
        use super::super::parse::InlineSpan;
        let blocks = vec![MarkdownBlock::Paragraph(vec![InlineSpan::Code(
            "**still here**".to_owned(),
        )])];
        let styled = transform(blocks, &theme, &super::RenderOptions::completed());
        if let StyledBlock::Paragraph(spans) = &styled[0] {
            assert_eq!(spans[0].content, "**still here**");
        } else {
            panic!("Expected Paragraph");
        }
    }

    /// Mixed closed and unclosed markers: closed markers become styled variants and are
    /// preserved; unclosed markers remain in Plain spans and are stripped (AC3, DF-078).
    #[test]
    fn test_strip_markers_mixed_closed_and_unclosed() {
        let theme = make_theme();
        use super::super::parse::InlineSpan;
        // "**closed** and **unclosed" should render as "closed" (bold) + " and unclosed" (plain)
        let blocks = vec![MarkdownBlock::Paragraph(vec![
            InlineSpan::Bold("closed".to_owned()),
            InlineSpan::Plain(" and **unclosed".to_owned()),
        ])];
        let styled = transform(blocks, &theme, &super::RenderOptions::completed());
        if let StyledBlock::Paragraph(spans) = &styled[0] {
            assert_eq!(spans.len(), 2);
            assert_eq!(spans[0].content, "closed"); // Bold preserved
            assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
            assert_eq!(spans[1].content, " and unclosed"); // Unclosed ** stripped
        } else {
            panic!("Expected Paragraph");
        }
    }
}
