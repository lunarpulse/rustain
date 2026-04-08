use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

/// Intermediate markdown block types produced by Stage 2.
#[derive(Debug, PartialEq)]
pub enum MarkdownBlock {
    Paragraph(Vec<InlineSpan>),
    Heading {
        level: u8,
        spans: Vec<InlineSpan>,
    },
    /// `ordered`: true for numbered lists.
    List {
        ordered: bool,
        items: Vec<ListItem>,
    },
    CodeBlock {
        language: Option<String>,
        content: String,
    },
    ThematicBreak,
}

/// A single list item with its nesting depth (capped at 1 for Story 3-6).
#[derive(Debug, PartialEq)]
pub struct ListItem {
    pub spans: Vec<InlineSpan>,
    pub depth: usize,
}

/// Inline content spans.
#[derive(Debug, PartialEq, Clone)]
pub enum InlineSpan {
    Plain(String),
    Bold(String),
    Italic(String),
    BoldItalic(String),
    Code(String),
}

/// Parse sanitized markdown into a `Vec<MarkdownBlock>`.
///
/// Uses a flat event iterator with a context stack to track nesting.
pub fn parse(input: &str) -> Vec<MarkdownBlock> {
    let options = Options::ENABLE_STRIKETHROUGH;
    let parser = Parser::new_ext(input, options);

    let mut blocks: Vec<MarkdownBlock> = Vec::new();

    // Inline accumulation state
    let mut inline_buf: Vec<InlineSpan> = Vec::new();
    let mut text_buf = String::new();
    let mut bold = false;
    let mut italic = false;

    // Block context stack
    #[derive(Debug, Clone, PartialEq)]
    enum BlockCtx {
        Paragraph,
        Heading(u8),
        ListItem { depth: usize },
        CodeBlock { language: Option<String> },
    }

    let mut ctx_stack: Vec<BlockCtx> = Vec::new();
    // (ordered, accumulated items) per nesting level
    let mut list_stack: Vec<(bool, Vec<ListItem>)> = Vec::new();

    macro_rules! flush_text {
        () => {
            if !text_buf.is_empty() {
                let s = std::mem::take(&mut text_buf);
                let span = match (bold, italic) {
                    (true, true) => InlineSpan::BoldItalic(s),
                    (true, false) => InlineSpan::Bold(s),
                    (false, true) => InlineSpan::Italic(s),
                    (false, false) => InlineSpan::Plain(s),
                };
                inline_buf.push(span);
            }
        };
    }

    for event in parser {
        // Inside a code block: only collect text, wait for End(CodeBlock)
        let in_code_block = matches!(ctx_stack.last(), Some(BlockCtx::CodeBlock { .. }));
        if in_code_block {
            match event {
                Event::Text(t) => text_buf.push_str(&t),
                Event::End(TagEnd::CodeBlock) => {
                    let raw = std::mem::take(&mut text_buf);
                    // pulldown-cmark appends a trailing newline; strip it
                    let content = raw.trim_end_matches('\n').to_owned();
                    let language = if let Some(BlockCtx::CodeBlock { language }) = ctx_stack.pop()
                    {
                        language
                    } else {
                        None
                    };
                    blocks.push(MarkdownBlock::CodeBlock { language, content });
                }
                _ => {}
            }
            continue;
        }

        match event {
            // ── Block starts ─────────────────────────────────────────────────
            Event::Start(Tag::Paragraph) => {
                ctx_stack.push(BlockCtx::Paragraph);
            }
            Event::Start(Tag::Heading { level, .. }) => {
                use pulldown_cmark::HeadingLevel;
                let lvl: u8 = match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    HeadingLevel::H3 => 3,
                    HeadingLevel::H4 => 4,
                    HeadingLevel::H5 => 5,
                    HeadingLevel::H6 => 6,
                };
                ctx_stack.push(BlockCtx::Heading(lvl));
            }
            Event::Start(Tag::List(first_idx)) => {
                let ordered = first_idx.is_some();
                list_stack.push((ordered, Vec::new()));
            }
            Event::Start(Tag::Item) => {
                // Depth = number of list levels above this one, capped at 1
                let depth = list_stack.len().saturating_sub(1).min(1);
                ctx_stack.push(BlockCtx::ListItem { depth });
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                let language = match kind {
                    pulldown_cmark::CodeBlockKind::Fenced(lang) => {
                        let s = lang.to_string();
                        if s.is_empty() { None } else { Some(s) }
                    }
                    pulldown_cmark::CodeBlockKind::Indented => None,
                };
                ctx_stack.push(BlockCtx::CodeBlock { language });
            }

            // ── Inline formatting toggles ─────────────────────────────────────
            Event::Start(Tag::Strong) => {
                flush_text!();
                bold = true;
            }
            Event::End(TagEnd::Strong) => {
                flush_text!();
                bold = false;
            }
            Event::Start(Tag::Emphasis) => {
                flush_text!();
                italic = true;
            }
            Event::End(TagEnd::Emphasis) => {
                flush_text!();
                italic = false;
            }

            // ── Inline content ────────────────────────────────────────────────
            Event::Text(text) => {
                text_buf.push_str(&text);
            }
            Event::Code(code) => {
                flush_text!();
                inline_buf.push(InlineSpan::Code(code.to_string()));
            }
            Event::SoftBreak => {
                // Treat soft breaks as hard line breaks so that single `\n` in
                // user input (and copy-pasted markdown/YAML/HTML) preserves line
                // structure visually. CommonMark soft-break-as-space is correct
                // for flowing prose but wrong for a TUI chat input context.
                // Anthropic API responses use `\n\n` for paragraphs and rarely
                // emit single `\n`, so this has negligible impact on assistant
                // message rendering.
                // Covers: DF-SoftBreak (discovered Story 3-6a)
                flush_text!();
                inline_buf.push(InlineSpan::Plain("\n".to_string()));
            }
            Event::HardBreak => {
                flush_text!();
                inline_buf.push(InlineSpan::Plain("\n".to_string()));
            }

            // ── Block ends ────────────────────────────────────────────────────
            Event::End(TagEnd::Paragraph) => {
                flush_text!();
                if ctx_stack.pop().is_some() {
                    let spans = std::mem::take(&mut inline_buf);
                    blocks.push(MarkdownBlock::Paragraph(spans));
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                flush_text!();
                if let Some(BlockCtx::Heading(lvl)) = ctx_stack.pop() {
                    let spans = std::mem::take(&mut inline_buf);
                    blocks.push(MarkdownBlock::Heading { level: lvl, spans });
                }
            }
            Event::End(TagEnd::Item) => {
                flush_text!();
                if let Some(BlockCtx::ListItem { depth }) = ctx_stack.pop() {
                    let spans = std::mem::take(&mut inline_buf);
                    if let Some((_, items)) = list_stack.last_mut() {
                        items.push(ListItem { spans, depth });
                    }
                }
            }
            Event::End(TagEnd::List(_)) => {
                if let Some((ordered, items)) = list_stack.pop() {
                    blocks.push(MarkdownBlock::List { ordered, items });
                }
            }

            Event::Rule => {
                blocks.push(MarkdownBlock::ThematicBreak);
            }

            // Ignore HTML blocks, links, images, strikethrough wrappers, etc.
            _ => {}
        }
    }

    // Flush any remaining content at EOF (handles input ending mid-block)
    flush_text!();
    if !inline_buf.is_empty() {
        let spans = std::mem::take(&mut inline_buf);
        blocks.push(MarkdownBlock::Paragraph(spans));
    }

    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(s: &str) -> InlineSpan {
        InlineSpan::Plain(s.to_owned())
    }

    #[test]
    fn test_plain_paragraph() {
        let blocks = parse("Hello world");
        assert_eq!(blocks.len(), 1);
        if let MarkdownBlock::Paragraph(spans) = &blocks[0] {
            assert_eq!(spans[0], plain("Hello world"));
        } else {
            panic!("Expected Paragraph");
        }
    }

    #[test]
    fn test_heading_h1() {
        let blocks = parse("# Title\n");
        assert!(matches!(
            &blocks[0],
            MarkdownBlock::Heading { level: 1, .. }
        ));
        if let MarkdownBlock::Heading { spans, .. } = &blocks[0] {
            assert!(!spans.is_empty());
        }
    }

    #[test]
    fn test_heading_h2() {
        let blocks = parse("## Sub\n");
        assert!(matches!(
            &blocks[0],
            MarkdownBlock::Heading { level: 2, .. }
        ));
    }

    #[test]
    fn test_heading_h4() {
        let blocks = parse("#### H4\n");
        assert!(matches!(
            &blocks[0],
            MarkdownBlock::Heading { level: 4, .. }
        ));
    }

    #[test]
    fn test_bold_span() {
        let blocks = parse("**bold text**");
        if let MarkdownBlock::Paragraph(spans) = &blocks[0] {
            assert!(spans.iter().any(|s| matches!(s, InlineSpan::Bold(_))));
        } else {
            panic!("Expected Paragraph");
        }
    }

    #[test]
    fn test_italic_span() {
        let blocks = parse("*italic text*");
        if let MarkdownBlock::Paragraph(spans) = &blocks[0] {
            assert!(spans.iter().any(|s| matches!(s, InlineSpan::Italic(_))));
        } else {
            panic!("Expected Paragraph");
        }
    }

    #[test]
    fn test_bold_italic_span() {
        let blocks = parse("***bold italic***");
        if let MarkdownBlock::Paragraph(spans) = &blocks[0] {
            assert!(spans
                .iter()
                .any(|s| matches!(s, InlineSpan::BoldItalic(_))));
        } else {
            panic!("Expected Paragraph");
        }
    }

    #[test]
    fn test_code_span() {
        let blocks = parse("`code here`");
        if let MarkdownBlock::Paragraph(spans) = &blocks[0] {
            assert!(spans.iter().any(|s| matches!(s, InlineSpan::Code(_))));
        } else {
            panic!("Expected Paragraph");
        }
    }

    #[test]
    fn test_unordered_list() {
        let blocks = parse("- item1\n- item2\n");
        assert_eq!(blocks.len(), 1);
        if let MarkdownBlock::List { ordered, items } = &blocks[0] {
            assert!(!ordered);
            assert_eq!(items.len(), 2);
        } else {
            panic!("Expected List");
        }
    }

    #[test]
    fn test_ordered_list() {
        let blocks = parse("1. first\n2. second\n");
        if let MarkdownBlock::List { ordered, items } = &blocks[0] {
            assert!(ordered);
            assert_eq!(items.len(), 2);
        } else {
            panic!("Expected ordered List");
        }
    }

    #[test]
    fn test_code_block_with_language() {
        let blocks = parse("```rust\nfn main() {}\n```\n");
        if let MarkdownBlock::CodeBlock { language, content } = &blocks[0] {
            assert_eq!(language.as_deref(), Some("rust"));
            assert!(content.contains("fn main()"));
        } else {
            panic!("Expected CodeBlock");
        }
    }

    #[test]
    fn test_code_block_no_language() {
        let blocks = parse("```\nsome code\n```\n");
        if let MarkdownBlock::CodeBlock { language, .. } = &blocks[0] {
            assert!(language.is_none());
        } else {
            panic!("Expected CodeBlock");
        }
    }

    #[test]
    fn test_thematic_break() {
        let blocks = parse("---\n");
        assert!(blocks
            .iter()
            .any(|b| matches!(b, MarkdownBlock::ThematicBreak)));
    }

    #[test]
    fn test_nested_list_capped_at_depth_1() {
        // pulldown-cmark represents nested lists as separate List events inside Item events.
        // With the current flat approach, depth is captured per item at parse time.
        let input = "- top\n  - nested\n    - deep\n";
        let blocks = parse(input);
        for block in &blocks {
            if let MarkdownBlock::List { items, .. } = block {
                for item in items {
                    assert!(
                        item.depth <= 1,
                        "depth {} exceeds cap of 1",
                        item.depth
                    );
                }
            }
        }
    }

    #[test]
    fn test_mixed_inline_in_heading() {
        let blocks = parse("## Hello **bold** world\n");
        if let MarkdownBlock::Heading { level, spans } = &blocks[0] {
            assert_eq!(*level, 2);
            assert!(spans.iter().any(|s| matches!(s, InlineSpan::Bold(_))));
        }
    }

    #[test]
    fn test_eof_flush_preserves_trailing_content() {
        // Content that ends without a block-closing event should still be captured.
        // pulldown-cmark wraps bare text in Paragraph, but if the parser somehow
        // receives events without End(Paragraph), the EOF flush catches it.
        // Test via the public API: bare text without trailing newline.
        let blocks = parse("hello world");
        assert!(!blocks.is_empty(), "EOF flush should capture trailing content");
        if let MarkdownBlock::Paragraph(spans) = &blocks[0] {
            assert_eq!(spans[0], plain("hello world"));
        }
    }

    #[test]
    fn test_eof_flush_mid_list_content() {
        // A list followed by trailing text — ensure nothing is silently dropped
        let blocks = parse("- item\n\ntrailing text");
        assert!(blocks.len() >= 2, "Should have list and trailing paragraph");
        // Last block should contain "trailing text"
        let last = blocks.last().unwrap();
        match last {
            MarkdownBlock::Paragraph(spans) => {
                let text: String = spans.iter().map(|s| match s {
                    InlineSpan::Plain(t) => t.as_str(),
                    _ => "",
                }).collect();
                assert!(text.contains("trailing text"));
            }
            _ => panic!("Expected trailing Paragraph, got {:?}", last),
        }
    }

    #[test]
    fn test_malformed_markdown_no_panic() {
        // Various malformed inputs should never panic
        let cases = vec![
            "**unclosed bold",
            "*unclosed italic",
            "***triple unclosed",
            "- list without content\n  - ",
            "## ",
            "",
            "\n\n\n",
        ];
        for input in cases {
            let _ = parse(input); // Must not panic
        }
    }

    #[test]
    fn test_code_block_preserves_content() {
        // Rust generics must not be stripped
        let blocks = parse("```rust\nlet v: Vec<String> = vec![];\n```\n");
        if let MarkdownBlock::CodeBlock { content, .. } = &blocks[0] {
            assert!(content.contains("Vec<String>"));
        } else {
            panic!("Expected CodeBlock");
        }
    }

    // ── SoftBreak-as-hard-newline tests (DF-SoftBreak) ───────────────────────

    /// Single \n inside a paragraph parses to a \n InlineSpan, not a space.
    /// Covers: DF-SoftBreak
    #[test]
    fn test_soft_break_emits_newline_span() {
        let blocks = parse("line one\nline two");
        assert_eq!(blocks.len(), 1, "Expected 1 paragraph");
        if let MarkdownBlock::Paragraph(spans) = &blocks[0] {
            // Must contain a Plain("\n") span between the two lines
            let has_newline = spans
                .iter()
                .any(|s| matches!(s, InlineSpan::Plain(t) if t == "\n"));
            assert!(
                has_newline,
                "Expected InlineSpan::Plain(\"\\n\") for soft break, got: {spans:?}"
            );
        } else {
            panic!("Expected Paragraph, got: {blocks:?}");
        }
    }

    /// Multi-line input produces a \n span between every adjacent line.
    /// Covers: DF-SoftBreak (3-line case)
    #[test]
    fn test_soft_break_three_lines_all_newlines() {
        let blocks = parse("alpha\nbeta\ngamma");
        assert_eq!(blocks.len(), 1);
        if let MarkdownBlock::Paragraph(spans) = &blocks[0] {
            let newline_count = spans
                .iter()
                .filter(|s| matches!(s, InlineSpan::Plain(t) if t == "\n"))
                .count();
            assert_eq!(newline_count, 2, "Expected 2 newline spans for 3 lines, got: {spans:?}");
        } else {
            panic!("Expected Paragraph");
        }
    }

    /// Inline formatting (bold, code) survives across soft-break lines.
    /// Covers: DF-SoftBreak (markdown content preserved)
    #[test]
    fn test_soft_break_preserves_inline_formatting() {
        let blocks = parse("**bold**\nnormal line");
        assert_eq!(blocks.len(), 1);
        if let MarkdownBlock::Paragraph(spans) = &blocks[0] {
            let has_bold = spans.iter().any(|s| matches!(s, InlineSpan::Bold(_)));
            let has_newline = spans
                .iter()
                .any(|s| matches!(s, InlineSpan::Plain(t) if t == "\n"));
            assert!(has_bold, "Bold span should survive soft break: {spans:?}");
            assert!(has_newline, "Newline span should follow bold: {spans:?}");
        } else {
            panic!("Expected Paragraph");
        }
    }

    /// SoftBreak and HardBreak produce the same output — both emit Plain("\n").
    /// Covers: DF-SoftBreak (parity with HardBreak)
    #[test]
    fn test_soft_break_matches_hard_break_output() {
        // Single \n (soft break in CommonMark)
        let soft_blocks = parse("line one\nline two");
        // Two spaces + \n (hard break in CommonMark)
        let hard_blocks = parse("line one  \nline two");

        // Both should produce the same Paragraph structure
        assert_eq!(
            soft_blocks.len(),
            hard_blocks.len(),
            "Soft and hard break should produce same block count"
        );
        if let (
            MarkdownBlock::Paragraph(soft_spans),
            MarkdownBlock::Paragraph(hard_spans),
        ) = (&soft_blocks[0], &hard_blocks[0])
        {
            assert_eq!(
                soft_spans, hard_spans,
                "Soft break and hard break should produce identical spans"
            );
        }
    }

    /// Code blocks are unaffected — their content is not parsed for inline events.
    /// Covers: DF-SoftBreak (code block isolation)
    #[test]
    fn test_soft_break_does_not_affect_code_blocks() {
        let blocks = parse("```yaml\nkey: value\nnested:\n  - item\n```\n");
        assert_eq!(blocks.len(), 1);
        if let MarkdownBlock::CodeBlock { content, .. } = &blocks[0] {
            // Content must be verbatim — all newlines preserved as-is
            assert!(content.contains("key: value"), "YAML key missing: {content}");
            assert!(content.contains("nested:"), "YAML nested key missing: {content}");
            assert!(content.contains("  - item"), "YAML list item missing: {content}");
        } else {
            panic!("Expected CodeBlock, got: {blocks:?}");
        }
    }
}
