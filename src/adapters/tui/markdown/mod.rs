//! Markdown rendering pipeline for the chat pane.
//!
//! 5-stage pipeline: sanitize → parse → transform → highlight → layout
//!
//! # Usage
//! ```ignore
//! let opts = markdown::RenderOptions::completed(); // or ::default() for streaming
//! let lines = markdown::render(content, width, theme, &opts);
//! let height = markdown::compute_height(content, width, &opts);
//! ```
//!
//! `compute_height` runs the full pipeline and returns `.len()` — no separate
//! counting implementation, so height and render are always in sync.
//!
//! Both `render` and `compute_height` must receive the **same** `RenderOptions`
//! instance to guarantee the height invariant required by virtual scrolling.

pub mod highlight;
pub mod layout;
pub mod parse;
pub mod sanitize;
pub mod transform;

use ratatui::prelude::*;

use crate::adapters::tui::theme::Theme;

/// Pipeline configuration. Streaming uses `Default`; completed messages use `completed()`.
///
/// Extension point for Epic 15 (syntax highlighting, tables, etc.).
/// Adding a field here with a `Default` impl is backwards-compatible — all
/// existing call sites that use `::default()` automatically get the new behaviour off.
pub struct RenderOptions {
    /// When `true`, unclosed inline markers (`**`, `*`, `` ` ``, `~~`) in `Plain`
    /// spans are stripped rather than rendered as literal characters (AC3, DF-078).
    /// Set to `true` for completed messages; leave `false` during streaming so the
    /// model can close the marker on the next chunk.
    pub strip_unclosed_markers: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            strip_unclosed_markers: false,
        }
    }
}

impl RenderOptions {
    /// Options for a **completed** message — strips unclosed inline markers.
    pub fn completed() -> Self {
        Self {
            strip_unclosed_markers: true,
        }
    }
}

/// Render `content` as markdown into a list of ratatui `Line` objects.
///
/// The pipeline is stateless — always re-parses the full input. This is safe
/// for streaming (re-parse the full buffer each tick) and correct for
/// incremental scroll height computation.
pub fn render(
    content: &str,
    width: usize,
    theme: &Theme,
    opts: &RenderOptions,
) -> Vec<Line<'static>> {
    let sanitized = sanitize::sanitize(content);
    let parsed = parse::parse(&sanitized);
    let styled = transform::transform(parsed, theme, opts);
    let highlighted = highlight::highlight(styled, theme);
    layout::layout(highlighted, width, theme)
}

/// Compute the number of lines that `render()` would produce for `content` at `width`.
///
/// Runs the full pipeline — intentionally uses the same code path as `render()`
/// to guarantee the height invariant. Never writes a separate line-counting
/// implementation (that would inevitably diverge and cause scroll glitches).
///
/// Pass the **same** `opts` that will be used for the matching `render()` call so
/// heights stay in sync.
pub fn compute_height(content: &str, width: usize, opts: &RenderOptions) -> usize {
    // Theme is needed by the pipeline; use the dark theme which is always available.
    // The line count does not depend on colors, only on spacing tokens (indent_list).
    // Both light and dark themes use the same spacing, so either is correct.
    let theme = Theme::dark();
    render(content, width, &theme, opts).len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::theme::Theme;

    fn theme() -> Theme {
        Theme::dark()
    }

    // ── Height invariant tests ────────────────────────────────────────────────

    fn assert_height_invariant(content: &str, width: usize, theme: &Theme) {
        assert_height_invariant_with_opts(content, width, theme, &RenderOptions::default());
        assert_height_invariant_with_opts(content, width, theme, &RenderOptions::completed());
    }

    fn assert_height_invariant_with_opts(
        content: &str,
        width: usize,
        theme: &Theme,
        opts: &RenderOptions,
    ) {
        let rendered = render(content, width, theme, opts);
        let height = compute_height(content, width, opts);
        assert_eq!(
            rendered.len(),
            height,
            "Height invariant violated for content={content:?} width={width} strip_unclosed_markers={}",
            opts.strip_unclosed_markers,
        );
    }

    #[test]
    fn test_height_invariant_plain_text() {
        let t = theme();
        assert_height_invariant("Hello world", 80, &t);
    }

    #[test]
    fn test_height_invariant_heading() {
        let t = theme();
        assert_height_invariant("# Title\n\nParagraph", 80, &t);
    }

    #[test]
    fn test_height_invariant_list() {
        let t = theme();
        assert_height_invariant("- item1\n- item2\n- item3\n", 80, &t);
    }

    #[test]
    fn test_height_invariant_code_block() {
        let t = theme();
        assert_height_invariant("```rust\nfn main() {}\n```\n", 80, &t);
    }

    #[test]
    fn test_height_invariant_mixed_content() {
        let t = theme();
        let content = "# Title\n\nParagraph with **bold** and `code`.\n\n- item 1\n- item 2\n\n```rust\nfn main() {}\n```\n";
        assert_height_invariant(content, 80, &t);
    }

    #[test]
    fn test_height_invariant_empty_string() {
        let t = theme();
        assert_height_invariant("", 80, &t);
    }

    #[test]
    fn test_height_invariant_single_line() {
        let t = theme();
        assert_height_invariant("A single line", 80, &t);
    }

    #[test]
    fn test_height_invariant_long_line_wrapping() {
        let t = theme();
        let content = "word ".repeat(50);
        assert_height_invariant(content.trim(), 40, &t);
    }

    #[test]
    fn test_height_invariant_cjk() {
        let t = theme();
        // CJK chars use 2 display cols each
        assert_height_invariant("你好世界 Hello", 80, &t);
    }

    #[test]
    fn test_height_invariant_thematic_break() {
        let t = theme();
        assert_height_invariant("text\n\n---\n\nmore", 80, &t);
    }

    /// Height invariant holds for both default and completed options on unclosed markers.
    // Covers: AC3 (DF-078), height invariant
    #[test]
    fn test_height_invariant_unclosed_markers() {
        let t = theme();
        let cases = [
            "**unclosed bold",
            "*unclosed italic",
            "`unclosed code",
            "~~unclosed strike",
            "mixed **bold and *italic unclosed",
        ];
        for content in cases {
            assert_height_invariant(content, 80, &t);
        }
    }

    // ── Render content tests ──────────────────────────────────────────────────

    #[test]
    fn test_empty_returns_empty() {
        let t = theme();
        let opts = RenderOptions::default();
        let lines = render("", 80, &t, &opts);
        assert!(lines.is_empty());
    }

    #[test]
    fn test_plain_text_renders() {
        let t = theme();
        let opts = RenderOptions::default();
        let lines = render("Hello world", 80, &t, &opts);
        assert!(!lines.is_empty());
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("Hello") || text.contains("world"));
    }

    #[test]
    fn test_bold_renders() {
        let t = theme();
        let opts = RenderOptions::default();
        let lines = render("**bold text**", 80, &t, &opts);
        let has_bold = lines.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::BOLD))
        });
        assert!(has_bold);
    }

    #[test]
    fn test_code_span_renders_with_bg() {
        let t = theme();
        let opts = RenderOptions::default();
        let lines = render("`code`", 80, &t, &opts);
        let has_bg = lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.style.bg.is_some()));
        assert!(has_bg);
    }

    #[test]
    fn test_unclosed_fence_renders_as_code_block() {
        let t = theme();
        let opts = RenderOptions::default();
        let lines = render("```rust\nfn main() {", 40, &t, &opts);
        // After sanitize, fence is closed; should render as code block with borders
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<String>();
        assert!(all_text.contains('┌') || all_text.contains("rust"));
    }

    #[test]
    fn test_unclosed_bold_degrades_gracefully() {
        let t = theme();
        let opts = RenderOptions::default();
        // Should not panic
        let lines = render("**unclosed bold", 80, &t, &opts);
        assert!(!lines.is_empty());
    }

    #[test]
    fn test_html_in_code_block_preserved() {
        let t = theme();
        let opts = RenderOptions::default();
        let content = "```rust\nlet v: Vec<String> = vec![];\nlet b = i < n;\n```\n";
        let lines = render(content, 80, &t, &opts);
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<String>();
        assert!(
            all_text.contains("Vec<String>"),
            "Rust generics should be preserved in code blocks"
        );
        assert!(
            all_text.contains("i < n"),
            "Comparison operators should be preserved in code blocks"
        );
    }

    #[test]
    fn test_zero_width_returns_empty() {
        let t = theme();
        let opts = RenderOptions::default();
        let lines = render("Hello", 0, &t, &opts);
        assert!(lines.is_empty());
    }

    // ── SoftBreak render tests (DF-SoftBreak) ────────────────────────────────
    //
    // Layout appends one blank Line::from("") after each paragraph block.
    // So N content lines in a single paragraph → N+1 rendered lines total.
    // Tests use content-presence checks or `>= N` guards, not exact counts,
    // to remain robust to future layout spacing changes.

    /// Single \n in user input renders on two distinct lines (not collapsed to one).
    /// Covers: DF-SoftBreak (core fix — multi-line user input displays correctly)
    #[test]
    fn test_single_newline_renders_as_two_lines() {
        let t = theme();
        let lines = render("line one\nline two", 80, &t, &RenderOptions::default());
        // Layout: 2 content lines + 1 trailing blank = 3 total; we check ≥ 2.
        assert!(
            lines.len() >= 2,
            "Single \\n should produce at least 2 rendered lines, got {}",
            lines.len()
        );
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        // Both pieces of content must appear
        assert!(
            all_text.contains("line one"),
            "'line one' missing from: {all_text:?}"
        );
        assert!(
            all_text.contains("line two"),
            "'line two' missing from: {all_text:?}"
        );
        // They must be on DIFFERENT rendered lines (not collapsed to one line)
        let non_blank: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .filter(|s| !s.trim().is_empty())
            .collect();
        assert_eq!(
            non_blank.len(),
            2,
            "Expected 2 non-blank lines, got: {non_blank:?}"
        );
        assert!(
            non_blank[0].contains("line one"),
            "First content line wrong: {:?}",
            non_blank[0]
        );
        assert!(
            non_blank[1].contains("line two"),
            "Second content line wrong: {:?}",
            non_blank[1]
        );
    }

    /// Three-line user input renders on three separate lines.
    /// Covers: DF-SoftBreak (multi-line case)
    #[test]
    fn test_multiline_user_input_preserves_all_lines() {
        let t = theme();
        let lines = render("alpha\nbeta\ngamma", 80, &t, &RenderOptions::default());
        let non_blank: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .filter(|s| !s.trim().is_empty())
            .collect();
        assert_eq!(
            non_blank.len(),
            3,
            "Expected 3 non-blank lines: {non_blank:?}"
        );
        assert!(non_blank[0].contains("alpha"), "Line 0: {:?}", non_blank[0]);
        assert!(non_blank[1].contains("beta"), "Line 1: {:?}", non_blank[1]);
        assert!(non_blank[2].contains("gamma"), "Line 2: {:?}", non_blank[2]);
    }

    /// Markdown formatting renders correctly alongside soft-break lines.
    /// Covers: DF-SoftBreak (copy-paste markdown with inline formatting)
    #[test]
    fn test_markdown_formatting_preserved_across_newlines() {
        let t = theme();
        let lines = render(
            "**bold text**\nnormal text",
            80,
            &t,
            &RenderOptions::default(),
        );
        let non_blank: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .filter(|s| !s.trim().is_empty())
            .collect();
        assert_eq!(
            non_blank.len(),
            2,
            "Expected 2 non-blank lines: {non_blank:?}"
        );
        let all: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(
            all.contains("bold text"),
            "Bold text content missing: {all:?}"
        );
        assert!(all.contains("normal text"), "Normal text missing: {all:?}");
    }

    /// YAML-like content with single newlines preserves line structure.
    /// Covers: DF-SoftBreak (copy-paste YAML use case)
    #[test]
    fn test_yaml_like_content_preserves_line_structure() {
        let t = theme();
        // "key: value\nnested:" → paragraph (2 content lines)
        // "  - item" → parsed as a list item by pulldown-cmark
        // Exact block structure varies; we assert ≥ 3 non-blank lines total.
        let yaml = "key: value\nnested:\n  - item";
        let lines = render(yaml, 80, &t, &RenderOptions::default());
        let non_blank: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .filter(|s| !s.trim().is_empty())
            .collect();
        assert!(
            non_blank.len() >= 3,
            "YAML with 2 newlines should render ≥ 3 non-blank lines, got: {non_blank:?}"
        );
    }

    /// Double \n (paragraph break) creates at least as many rendered lines as single \n.
    /// Covers: DF-SoftBreak (existing paragraph behaviour unchanged)
    #[test]
    fn test_double_newline_still_creates_paragraph_break() {
        let t = theme();
        let single = render("para one\npara two", 80, &t, &RenderOptions::default());
        let double = render("para one\n\npara two", 80, &t, &RenderOptions::default());
        assert!(
            double.len() >= single.len(),
            "Double newline (paragraph) should produce ≥ lines of single newline. \
            single={}, double={}",
            single.len(),
            double.len()
        );
    }

    /// Height invariant holds for single-newline content after SoftBreak change.
    /// Covers: DF-SoftBreak (height invariant must not be broken)
    #[test]
    fn test_height_invariant_single_newline() {
        let t = theme();
        assert_height_invariant("line one\nline two", 80, &t);
        assert_height_invariant("alpha\nbeta\ngamma", 80, &t);
        assert_height_invariant("key: value\nnested:\n  - item", 80, &t);
    }

    /// Code spans inline render correctly alongside newline-separated lines.
    /// Covers: DF-SoftBreak (inline code preserved with newlines)
    #[test]
    fn test_inline_code_preserved_with_newlines() {
        let t = theme();
        let lines = render(
            "`code here`\nnormal line",
            80,
            &t,
            &RenderOptions::default(),
        );
        let non_blank: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .filter(|s| !s.trim().is_empty())
            .collect();
        assert_eq!(
            non_blank.len(),
            2,
            "Expected 2 non-blank lines: {non_blank:?}"
        );
        let all: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(
            all.contains("code here"),
            "Code span content missing: {all:?}"
        );
        assert!(all.contains("normal line"), "Normal line missing: {all:?}");
    }
}
