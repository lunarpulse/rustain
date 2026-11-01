//! Stage 5: convert the styled block tree into `Vec<Line<'static>>`.
//!
//! Containers (quotes, lists) render their children at a reduced width and
//! then prefix every returned line, so nesting never re-implements wrapping.
//! All width math goes through `text_geometry` (AD-2114). Prose keeps interior
//! whitespace runs, drops only the run at a soft-wrap break, keeps inline code
//! spans whole when they fit on a fresh line and hard-wraps anything wider than
//! a line at grapheme boundaries — no character is clipped away (D5).

use ratatui::prelude::*;
use unicode_segmentation::UnicodeSegmentation;

use super::table;
use super::transform::{StyledBlock, StyledInline, StyledList};
use crate::adapters::tui::text_geometry as geom;
use crate::adapters::tui::theme::Theme;

/// Right-arrow indicator for clipped long lines in code blocks.
const CLIP_INDICATOR: &str = "→";
/// Bullet for unordered lists, at every depth (D8).
const BULLET: &str = "\u{2022}";
/// Block-quote rail: one per nesting level (D9).
const RAIL: &str = "│ ";
/// Cells of [`RAIL`].
const RAIL_CELLS: usize = 2;
/// Containers add prefixes only to this nesting depth (D13); deeper content
/// renders at the capped indentation.
const MAX_PREFIX_DEPTH: usize = 16;
/// A container adds its prefix only while at least this many cells remain for
/// content, so wide graphemes always fit a prefixed row.
const MIN_CONTENT_CELLS: usize = 2;

/// Resolved per-layout styles.
struct Styles {
    body: Style,
    muted: Style,
    rail: Style,
    rule: Style,
    code_bg: Style,
    code_border: Style,
}

impl Styles {
    fn new(theme: &Theme) -> Self {
        let colors = &theme.colors;
        Self {
            body: Style::default().fg(colors.fg_primary),
            muted: Style::default().fg(colors.fg_muted),
            rail: Style::default().fg(colors.fg_secondary),
            rule: Style::default().fg(colors.accent),
            code_bg: Style::default().bg(colors.code_block_bg),
            code_border: Style::default().fg(colors.code_block_border),
        }
    }
}

/// How blank lines separate sibling blocks (D6).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Separator {
    /// One blank line after every block (top level).
    AfterEach,
    /// One blank line between siblings; the last child adds none (containers).
    Between,
    /// No blank lines (children of a tight list item).
    Nothing,
}

fn blank() -> Line<'static> {
    Line::from("")
}

/// Lay the styled blocks out at `width` cells.
#[must_use]
pub fn layout(blocks: Vec<StyledBlock>, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let styles = Styles::new(theme);
    let mut lines = Vec::new();
    render_blocks(&blocks, width, 0, Separator::AfterEach, &styles, &mut lines);
    lines
}

/// Lay user-typed text out as typed: every source line break and whitespace
/// run kept, tabs at 8-cell stops, wrapped at word boundaries with
/// grapheme-safe hard breaks, followed by one blank line like a paragraph.
#[must_use]
pub fn literal(text: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let body = normalized.trim_end_matches('\n');
    if width == 0 || body.is_empty() {
        return Vec::new();
    }
    let style = Style::default().fg(theme.colors.fg_primary);
    let mut wrapper = Wrapper::new(width, true);
    for (index, source) in body.split('\n').enumerate() {
        if index > 0 {
            wrapper.hard_break();
        }
        let source = geom::expand_tabs(source, 0);
        for (is_space, run) in runs(&source) {
            if is_space {
                wrapper.space(run, style);
            } else {
                wrapper.unit(&[Piece {
                    text: run.to_owned(),
                    style,
                }]);
            }
        }
    }
    let mut lines = wrapper.finish();
    lines.push(blank());
    lines
}

fn render_blocks(
    blocks: &[StyledBlock],
    width: usize,
    depth: usize,
    separator: Separator,
    styles: &Styles,
    out: &mut Vec<Line<'static>>,
) {
    for (index, block) in blocks.iter().enumerate() {
        if index > 0 && separator == Separator::Between {
            out.push(blank());
        }
        render_block(block, width, depth, styles, out);
        if separator == Separator::AfterEach {
            out.push(blank());
        }
    }
}

fn render_block(
    block: &StyledBlock,
    width: usize,
    depth: usize,
    styles: &Styles,
    out: &mut Vec<Line<'static>>,
) {
    match block {
        StyledBlock::Paragraph(inlines) => out.extend(wrap_inlines(inlines, width)),
        StyledBlock::Heading { level, inlines } => {
            out.extend(wrap_inlines(inlines, width));
            if *level == 1 {
                out.push(Line::from(Span::styled("─".repeat(width), styles.rule)));
            }
        }
        StyledBlock::List(list) => render_list(list, width, depth, styles, out),
        StyledBlock::Quote(children) => render_quote(children, width, depth, styles, out),
        StyledBlock::Code { language, content } => {
            render_code_block(language.as_deref(), content, width, styles, out);
        }
        StyledBlock::Html(source) => render_html(source, width, styles, out),
        StyledBlock::Table(table) => {
            out.extend(table::render(
                table,
                width,
                styles.code_border,
                styles.muted,
            ));
        }
        StyledBlock::Rule => out.push(Line::from(Span::styled("─".repeat(width), styles.muted))),
    }
}

fn render_quote(
    children: &[StyledBlock],
    width: usize,
    depth: usize,
    styles: &Styles,
    out: &mut Vec<Line<'static>>,
) {
    if depth >= MAX_PREFIX_DEPTH || width < RAIL_CELLS + MIN_CONTENT_CELLS {
        render_blocks(children, width, depth + 1, Separator::Between, styles, out);
        return;
    }
    let mut inner = Vec::new();
    render_blocks(
        children,
        width - RAIL_CELLS,
        depth + 1,
        Separator::Between,
        styles,
        &mut inner,
    );
    for mut line in inner {
        line.spans.insert(0, Span::styled(RAIL, styles.rail));
        out.push(line);
    }
}

/// `• `, `3. `, with `[ ] `/`[x] ` appended for task items.
fn item_marker(list: &StyledList, index: usize) -> String {
    let mut marker = match list.start {
        Some(start) => format!("{}. ", start.saturating_add(index as u64)),
        None => format!("{BULLET} "),
    };
    match list.items[index].task {
        Some(true) => marker.push_str("[x] "),
        Some(false) => marker.push_str("[ ] "),
        None => {}
    }
    marker
}

fn render_list(
    list: &StyledList,
    width: usize,
    depth: usize,
    styles: &Styles,
    out: &mut Vec<Line<'static>>,
) {
    let separator = if list.tight {
        Separator::Nothing
    } else {
        Separator::Between
    };
    for (index, item) in list.items.iter().enumerate() {
        if index > 0 && !list.tight {
            out.push(blank());
        }
        let marker = item_marker(list, index);
        let marker_cells = geom::cells(&marker);
        if depth >= MAX_PREFIX_DEPTH || width < marker_cells + MIN_CONTENT_CELLS {
            render_blocks(&item.blocks, width, depth + 1, separator, styles, out);
            continue;
        }
        let mut inner = Vec::new();
        render_blocks(
            &item.blocks,
            width - marker_cells,
            depth + 1,
            separator,
            styles,
            &mut inner,
        );
        if inner.is_empty() {
            out.push(Line::from(Span::raw(marker)));
            continue;
        }
        let hanging = geom::spaces(marker_cells);
        for (row, mut line) in inner.into_iter().enumerate() {
            let lead = if row == 0 {
                marker.clone()
            } else {
                hanging.clone()
            };
            line.spans.insert(0, Span::raw(lead));
            out.push(line);
        }
    }
}

/// Fenced code keeps its bordered container and language header; lines never
/// wrap and clip with `→`. Tabs expand to 8-cell stops and other control
/// characters are dropped so the border stays straight (D5).
fn render_code_block(
    language: Option<&str>,
    content: &str,
    width: usize,
    styles: &Styles,
    out: &mut Vec<Line<'static>>,
) {
    let code_lines: Vec<&str> = if content.is_empty() {
        Vec::new()
    } else {
        content.split('\n').collect()
    };
    // A border needs three cells; narrower panes paint the bare code.
    if width < 3 {
        for code_line in code_lines {
            let fitted = geom::fit_exact(&geom::expand_tabs(code_line, 0), width);
            out.push(Line::from(Span::styled(fitted, styles.code_bg)));
        }
        return;
    }
    let inner_width = width - 2;
    out.push(build_code_header(language, inner_width, styles.code_border));
    for code_line in code_lines {
        let code_line = geom::expand_tabs(code_line, 0);
        if geom::cells(&code_line) <= inner_width {
            out.push(Line::from(vec![
                Span::styled("│", styles.code_border),
                Span::styled(geom::pad_right(&code_line, inner_width), styles.code_bg),
                Span::styled("│", styles.code_border),
            ]));
        } else {
            // Reserve one cell for the indicator.
            let clipped = geom::fit_exact(&code_line, inner_width - 1);
            out.push(Line::from(vec![
                Span::styled("│", styles.code_border),
                Span::styled(clipped, styles.code_bg),
                Span::styled(CLIP_INDICATOR, styles.code_border),
                Span::styled("│", styles.code_border),
            ]));
        }
    }
    out.push(Line::from(Span::styled(
        format!("└{}┘", "─".repeat(inner_width)),
        styles.code_border,
    )));
}

/// Header line `┌─ <lang> ──────┐` or `┌──────────────┐` over `inner_width` cells.
fn build_code_header(language: Option<&str>, inner_width: usize, border: Style) -> Line<'static> {
    let header_inner = match language {
        Some(lang) => {
            let label = format!(" {} ", clean_prose(lang));
            let label_cells = geom::cells(&label);
            if label_cells + 2 <= inner_width {
                format!("─{label}{}", "─".repeat(inner_width - label_cells - 1))
            } else {
                "─".repeat(inner_width)
            }
        }
        None => "─".repeat(inner_width),
    };
    Line::from(Span::styled(format!("┌{header_inner}┐"), border))
}

/// An HTML block shows as literal body text: source lines and whitespace kept,
/// tabs expanded, long lines hard-wrapped at grapheme boundaries.
fn render_html(source: &str, width: usize, styles: &Styles, out: &mut Vec<Line<'static>>) {
    for raw in source.split('\n') {
        let line = geom::expand_tabs(raw, 0);
        for row in geom::wrap_hard(&line, width) {
            out.push(Line::from(Span::styled(row.to_owned(), styles.body)));
        }
    }
}

// ── Wrapping ─────────────────────────────────────────────────────────────────

/// A run of non-whitespace text in one style.
struct Piece {
    text: String,
    style: Style,
}

/// Split `text` into maximal runs of spaces and non-spaces: `(is_space, run)`.
fn runs(text: &str) -> Vec<(bool, &str)> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut current: Option<bool> = None;
    for (at, c) in text.char_indices() {
        let space = c == ' ';
        match current {
            Some(previous) if previous != space => {
                out.push((previous, &text[start..at]));
                start = at;
                current = Some(space);
            }
            None => current = Some(space),
            _ => {}
        }
    }
    if let Some(last) = current {
        out.push((last, &text[start..]));
    }
    out
}

/// Prose text with tabs read as one space and every other control character
/// dropped (the terminal painter drops them, so they must not be measured).
pub(super) fn clean_prose(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.chars().any(char::is_control) {
        return std::borrow::Cow::Borrowed(text);
    }
    std::borrow::Cow::Owned(
        text.chars()
            .filter_map(|c| match c {
                '\t' => Some(' '),
                c if c.is_control() => None,
                c => Some(c),
            })
            .collect(),
    )
}

/// Greedy whitespace-preserving wrapper.
///
/// Callers feed *units* (maximal runs of non-space text, possibly spanning
/// several styles) separated by *spaces*. A space is emitted only when the next
/// unit follows on the same row; a space at a wrap break is consumed.
struct Wrapper {
    width: usize,
    /// Keep the space run that starts a logical line (literal text only).
    keep_leading: bool,
    lines: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    col: usize,
    line_start: bool,
    pending: Vec<(String, Style)>,
    pending_cells: usize,
}

impl Wrapper {
    fn new(width: usize, keep_leading: bool) -> Self {
        Self {
            width: width.max(1),
            keep_leading,
            lines: Vec::new(),
            current: Vec::new(),
            col: 0,
            line_start: true,
            pending: Vec::new(),
            pending_cells: 0,
        }
    }

    fn push_text(&mut self, text: &str, style: Style) {
        if text.is_empty() {
            return;
        }
        if let Some(last) = self.current.last_mut()
            && last.style == style
        {
            last.content.to_mut().push_str(text);
            return;
        }
        self.current.push(Span::styled(text.to_owned(), style));
    }

    fn flush_line(&mut self) {
        let spans = std::mem::take(&mut self.current);
        self.lines.push(Line::from(spans));
        self.col = 0;
        self.line_start = false;
        self.pending.clear();
        self.pending_cells = 0;
    }

    fn space(&mut self, text: &str, style: Style) {
        self.pending_cells += geom::cells(text);
        self.pending.push((text.to_owned(), style));
    }

    fn hard_break(&mut self) {
        self.flush_line();
        self.line_start = true;
    }

    fn unit(&mut self, pieces: &[Piece]) {
        let unit_cells: usize = pieces.iter().map(|p| geom::cells(&p.text)).sum();
        let lead = if self.col > 0 || (self.keep_leading && self.line_start) {
            self.pending_cells
        } else {
            0
        };
        if self.col + lead + unit_cells <= self.width {
            if lead > 0 {
                for (text, style) in std::mem::take(&mut self.pending) {
                    self.push_text(&text, style);
                }
            }
            for piece in pieces {
                self.push_text(&piece.text, piece.style);
            }
            self.col += lead + unit_cells;
        } else {
            if self.col > 0 {
                self.flush_line();
            }
            if unit_cells <= self.width {
                for piece in pieces {
                    self.push_text(&piece.text, piece.style);
                }
                self.col = unit_cells;
            } else {
                self.hard_wrap(pieces);
            }
        }
        self.pending.clear();
        self.pending_cells = 0;
        self.line_start = false;
    }

    /// A unit wider than a whole row breaks at grapheme boundaries. A grapheme
    /// wider than the row itself (a CJK glyph at one cell) cannot be painted
    /// and is replaced by blank cells, never fragmented.
    fn hard_wrap(&mut self, pieces: &[Piece]) {
        let mut row = 0usize;
        for piece in pieces {
            let text = piece.text.as_str();
            // ASCII text is one cell per byte: cut whole rows without
            // walking graphemes (keeps a 1 MiB token cheap).
            if text.is_ascii() {
                let mut rest = text;
                while !rest.is_empty() {
                    if row >= self.width {
                        self.flush_line();
                        row = 0;
                    }
                    let take = (self.width - row).min(rest.len());
                    self.push_text(&rest[..take], piece.style);
                    row += take;
                    rest = &rest[take..];
                }
                continue;
            }
            // Start of the slice of `text` not yet pushed onto the current row.
            let mut pending_from = 0usize;
            for (at, grapheme) in text.grapheme_indices(true) {
                let mut cells = geom::cells(grapheme);
                let oversized = cells > self.width;
                if oversized {
                    cells = self.width;
                }
                if row > 0 && row + cells > self.width {
                    self.push_text(&text[pending_from..at], piece.style);
                    self.flush_line();
                    row = 0;
                    pending_from = at;
                }
                if oversized {
                    self.push_text(&text[pending_from..at], piece.style);
                    self.push_text(&" ".repeat(self.width), piece.style);
                    pending_from = at + grapheme.len();
                }
                row += cells;
            }
            self.push_text(&text[pending_from..], piece.style);
        }
        self.col = row;
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        if !self.current.is_empty() {
            self.flush_line();
        }
        if self.lines.is_empty() {
            self.lines.push(blank());
        }
        self.lines
    }
}

/// Wrap styled inline nodes to `width` cells, one `Line` per physical row.
fn wrap_inlines(inlines: &[StyledInline], width: usize) -> Vec<Line<'static>> {
    let mut wrapper = Wrapper::new(width, false);
    let mut unit: Vec<Piece> = Vec::new();
    fn flush(wrapper: &mut Wrapper, unit: &mut Vec<Piece>) {
        if !unit.is_empty() {
            wrapper.unit(unit);
            unit.clear();
        }
    }
    for inline in inlines {
        match inline {
            StyledInline::Text(span) => {
                let text = clean_prose(&span.content);
                if span.atomic {
                    if !text.is_empty() {
                        unit.push(Piece {
                            text: text.into_owned(),
                            style: span.style,
                        });
                    }
                    continue;
                }
                for (is_space, run) in runs(&text) {
                    if is_space {
                        flush(&mut wrapper, &mut unit);
                        wrapper.space(run, span.style);
                    } else {
                        unit.push(Piece {
                            text: run.to_owned(),
                            style: span.style,
                        });
                    }
                }
            }
            StyledInline::SoftBreak(style) => {
                flush(&mut wrapper, &mut unit);
                wrapper.space(" ", *style);
            }
            StyledInline::HardBreak => {
                flush(&mut wrapper, &mut unit);
                wrapper.hard_break();
            }
        }
    }
    flush(&mut wrapper, &mut unit);
    wrapper.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::markdown::{RenderOptions, render};

    fn theme() -> Theme {
        Theme::dark()
    }

    fn md_lines(markdown: &str, width: usize) -> Vec<Line<'static>> {
        render(markdown, width, &theme(), &RenderOptions::completed()).lines
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn md_rows(markdown: &str, width: usize) -> Vec<String> {
        md_lines(markdown, width).iter().map(line_text).collect()
    }

    fn literal_rows(text: &str, width: usize) -> Vec<String> {
        literal(text, width, &theme())
            .iter()
            .map(line_text)
            .collect()
    }

    #[test]
    fn test_empty_input_returns_empty() {
        assert!(layout(vec![], 80, &theme()).is_empty());
    }

    #[test]
    fn test_zero_width_returns_empty() {
        assert!(md_lines("hello", 0).is_empty());
        assert!(literal("hello", 0, &theme()).is_empty());
    }

    #[test]
    fn test_blocks_are_followed_by_exactly_one_blank_line() {
        assert_eq!(md_rows("hello", 80), ["hello", ""]);
        assert_eq!(md_rows("# Title", 10), ["Title", "──────────", ""]);
        assert_eq!(md_rows("## Title", 10), ["Title", ""]);
        assert_eq!(md_rows("- a\n- b", 80), ["• a", "• b", ""]);
        assert_eq!(md_rows("---", 6), ["──────", ""]);
        assert_eq!(md_rows("> q", 20), ["│ q", ""]);
        assert_eq!(md_rows("<b>x</b>\n", 20), ["<b>x</b>", ""]);
    }

    #[test]
    fn test_wrap_at_width() {
        assert_eq!(md_rows("hello world", 5), ["hello", "world", ""]);
        assert_eq!(md_rows("hello world", 11), ["hello world", ""]);
        assert_eq!(md_rows("hello world", 80).len(), 2);
    }

    #[test]
    fn test_code_block_has_header_and_footer() {
        let rows = md_rows("```rust\nfn main() {}\n```", 40);
        assert_eq!(rows.len(), 4);
        assert!(rows[0].starts_with("┌─ rust ") && rows[0].ends_with('┐'));
        assert!(rows[1].starts_with("│fn main() {}") && rows[1].ends_with('│'));
        assert!(rows[2].starts_with('└') && rows[2].ends_with('┘'));
        assert_eq!(rows[3], "");
        let python = md_rows("```python\nprint('hi')\n```", 40);
        assert!(python[0].contains("python"));
    }

    #[test]
    fn test_code_header_label_drops_control_characters() {
        let rows = md_rows("```\u{1b}]52;c;eA==\u{7}\nx\n```", 40);
        assert!(rows[0].starts_with("┌─ ]52;c;eA== "), "{:?}", rows[0]);
        assert!(!rows[0].chars().any(char::is_control), "{:?}", rows[0]);
        assert_eq!(geom::cells(&rows[0]), 40);
    }

    #[test]
    fn test_code_block_long_line_clipped() {
        let rows = md_rows(&format!("```\n{}\n```", "x".repeat(100)), 20);
        assert!(rows[1].contains(CLIP_INDICATOR), "{:?}", rows[1]);
        assert_eq!(geom::cells(&rows[1]), 20);
    }

    #[test]
    fn test_code_block_never_overflows_tiny_widths() {
        for width in 1..8 {
            for row in md_rows("```rust\nlet long = 1;\n```", width) {
                assert!(geom::cells(&row) <= width, "{row:?} at {width}");
            }
        }
    }

    #[test]
    fn test_code_block_tabs_expand_and_keep_the_border_straight() {
        let rows = md_rows("```\n\tx\n\t\ty\n```", 24);
        assert_eq!(rows[1], format!("│{}x{}│", " ".repeat(8), " ".repeat(13)));
        assert_eq!(rows[2], format!("│{}y{}│", " ".repeat(16), " ".repeat(5)));
        for row in &rows[..4] {
            assert_eq!(geom::cells(row), 24);
        }
    }

    #[test]
    fn test_list_hanging_indent_matches_the_marker_width() {
        let rows = md_rows(&format!("- {}", "a ".repeat(20).trim()), 20);
        assert!(rows.len() > 2);
        for row in &rows[1..rows.len() - 1] {
            assert_eq!(row.chars().take_while(|c| *c == ' ').count(), 2, "{row:?}");
        }
        let ordered = md_rows(&format!("10. {}", "a ".repeat(20).trim()), 20);
        assert_eq!(
            ordered[1].chars().take_while(|c| *c == ' ').count(),
            4,
            "{ordered:?}"
        );
    }

    #[test]
    fn test_loose_lists_and_quotes_separate_children_with_prefixed_blank_lines() {
        assert_eq!(md_rows("- a\n\n- b", 20), ["• a", "", "• b", ""]);
        assert_eq!(md_rows("> a\n>\n> b", 20), ["│ a", "│ ", "│ b", ""]);
    }

    #[test]
    fn test_container_depth_is_capped_and_never_wider_than_the_pane() {
        let source = format!("{}x", "> ".repeat(2_000));
        for width in [3usize, 10, 40] {
            let rows = md_rows(&source, width);
            assert!(rows.iter().any(|r| r.ends_with('x')), "{width}");
            for row in &rows {
                assert!(geom::cells(row) <= width, "{row:?} at {width}");
            }
        }
        let nested: String = (0..40)
            .map(|level| format!("{}- l{level}\n", "  ".repeat(level)))
            .collect();
        for row in md_rows(&nested, 30) {
            assert!(geom::cells(&row) <= 30, "{row:?}");
        }
    }

    #[test]
    fn test_whitespace_runs_are_preserved_and_the_break_run_is_consumed() {
        assert_eq!(md_rows("x  y", 20), ["x  y", ""]);
        assert_eq!(md_rows("aaaa   bbbb", 5), ["aaaa", "bbbb", ""]);
        assert_eq!(md_rows("`a  b`", 20), ["a  b", ""]);
        assert_eq!(md_rows("aaaa `b  c`", 8), ["aaaa", "b  c", ""]);
    }

    #[test]
    fn test_unit_spanning_styles_is_not_split_between_them() {
        // `**bo**ld` is one word: it moves to the next row whole.
        assert_eq!(md_rows("aaaa **bo**ld", 8), ["aaaa", "bold", ""]);
    }

    #[test]
    fn test_overlong_token_hard_wraps_without_loss() {
        assert_eq!(md_rows("abcdefghij", 4), ["abcd", "efgh", "ij", ""]);
        assert_eq!(md_rows("x abcdefghij", 4), ["x", "abcd", "efgh", "ij", ""]);
        assert_eq!(md_rows("`abcdefghij`", 4), ["abcd", "efgh", "ij", ""]);
    }

    #[test]
    fn test_wrap_unicode_cjk() {
        assert_eq!(md_rows("你好 world", 4), ["你好", "worl", "d", ""]);
        assert_eq!(md_rows("你好世界", 4), ["你好", "世界", ""]);
    }

    #[test]
    fn test_prose_controls_are_not_painted() {
        assert_eq!(md_rows("a\u{7}b\tc", 20), ["ab c", ""]);
    }

    #[test]
    fn test_literal_keeps_lines_whitespace_and_tabs() {
        assert_eq!(
            literal_rows("**x** <T>\n  y\tz\n\nlast", 40),
            ["**x** <T>", "  y     z", "", "last", ""]
        );
        assert_eq!(literal_rows("a\r\nb\rc", 40), ["a", "b", "c", ""]);
        assert_eq!(literal_rows("a  b", 40), ["a  b", ""]);
        assert_eq!(literal_rows("trailing\n\n", 40), ["trailing", ""]);
    }

    #[test]
    fn test_literal_wraps_at_words_then_graphemes() {
        assert_eq!(literal_rows("aaa bbb ccc", 7), ["aaa bbb", "ccc", ""]);
        assert_eq!(literal_rows("abcdefghij", 4), ["abcd", "efgh", "ij", ""]);
        // Indentation is kept while the word fits behind it; a word that
        // only fits without it takes the whole row instead.
        assert_eq!(literal_rows("    word more", 10), ["    word", "more", ""]);
        assert_eq!(
            literal_rows("    indented words", 10),
            ["indented", "words", ""]
        );
        assert!(literal("", 10, &theme()).is_empty());
    }

    // ── Story 21.11 display-width tests (entered through `markdown::render`) ──
    //
    // G = 2 cells / 1 grapheme / 6 scalars, C = 2 cells, A = 1 cell / 2 scalars.
    const G: &str = "👨\u{200d}👩\u{200d}👧";
    const C: &str = "界";
    const A: &str = "e\u{301}";

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
        let rows_at = |width: usize| -> Vec<String> {
            md_lines(&token, width).iter().map(line_text).collect()
        };
        assert_eq!(rows_at(4)[0], token);
        assert_eq!(rows_at(3)[0], format!("{G}x"));
        assert_eq!(rows_at(2)[0], G);
        // Below one grapheme the glyph is omitted, never fragmented.
        let tiny = rows_at(1);
        assert!(
            !tiny[0].contains('👨') && !tiny[0].contains('\u{200d}'),
            "fragment of G painted at width 1: {:?}",
            tiny[0]
        );
        assert_eq!(geom::cells(&tiny[0]), 1);
        // The tail is wrapped, not lost (D5).
        assert_eq!(tiny[1..3], ["x", "x"]);
        assert_eq!(rows_at(3)[1], "x");

        let wide = C.repeat(3); // 6 cells
        assert_eq!(rows_at_for(&wide, 6)[0], wide);
        assert_eq!(rows_at_for(&wide, 5)[0], C.repeat(2));
        assert_eq!(rows_at_for(&wide, 5)[1], C);

        let accent = A.repeat(4); // 4 cells
        assert_eq!(rows_at_for(&accent, 4)[0], accent);
        assert_eq!(rows_at_for(&accent, 3)[0], A.repeat(3));
    }

    fn rows_at_for(text: &str, width: usize) -> Vec<String> {
        md_lines(text, width).iter().map(line_text).collect()
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
