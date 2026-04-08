//! Markdown rendering pipeline for the chat pane.
//!
//! 5-stage pipeline: sanitize → parse → transform → highlight → layout
//!
//! # Usage
//! ```ignore
//! let lines = markdown::render(content, width, theme);
//! let height = markdown::compute_height(content, width);
//! ```
//!
//! `compute_height` runs the full pipeline and returns `.len()` — no separate
//! counting implementation, so height and render are always in sync.

pub mod highlight;
pub mod layout;
pub mod parse;
pub mod sanitize;
pub mod transform;

use ratatui::prelude::*;

use crate::adapters::tui::theme::Theme;

/// Render `content` as markdown into a list of ratatui `Line` objects.
///
/// The pipeline is stateless — always re-parses the full input. This is safe
/// for streaming (re-parse the full buffer each tick) and correct for
/// incremental scroll height computation.
pub fn render(content: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let sanitized = sanitize::sanitize(content);
    let parsed = parse::parse(&sanitized);
    let styled = transform::transform(parsed, theme);
    let highlighted = highlight::highlight(styled, theme);
    layout::layout(highlighted, width, theme)
}

/// Compute the number of lines that `render()` would produce for `content` at `width`.
///
/// Runs the full pipeline — intentionally uses the same code path as `render()`
/// to guarantee the height invariant. Never writes a separate line-counting
/// implementation (that would inevitably diverge and cause scroll glitches).
pub fn compute_height(content: &str, width: usize) -> usize {
    // Theme is needed by the pipeline; use the dark theme which is always available.
    // The line count does not depend on colors, only on spacing tokens (indent_list).
    // Both light and dark themes use the same spacing, so either is correct.
    let theme = Theme::dark();
    render(content, width, &theme).len()
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
        let rendered = render(content, width, theme);
        let height = compute_height(content, width);
        assert_eq!(
            rendered.len(),
            height,
            "Height invariant violated for content={content:?} width={width}"
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

    // ── Render content tests ──────────────────────────────────────────────────

    #[test]
    fn test_empty_returns_empty() {
        let t = theme();
        let lines = render("", 80, &t);
        assert!(lines.is_empty());
    }

    #[test]
    fn test_plain_text_renders() {
        let t = theme();
        let lines = render("Hello world", 80, &t);
        assert!(!lines.is_empty());
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("Hello") || text.contains("world"));
    }

    #[test]
    fn test_bold_renders() {
        let t = theme();
        let lines = render("**bold text**", 80, &t);
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
        let lines = render("`code`", 80, &t);
        let has_bg = lines.iter().any(|l| l.spans.iter().any(|s| s.style.bg.is_some()));
        assert!(has_bg);
    }

    #[test]
    fn test_unclosed_fence_renders_as_code_block() {
        let t = theme();
        let lines = render("```rust\nfn main() {", 40, &t);
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
        // Should not panic
        let lines = render("**unclosed bold", 80, &t);
        assert!(!lines.is_empty());
    }

    #[test]
    fn test_html_in_code_block_preserved() {
        let t = theme();
        let content = "```rust\nlet v: Vec<String> = vec![];\nlet b = i < n;\n```\n";
        let lines = render(content, 80, &t);
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
        let lines = render("Hello", 0, &t);
        assert!(lines.is_empty());
    }
}
