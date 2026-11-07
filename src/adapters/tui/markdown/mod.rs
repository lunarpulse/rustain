//! Markdown rendering pipeline for the chat pane.
//!
//! 5-stage pipeline: sanitize → parse → transform → highlight → layout
//!
//! # Usage
//! ```ignore
//! let opts = markdown::RenderOptions::completed(); // or ::streaming() / ::literal()
//! let lines = markdown::render(content, width, theme, &opts).lines;
//! let height = markdown::compute_height(content, width, theme, &opts);
//! ```
//!
//! `compute_height` runs the full pipeline and returns `lines.len()` — no
//! separate counting implementation, so height and render are always in sync
//! (AD-2113). Both calls must receive the **same** theme and `RenderOptions`
//! as the paint pass; there is no default, so every caller chooses its mode.
//!
//! # Modes
//! * [`RenderMode::Streaming`] — the open, still-growing reply: CommonMark plus
//!   display-only closers for the trailing paragraph (D3).
//! * [`RenderMode::Completed`] — CommonMark exactly: unmatched delimiters,
//!   escapes and HTML show as the literal text CommonMark resolves (D2).
//! * [`RenderMode::Literal`] — user-typed text: no markdown, source line
//!   breaks and whitespace kept (D12).

mod block;
pub mod highlight;
pub mod layout;
pub mod link;
pub mod parse;
pub mod sanitize;
pub mod table;
pub mod transform;

use ratatui::prelude::*;

pub use block::render_block;
pub use link::{LinkPresentation, LinkRun};

use crate::adapters::tui::theme::Theme;

/// How the source text is interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderMode {
    Streaming,
    Completed,
    Literal,
}
/// Geometry policy for inspectable blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockPresentation {
    Stage,
    Window,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockKind {
    Code {
        language: Option<String>,
        lines: usize,
    },
    Table {
        cols: usize,
        rows: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockSpan {
    pub ordinal: usize,
    pub kind: BlockKind,
    pub rows: std::ops::Range<usize>,
    pub cells: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockMode {
    Rendered,
    Source,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockRender {
    pub mode: BlockMode,
    pub wrap: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct BlockView {
    pub kind: BlockKind,
    pub rows: Vec<Line<'static>>,
    pub source_line: Vec<Option<usize>>,
    pub cells: usize,
    pub source: String,
}

/// Pipeline configuration. There is deliberately no `Default`: a caller that
/// forgets to choose a mode would silently get the wrong one. Links default to
/// [`LinkPresentation::Explicit`] (the full URL stays visible); the Stage opts
/// in to `Labeled` with [`RenderOptions::with_links`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderOptions {
    pub mode: RenderMode,
    pub links: LinkPresentation,
    pub blocks: BlockPresentation,
}

impl RenderOptions {
    /// The open streaming reply: formatting still open in the trailing
    /// paragraph is closed for display only.
    #[must_use]
    pub const fn streaming() -> Self {
        Self::new(RenderMode::Streaming)
    }

    /// A completed message: CommonMark exactly, never a speculative closer.
    #[must_use]
    pub const fn completed() -> Self {
        Self::new(RenderMode::Completed)
    }

    /// User-typed text shown as typed.
    #[must_use]
    pub const fn literal() -> Self {
        Self::new(RenderMode::Literal)
    }

    const fn new(mode: RenderMode) -> Self {
        Self {
            mode,
            links: LinkPresentation::Explicit,
            blocks: BlockPresentation::Stage,
        }
    }

    /// The same options with another link presentation.
    #[must_use]
    pub const fn with_links(mut self, links: LinkPresentation) -> Self {
        self.links = links;
        self
    }

    #[must_use]
    pub const fn with_blocks(mut self, blocks: BlockPresentation) -> Self {
        self.blocks = blocks;
        self
    }
}

/// The rendered lines of one markdown source, and the OSC 8 runs of its links.
#[derive(Debug, Clone)]
pub struct Rendered {
    pub lines: Vec<Line<'static>>,
    /// One run per physical line segment of a safe link label, in line order;
    /// empty unless the options asked for `Labeled { osc8: true }`. `line`
    /// indexes `lines`, `start_cell` is the first label cell of that line.
    pub links: Vec<LinkRun>,
    pub blocks: Vec<BlockSpan>,
}

/// Debug-only count of pipeline runs on the current thread, used to prove the
/// one-pass-per-draw rule (AC8) without a second counter in production.
#[cfg(any(test, debug_assertions))]
pub mod metrics {
    use std::cell::Cell;

    thread_local! {
        static RENDERS: Cell<usize> = const { Cell::new(0) };
    }

    pub(super) fn record_render() {
        RENDERS.with(|count| count.set(count.get() + 1));
    }

    /// Pipeline runs on this thread since the last [`reset`].
    pub fn renders() -> usize {
        RENDERS.with(Cell::get)
    }

    pub fn reset() {
        RENDERS.with(|count| count.set(0));
    }
}

/// Render `content` as markdown into ratatui `Line`s.
///
/// The pipeline is stateless — it always re-parses the full input, which is
/// safe for streaming (re-render the whole buffer each draw) and correct for
/// height computation.
#[must_use]
pub fn render(content: &str, width: usize, theme: &Theme, opts: &RenderOptions) -> Rendered {
    #[cfg(any(test, debug_assertions))]
    metrics::record_render();
    if opts.mode == RenderMode::Literal {
        return Rendered {
            lines: layout::literal(content, width, theme),
            links: Vec::new(),
            blocks: Vec::new(),
        };
    }
    let sanitized = sanitize::sanitize(content, opts.mode);
    let parsed = parse::parse(&sanitized, opts.links);
    let styled = transform::transform(parsed, theme);
    let highlighted = highlight::highlight(styled, theme);
    let rows = layout::layout_with_blocks(highlighted, width, theme, opts.blocks);
    Rendered {
        lines: rows.lines,
        links: rows.links,
        blocks: rows.blocks,
    }
}

/// Number of lines `render()` produces for `content` at `width`.
///
/// Runs the full pipeline — the same code path as `render()` — so the height
/// invariant needs no second implementation. Pass the **painting** theme and
/// the **same** `opts` as the matching `render()` call.
#[must_use]
pub fn compute_height(content: &str, width: usize, theme: &Theme, opts: &RenderOptions) -> usize {
    render(content, width, theme, opts).lines.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::terminal_info::ColorCapability;

    const MODES: [RenderOptions; 3] = [
        RenderOptions::streaming(),
        RenderOptions::completed(),
        RenderOptions::literal(),
    ];

    fn theme() -> Theme {
        Theme::dark()
    }

    fn lines(content: &str, width: usize, opts: RenderOptions) -> Vec<Line<'static>> {
        render(content, width, &theme(), &opts).lines
    }

    fn text_of(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn non_blank(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(text_of)
            .filter(|s| !s.trim().is_empty())
            .collect()
    }

    // ── Height invariant ──────────────────────────────────────────────────────

    fn assert_height_invariant(content: &str, width: usize) {
        for theme in [
            Theme::dark(),
            Theme::for_capability(ColorCapability::Monochrome),
        ] {
            for opts in MODES {
                let rendered = render(content, width, &theme, &opts);
                assert_eq!(
                    rendered.lines.len(),
                    compute_height(content, width, &theme, &opts),
                    "height invariant violated for {content:?} width={width} mode={:?}",
                    opts.mode
                );
            }
        }
    }

    #[test]
    fn test_height_invariant_every_element_mode_and_width() {
        let cases = [
            "Hello world",
            "# Title\n\nParagraph",
            "- item1\n- item2\n- item3\n",
            "- a\n  - b\n    - c\n      - d\n- e",
            "- a\n\n- b",
            "3. x\n4. y",
            "- [ ] t\n- [x] d",
            "> q\n> > r",
            "```rust\nfn main() {}\n```\n",
            "```\n\tindent\n```",
            "# Title\n\nParagraph with **bold** and `code`.\n\n- item 1\n- item 2\n\n```rust\nfn main() {}\n```\n",
            "",
            "A single line",
            "你好世界 Hello",
            "text\n\n---\n\nmore",
            "**unclosed bold",
            "*unclosed italic",
            "`unclosed code",
            "~~unclosed strike",
            "mixed **bold and *italic unclosed",
            "line one\nline two",
            "key: value\nnested:\n  - item",
            "<div>\n  x\n</div>",
            "Vec<String> and SELECT * FROM t",
            "[l](https://e.test) ![i](u)",
        ];
        for content in cases {
            for width in [0, 1, 2, 3, 5, 10, 40, 80, 120] {
                assert_height_invariant(content, width);
            }
        }
        assert_height_invariant(&"word ".repeat(50), 40);
    }

    #[test]
    fn test_compute_height_uses_the_given_mode() {
        // `**aaaa bbbb` is 11 cells completed but 9 once the streaming closer
        // hides the opener; a height pass with the wrong mode would differ.
        let t = theme();
        let source = "**aaaa bbbb";
        assert_eq!(
            compute_height(source, 10, &t, &RenderOptions::completed()),
            3
        );
        assert_eq!(
            compute_height(source, 10, &t, &RenderOptions::streaming()),
            2
        );
        assert_eq!(compute_height("a\nb", 10, &t, &RenderOptions::literal()), 3);
        assert_eq!(
            compute_height("a\nb", 10, &t, &RenderOptions::completed()),
            2
        );
    }

    // ── Render content ────────────────────────────────────────────────────────

    #[test]
    fn test_empty_returns_empty() {
        for opts in MODES {
            assert!(lines("", 80, opts).is_empty(), "{:?}", opts.mode);
        }
    }

    #[test]
    fn test_zero_width_returns_empty() {
        for opts in MODES {
            assert!(lines("Hello", 0, opts).is_empty(), "{:?}", opts.mode);
        }
    }

    #[test]
    fn test_plain_text_renders_with_a_trailing_blank_line() {
        let out = lines("Hello world", 80, RenderOptions::completed());
        assert_eq!(
            out.iter().map(text_of).collect::<Vec<_>>(),
            ["Hello world", ""]
        );
    }

    #[test]
    fn test_bold_and_code_span_styles() {
        let out = lines("**bold text** `code`", 80, RenderOptions::completed());
        assert!(out.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::BOLD))
        }));
        assert!(
            out.iter()
                .any(|l| l.spans.iter().any(|s| s.style.bg.is_some()))
        );
    }

    #[test]
    fn test_rust_generics_and_operators_are_preserved_in_code_blocks() {
        let all: String = lines(
            "```rust\nlet v: Vec<String> = vec![];\nlet b = i < n;\n```\n",
            80,
            RenderOptions::completed(),
        )
        .iter()
        .map(text_of)
        .collect();
        assert!(
            all.contains("Vec<String>") && all.contains("i < n"),
            "{all}"
        );
    }

    // ── Soft breaks (contract change B7) ──────────────────────────────────────

    #[test]
    fn test_single_newline_joins_in_markdown_and_splits_in_literal() {
        let md = lines("line one\nline two", 80, RenderOptions::completed());
        assert_eq!(non_blank(&md), ["line one line two"]);
        let literal = lines("line one\nline two", 80, RenderOptions::literal());
        assert_eq!(non_blank(&literal), ["line one", "line two"]);
    }

    #[test]
    fn test_multiline_user_input_keeps_every_line_in_literal_mode() {
        let out = lines("alpha\nbeta\ngamma", 80, RenderOptions::literal());
        assert_eq!(non_blank(&out), ["alpha", "beta", "gamma"]);
        let yaml = lines(
            "key: value\nnested:\n  - item",
            80,
            RenderOptions::literal(),
        );
        assert_eq!(non_blank(&yaml), ["key: value", "nested:", "  - item"]);
    }

    #[test]
    fn test_literal_mode_does_not_interpret_markdown() {
        let out = lines("**bold text**\n`code here`", 80, RenderOptions::literal());
        assert_eq!(non_blank(&out), ["**bold text**", "`code here`"]);
        assert!(out.iter().all(|l| {
            l.spans
                .iter()
                .all(|s| !s.style.add_modifier.contains(Modifier::BOLD) && s.style.bg.is_none())
        }));
    }

    #[test]
    fn test_double_newline_is_a_paragraph_break() {
        let single = lines("para one\npara two", 80, RenderOptions::completed());
        let double = lines("para one\n\npara two", 80, RenderOptions::completed());
        assert_eq!(single.len(), 2);
        assert_eq!(double.len(), 4);
    }

    #[test]
    fn test_render_counter_counts_one_run_per_call_including_height() {
        metrics::reset();
        let t = theme();
        let _ = render("a", 10, &t, &RenderOptions::completed());
        assert_eq!(metrics::renders(), 1);
        let _ = compute_height("a", 10, &t, &RenderOptions::completed());
        assert_eq!(metrics::renders(), 2);
    }
}
