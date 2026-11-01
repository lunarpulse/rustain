//! Stage 2: parse sanitized markdown into an owned block tree.
//!
//! The tree is built **iteratively** from the event stream with an explicit
//! container stack (AD-2113, D13): no recursion per event, and containers
//! nested deeper than [`MAX_CONTAINER_DEPTH`] are merged into the deepest one,
//! so neither the builder nor the later recursive stages nor `Drop` can be
//! driven off the stack by adversarial input.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// Container frames (quote, list, item) the builder opens before it stops
/// nesting and flattens the rest into the deepest open container.
pub const MAX_CONTAINER_DEPTH: usize = 64;

/// Block nodes of the intermediate tree (adapter-local, AD-2113).
#[derive(Debug, PartialEq)]
pub enum Block {
    Paragraph(Vec<Inline>),
    Heading {
        level: u8,
        inlines: Vec<Inline>,
    },
    List(List),
    Quote(Vec<Block>),
    Code {
        language: Option<String>,
        content: String,
    },
    /// An HTML block: shown as literal text, source lines kept.
    Html(String),
    Rule,
}

/// An ordered or unordered list. `tight` is true when no item starts a
/// paragraph (pulldown-cmark exposes no tightness flag).
#[derive(Debug, PartialEq)]
pub struct List {
    pub start: Option<u64>,
    pub tight: bool,
    pub items: Vec<Item>,
}

/// One list item: an optional task state and its child blocks.
#[derive(Debug, PartialEq)]
pub struct Item {
    pub task: Option<bool>,
    pub blocks: Vec<Block>,
}

/// Inline formatting active on a text run.
#[derive(Debug, PartialEq, Clone, Copy, Default)]
pub struct Fmt {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
    pub link: bool,
    /// The ` (url)` shown after a link label (Explicit presentation).
    pub destination: bool,
}

/// Inline nodes of a paragraph or heading.
#[derive(Debug, PartialEq, Clone)]
pub enum Inline {
    Text { text: String, fmt: Fmt },
    SoftBreak,
    HardBreak,
}

enum Frame {
    Root(Vec<Block>),
    Quote(Vec<Block>),
    List {
        start: Option<u64>,
        tight: bool,
        items: Vec<Item>,
    },
    Item {
        task: Option<bool>,
        blocks: Vec<Block>,
    },
}

#[derive(Default)]
struct InlineState {
    bold: u32,
    italic: u32,
    strike: u32,
    link: u32,
}

struct Builder {
    stack: Vec<Frame>,
    /// Container starts ignored because the stack is at its depth cap.
    overflow: usize,
    inlines: Vec<Inline>,
    state: InlineState,
    heading: Option<u8>,
    code: Option<(Option<String>, String)>,
    html: Option<String>,
    /// Open link: destination and the label text seen so far.
    link: Option<(String, String)>,
    /// Alt text of the open image.
    image: Option<String>,
}

/// Parse sanitized markdown into a block tree.
///
/// Tables are not enabled here (they arrive with their producer-side escaping,
/// slice 12-B); a `|` row is an ordinary paragraph.
#[must_use]
pub fn parse(input: &str) -> Vec<Block> {
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut builder = Builder {
        stack: vec![Frame::Root(Vec::new())],
        overflow: 0,
        inlines: Vec::new(),
        state: InlineState::default(),
        heading: None,
        code: None,
        html: None,
        link: None,
        image: None,
    };
    for event in Parser::new_ext(input, options) {
        builder.event(event);
    }
    builder.finish()
}

impl Builder {
    fn blocks_mut(&mut self) -> &mut Vec<Block> {
        for frame in self.stack.iter_mut().rev() {
            match frame {
                Frame::Root(blocks) | Frame::Quote(blocks) | Frame::Item { blocks, .. } => {
                    return blocks;
                }
                Frame::List { .. } => {}
            }
        }
        unreachable!("the root frame is never popped")
    }

    fn push_block(&mut self, block: Block) {
        self.blocks_mut().push(block);
    }

    /// Text that arrived directly inside a tight list item has no paragraph
    /// events; it becomes a paragraph the moment a block-level event follows.
    fn flush_implicit(&mut self) {
        if self.heading.is_none() && !self.inlines.is_empty() {
            let inlines = std::mem::take(&mut self.inlines);
            self.push_block(Block::Paragraph(inlines));
        }
    }

    fn can_nest(&self) -> bool {
        self.stack.len() < MAX_CONTAINER_DEPTH
    }

    fn fmt(&self) -> Fmt {
        Fmt {
            bold: self.state.bold > 0,
            italic: self.state.italic > 0,
            strike: self.state.strike > 0,
            code: false,
            link: self.state.link > 0,
            destination: false,
        }
    }

    fn push_inline(&mut self, text: &str, fmt: Fmt) {
        if text.is_empty() {
            return;
        }
        if let Some(Inline::Text {
            text: last,
            fmt: last_fmt,
        }) = self.inlines.last_mut()
            && *last_fmt == fmt
        {
            last.push_str(text);
            return;
        }
        self.inlines.push(Inline::Text {
            text: text.to_owned(),
            fmt,
        });
    }

    /// Route text to the open image (alt text), the open link label and the
    /// inline run.
    fn text(&mut self, text: &str, code: bool) {
        if let Some(alt) = &mut self.image {
            alt.push_str(text);
            return;
        }
        if let Some((_, label)) = &mut self.link {
            label.push_str(text);
        }
        let mut fmt = self.fmt();
        fmt.code = code;
        self.push_inline(text, fmt);
    }

    fn event(&mut self, event: Event<'_>) {
        if let Some((_, content)) = &mut self.code {
            match event {
                Event::Text(text) => content.push_str(&text),
                Event::End(TagEnd::CodeBlock) => {
                    let (language, content) = self.code.take().unwrap_or_default();
                    self.push_block(Block::Code {
                        language,
                        content: content.trim_end_matches('\n').to_owned(),
                    });
                }
                _ => {}
            }
            return;
        }
        if let Some(source) = &mut self.html {
            match event {
                Event::Html(text) | Event::Text(text) => source.push_str(&text),
                Event::End(TagEnd::HtmlBlock) => {
                    let source = self.html.take().unwrap_or_default();
                    self.push_block(Block::Html(source.trim_end_matches('\n').to_owned()));
                }
                _ => {}
            }
            return;
        }
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text, false),
            Event::Code(code) => self.text(&code, true),
            // HTML is literal text everywhere (D2): never stripped.
            Event::Html(text) | Event::InlineHtml(text) => self.text(&text, false),
            Event::SoftBreak => {
                if let Some((_, label)) = &mut self.link {
                    label.push(' ');
                }
                if self.image.is_none() {
                    self.inlines.push(Inline::SoftBreak);
                }
            }
            Event::HardBreak => {
                if self.image.is_none() {
                    self.inlines.push(Inline::HardBreak);
                }
            }
            Event::Rule => {
                self.flush_implicit();
                self.push_block(Block::Rule);
            }
            Event::TaskListMarker(done) => {
                if let Some(Frame::Item { task, .. }) = self.stack.last_mut() {
                    *task = Some(done);
                }
            }
            // Math, footnotes and metadata are not enabled in `parse`.
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                self.flush_implicit();
                // A paragraph inside an item makes the enclosing list loose.
                let depth = self.stack.len();
                if depth >= 2
                    && matches!(self.stack[depth - 1], Frame::Item { .. })
                    && let Frame::List { tight, .. } = &mut self.stack[depth - 2]
                {
                    *tight = false;
                }
            }
            Tag::Heading { level, .. } => {
                self.flush_implicit();
                self.heading = Some(match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    HeadingLevel::H3 => 3,
                    HeadingLevel::H4 => 4,
                    HeadingLevel::H5 => 5,
                    HeadingLevel::H6 => 6,
                });
            }
            Tag::BlockQuote(_) => {
                self.flush_implicit();
                if self.can_nest() {
                    self.stack.push(Frame::Quote(Vec::new()));
                } else {
                    self.overflow += 1;
                }
            }
            Tag::List(start) => {
                self.flush_implicit();
                if self.can_nest() {
                    self.stack.push(Frame::List {
                        start,
                        tight: true,
                        items: Vec::new(),
                    });
                } else {
                    self.overflow += 1;
                }
            }
            Tag::Item => {
                if self.can_nest() && matches!(self.stack.last(), Some(Frame::List { .. })) {
                    self.stack.push(Frame::Item {
                        task: None,
                        blocks: Vec::new(),
                    });
                } else {
                    self.overflow += 1;
                }
            }
            Tag::CodeBlock(kind) => {
                self.flush_implicit();
                let language = match kind {
                    CodeBlockKind::Fenced(info) if !info.is_empty() => Some(info.to_string()),
                    _ => None,
                };
                self.code = Some((language, String::new()));
            }
            Tag::HtmlBlock => {
                self.flush_implicit();
                self.html = Some(String::new());
            }
            Tag::Emphasis => self.state.italic += 1,
            Tag::Strong => self.state.bold += 1,
            Tag::Strikethrough => self.state.strike += 1,
            Tag::Link { dest_url, .. } => {
                self.state.link += 1;
                self.link = Some((dest_url.to_string(), String::new()));
            }
            Tag::Image { .. } => self.image = Some(String::new()),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                let inlines = std::mem::take(&mut self.inlines);
                if !inlines.is_empty() {
                    self.push_block(Block::Paragraph(inlines));
                }
            }
            TagEnd::Heading(_) => {
                let level = self.heading.take().unwrap_or(1);
                let inlines = std::mem::take(&mut self.inlines);
                self.push_block(Block::Heading { level, inlines });
            }
            TagEnd::BlockQuote(_) => {
                self.flush_implicit();
                if self.overflow > 0 {
                    self.overflow -= 1;
                } else if let Some(Frame::Quote(blocks)) = self.stack.pop() {
                    self.push_block(Block::Quote(blocks));
                }
            }
            TagEnd::List(_) => {
                if self.overflow > 0 {
                    self.overflow -= 1;
                } else if let Some(Frame::List {
                    start,
                    tight,
                    items,
                }) = self.stack.pop()
                {
                    self.push_block(Block::List(List {
                        start,
                        tight,
                        items,
                    }));
                }
            }
            TagEnd::Item => {
                self.flush_implicit();
                if self.overflow > 0 {
                    self.overflow -= 1;
                } else if let Some(Frame::Item { task, blocks }) = self.stack.pop()
                    && let Some(Frame::List { items, .. }) = self.stack.last_mut()
                {
                    items.push(Item { task, blocks });
                }
            }
            TagEnd::Emphasis => self.state.italic = self.state.italic.saturating_sub(1),
            TagEnd::Strong => self.state.bold = self.state.bold.saturating_sub(1),
            TagEnd::Strikethrough => self.state.strike = self.state.strike.saturating_sub(1),
            TagEnd::Link => {
                self.state.link = self.state.link.saturating_sub(1);
                if let Some((url, label)) = self.link.take() {
                    self.link_destination(&url, &label);
                }
            }
            TagEnd::Image => {
                if let Some(alt) = self.image.take() {
                    let shown = if alt.trim().is_empty() {
                        "[image]".to_owned()
                    } else {
                        format!("[image: {}]", alt.trim())
                    };
                    self.text(&shown, false);
                }
            }
            _ => {}
        }
    }

    /// Explicit link presentation: `label (url)`, the URL once when the label
    /// already is the URL (autolinks).
    fn link_destination(&mut self, url: &str, label: &str) {
        let bare = url.strip_prefix("mailto:").unwrap_or(url);
        if label == url || label == bare || url.is_empty() {
            return;
        }
        let mut fmt = self.fmt();
        fmt.destination = true;
        fmt.link = false;
        self.push_inline(&format!(" ({url})"), fmt);
    }

    fn finish(mut self) -> Vec<Block> {
        self.flush_implicit();
        if self.heading.is_some() || !self.inlines.is_empty() {
            let inlines = std::mem::take(&mut self.inlines);
            self.push_block(Block::Paragraph(inlines));
        }
        match self.stack.into_iter().next() {
            Some(Frame::Root(blocks)) => blocks,
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Inline {
        Inline::Text {
            text: s.to_owned(),
            fmt: Fmt::default(),
        }
    }

    fn bold(s: &str) -> Inline {
        Inline::Text {
            text: s.to_owned(),
            fmt: Fmt {
                bold: true,
                ..Fmt::default()
            },
        }
    }

    fn list(block: &Block) -> &List {
        match block {
            Block::List(list) => list,
            other => panic!("expected a list, got {other:?}"),
        }
    }

    fn only_paragraph(blocks: &[Block]) -> &[Inline] {
        match blocks {
            [Block::Paragraph(inlines)] => inlines,
            other => panic!("expected one paragraph, got {other:?}"),
        }
    }

    #[test]
    fn test_paragraph() {
        assert_eq!(parse("hello"), [Block::Paragraph(vec![text("hello")])]);
    }

    #[test]
    fn test_heading_levels() {
        for (md, level) in [("# a", 1), ("## a", 2), ("### a", 3), ("###### a", 6)] {
            assert_eq!(
                parse(md),
                [Block::Heading {
                    level,
                    inlines: vec![text("a")]
                }]
            );
        }
    }

    #[test]
    fn test_bold_italic_strike_and_code_runs() {
        let blocks = parse("a **b** *c* ~~d~~ `e`");
        let [Block::Paragraph(inlines)] = blocks.as_slice() else {
            panic!("{blocks:?}")
        };
        let fmt = |f: fn(&mut Fmt)| {
            let mut fmt = Fmt::default();
            f(&mut fmt);
            fmt
        };
        let runs: Vec<(&str, Fmt)> = inlines
            .iter()
            .filter_map(|i| match i {
                Inline::Text { text, fmt } => Some((text.as_str(), *fmt)),
                _ => None,
            })
            .collect();
        assert_eq!(runs[1], ("b", fmt(|f| f.bold = true)));
        assert_eq!(runs[3], ("c", fmt(|f| f.italic = true)));
        assert_eq!(runs[5], ("d", fmt(|f| f.strike = true)));
        assert_eq!(runs[7], ("e", fmt(|f| f.code = true)));
    }

    #[test]
    fn test_soft_break_is_a_soft_break_not_a_newline_span() {
        // Contract change (B7): CommonMark soft breaks render as spaces.
        assert_eq!(
            parse("one\ntwo"),
            [Block::Paragraph(vec![
                text("one"),
                Inline::SoftBreak,
                text("two")
            ])]
        );
        assert_eq!(
            parse("one  \ntwo"),
            [Block::Paragraph(vec![
                text("one"),
                Inline::HardBreak,
                text("two")
            ])]
        );
    }

    #[test]
    fn test_nested_tight_list_keeps_parent_text_and_order() {
        // Baseline B1: parent text was merged into the child, depth capped at 1.
        let blocks = parse("- a\n  - b\n    - c\n      - d\n- e");
        let top = list(&blocks[0]);
        assert!(top.tight);
        assert_eq!(top.items.len(), 2);
        assert_eq!(
            top.items[0].blocks[0],
            Block::Paragraph(vec![text("a")]),
            "parent text stays the parent's first block"
        );
        let mut depth = 1;
        let mut current = list(&top.items[0].blocks[1]);
        while let Some(Block::List(next)) = current.items[0].blocks.get(1) {
            depth += 1;
            current = next;
        }
        assert_eq!(depth, 3);
        assert_eq!(
            current.items[0].blocks[0],
            Block::Paragraph(vec![text("d")])
        );
        assert_eq!(top.items[1].blocks[0], Block::Paragraph(vec![text("e")]));
    }

    #[test]
    fn test_loose_list_keeps_paragraphs_inside_items() {
        // Baseline B2: the paragraphs left the list.
        let blocks = parse("- a\n\n- b");
        assert_eq!(blocks.len(), 1);
        let l = list(&blocks[0]);
        assert!(!l.tight);
        assert_eq!(l.items.len(), 2);
        assert_eq!(only_paragraph(&l.items[1].blocks), [text("b")]);
    }

    #[test]
    fn test_ordered_list_keeps_its_start_number() {
        let blocks = parse("3. x\n4. y");
        assert_eq!(list(&blocks[0]).start, Some(3));
        assert_eq!(list(&parse("- x")[0]).start, None);
    }

    #[test]
    fn test_task_markers() {
        let blocks = parse("- [ ] a\n- [x] b\n- c");
        let l = list(&blocks[0]);
        assert_eq!(
            l.items.iter().map(|i| i.task).collect::<Vec<_>>(),
            [Some(false), Some(true), None]
        );
    }

    #[test]
    fn test_list_item_holding_code_and_quote() {
        let blocks = parse("- a\n  ```rust\n  x\n  ```\n  > q");
        let item = &list(&blocks[0]).items[0];
        assert!(matches!(item.blocks[0], Block::Paragraph(_)));
        assert!(matches!(item.blocks[1], Block::Code { .. }));
        assert!(matches!(item.blocks[2], Block::Quote(_)));
    }

    #[test]
    fn test_code_block_language_and_content() {
        assert_eq!(
            parse("```rust\nfn main() {}\n```"),
            [Block::Code {
                language: Some("rust".to_owned()),
                content: "fn main() {}".to_owned()
            }]
        );
        assert_eq!(
            parse("    indented"),
            [Block::Code {
                language: None,
                content: "indented".to_owned()
            }]
        );
    }

    #[test]
    fn test_unclosed_fence_runs_to_the_end() {
        assert_eq!(
            parse("~~~\nabc\ndef"),
            [Block::Code {
                language: None,
                content: "abc\ndef".to_owned()
            }]
        );
    }

    #[test]
    fn test_thematic_break_and_quote_nesting() {
        assert_eq!(parse("---"), [Block::Rule]);
        let blocks = parse("> a\n> > b");
        let Block::Quote(outer) = &blocks[0] else {
            panic!("{blocks:?}")
        };
        assert!(matches!(outer[1], Block::Quote(_)));
    }

    #[test]
    fn test_html_is_literal_text_never_dropped() {
        // DF-081 / B4 / B5: `<String>` is an InlineHtml event.
        assert_eq!(
            parse("Vec<String> and `Vec<T>`"),
            [Block::Paragraph(vec![
                text("Vec<String> and "),
                Inline::Text {
                    text: "Vec<T>".to_owned(),
                    fmt: Fmt {
                        code: true,
                        ..Fmt::default()
                    }
                }
            ])]
        );
        assert_eq!(
            parse("<div>\n  x\n</div>"),
            [Block::Html("<div>\n  x\n</div>".to_owned())]
        );
    }

    #[test]
    fn test_links_and_images_stay_visible() {
        let blocks = parse("[lbl](https://e.test/p) ![alt](u.png) <https://auto.test/>");
        let [Block::Paragraph(inlines)] = blocks.as_slice() else {
            panic!("{blocks:?}")
        };
        let texts: String = inlines
            .iter()
            .filter_map(|i| match i {
                Inline::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts,
            "lbl (https://e.test/p) [image: alt] https://auto.test/"
        );
    }

    #[test]
    fn test_bold_in_a_tight_item_is_one_paragraph() {
        let blocks = parse("- **x** y");
        let item = &list(&blocks[0]).items[0];
        assert_eq!(only_paragraph(&item.blocks), [bold("x"), text(" y")]);
    }

    #[test]
    fn test_container_depth_is_bounded_and_the_content_survives() {
        let source = format!("{}x", "> ".repeat(10_000));
        let blocks = parse(&source);
        let mut depth = 0usize;
        let mut current = &blocks;
        let mut leaf = None;
        while let Some(Block::Quote(inner)) = current.first() {
            depth += 1;
            current = inner;
            leaf = Some(inner);
        }
        assert!(depth <= MAX_CONTAINER_DEPTH, "depth {depth}");
        assert!(leaf.is_some_and(|blocks| {
            blocks
                .iter()
                .any(|b| matches!(b, Block::Paragraph(i) if i == &[text("x")]))
        }));
        // A 1,500-deep list parses without recursion and keeps its first item.
        let deep: String = (0..1_500)
            .map(|level| format!("{}- item{level}\n", "  ".repeat(level)))
            .collect();
        let parsed = parse(&deep);
        let first = list(&parsed[0]);
        assert_eq!(
            first.items[0].blocks[0],
            Block::Paragraph(vec![text("item0")])
        );
    }
}
