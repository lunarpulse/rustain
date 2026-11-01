use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_segmentation::UnicodeSegmentation;

use crate::adapters::tui::text_geometry as geom;
use crate::adapters::tui::theme::Theme;
use crate::domain::models::FocusState;

/// Maximum number of visible input lines before scrolling.
pub const MAX_INPUT_LINES: usize = 8;

/// Cells of one composer grapheme when it starts at `used` cells into a row of
/// `width` cells. Tabs advance to the next tab stop (display only; the source
/// keeps the tab), controls other than tab are not painted, everything else is
/// its string width.
fn grapheme_cells(grapheme: &str, used: usize, width: usize) -> usize {
    if grapheme == "\t" {
        (geom::TAB_STOP - used % geom::TAB_STOP).min(width)
    } else if grapheme.chars().next().is_some_and(char::is_control) {
        0
    } else {
        geom::cells(grapheme)
    }
}

/// One physical (visual) row of the composer: a half-open slice of the source
/// in scalar and byte units. `closes_line` marks the last row of a logical
/// line; `slot` marks the blank caret row after an exactly-full final row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VisualRow {
    scalar_start: usize,
    scalar_end: usize,
    byte_start: usize,
    byte_end: usize,
    cells: usize,
    closes_line: bool,
    slot: bool,
}

/// The single width-aware projection of a composer buffer (Story 21.11, A2).
///
/// Height, paint, scroll and source-index <-> display mapping all read this
/// one packing: whole grapheme clusters per row, source spaces/newlines
/// preserved, scalar source indices kept distinct from byte offsets and cells.
#[derive(Clone, Debug)]
pub struct InputGeometry {
    rows: Vec<VisualRow>,
    width: usize,
}

impl InputGeometry {
    /// Project `text` into rows of `inner_width` cells (at least one).
    #[must_use]
    pub fn new(text: &str, inner_width: usize) -> Self {
        let width = inner_width.max(1);
        let mut rows = Vec::new();
        let mut scalar = 0usize;
        let mut byte = 0usize;
        for line in text.split('\n') {
            let mut row_scalar = scalar;
            let mut row_byte = byte;
            let mut used = 0usize;
            for grapheme in line.graphemes(true) {
                let mut w = grapheme_cells(grapheme, used, width);
                if used > 0 && used.saturating_add(w) > width {
                    rows.push(VisualRow {
                        scalar_start: row_scalar,
                        scalar_end: scalar,
                        byte_start: row_byte,
                        byte_end: byte,
                        cells: used,
                        closes_line: false,
                        slot: false,
                    });
                    row_scalar = scalar;
                    row_byte = byte;
                    used = 0;
                    w = grapheme_cells(grapheme, 0, width);
                }
                used = used.saturating_add(w);
                scalar += grapheme.chars().count();
                byte += grapheme.len();
            }
            rows.push(VisualRow {
                scalar_start: row_scalar,
                scalar_end: scalar,
                byte_start: row_byte,
                byte_end: byte,
                cells: used,
                closes_line: true,
                slot: false,
            });
            // The logical line's '\n' terminator (absent after the last line).
            scalar += 1;
            byte += 1;
        }
        let total_scalar = scalar - 1;
        let total_byte = byte - 1;
        if rows.last().is_some_and(|last| last.cells >= width) {
            rows.push(VisualRow {
                scalar_start: total_scalar,
                scalar_end: total_scalar,
                byte_start: total_byte,
                byte_end: total_byte,
                cells: 0,
                closes_line: true,
                slot: true,
            });
        }
        Self { rows, width }
    }

    /// Number of physical rows, including a trailing blank caret row.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Display cells used by `row` (0 past the end).
    #[must_use]
    pub fn row_cells(&self, row: usize) -> usize {
        self.rows.get(row).map_or(0, |r| r.cells)
    }

    /// Painted text of `row` (tabs expanded, controls dropped).
    #[must_use]
    pub fn row_text(&self, text: &str, row: usize) -> String {
        let Some(r) = self.rows.get(row) else {
            return String::new();
        };
        let mut out = String::with_capacity(r.byte_end - r.byte_start);
        let mut used = 0usize;
        for grapheme in text[r.byte_start..r.byte_end].graphemes(true) {
            let w = grapheme_cells(grapheme, used, self.width);
            if grapheme == "\t" {
                out.push_str(&geom::spaces(w));
            } else if w > 0 {
                out.push_str(grapheme);
            }
            used = used.saturating_add(w);
        }
        out
    }

    /// Grapheme stops of one row: (scalar start, scalar length, start column,
    /// cells).
    fn stops(&self, text: &str, row: &VisualRow) -> Vec<(usize, usize, usize, usize)> {
        let mut out = Vec::new();
        let mut scalar = row.scalar_start;
        let mut used = 0usize;
        for grapheme in text[row.byte_start..row.byte_end].graphemes(true) {
            let w = grapheme_cells(grapheme, used, self.width);
            let len = grapheme.chars().count();
            out.push((scalar, len, used, w));
            scalar += len;
            used = used.saturating_add(w);
        }
        out
    }

    /// Visual `(row, column)` of the scalar source index `cursor`.
    ///
    /// A cursor on a soft-wrap boundary belongs to the following row
    /// (following-row affinity); the end-of-buffer slot after an exactly-full
    /// final row is the trailing blank row. An end-of-line caret on a full
    /// non-final row sits on that row's last glyph cell (never a border).
    #[must_use]
    pub fn cursor(&self, text: &str, cursor: usize) -> (usize, usize) {
        let last = self.rows.len() - 1;
        let mut index = last;
        for (i, row) in self.rows.iter().enumerate() {
            let next_is_slot = self.rows.get(i + 1).is_some_and(|n| n.slot);
            if cursor < row.scalar_end
                || (cursor == row.scalar_end && row.closes_line && !next_is_slot)
            {
                index = i;
                break;
            }
        }
        let row = &self.rows[index];
        if row.slot {
            return (index, 0);
        }
        let stops = self.stops(text, row);
        for &(start, len, col, _) in &stops {
            if cursor < start + len {
                return (index, col);
            }
        }
        if row.cells >= self.width {
            // A full row's end-of-line caret: last grapheme's start column.
            let col = stops.last().map_or(0, |&(_, _, col, _)| col);
            return (index, col);
        }
        (index, row.cells)
    }

    /// Scalar source index at visual `(row, col)`: the grapheme stop whose
    /// cells contain `col`; past the row's end it is the row's end-of-line
    /// position (the last grapheme's start on a soft-wrapped row, whose end
    /// belongs to the next row).
    #[must_use]
    pub fn cursor_at(&self, text: &str, row: usize, col: usize) -> usize {
        let row = row.min(self.rows.len() - 1);
        let Some(r) = self.rows.get(row) else {
            return text.chars().count();
        };
        if r.slot {
            return r.scalar_start;
        }
        let stops = self.stops(text, r);
        for &(start, _, begin, w) in &stops {
            if col < begin + w.max(1) {
                return start;
            }
        }
        let next_is_slot = self.rows.get(row + 1).is_some_and(|next| next.slot);
        if r.closes_line && !next_is_slot {
            r.scalar_end
        } else {
            stops.last().map_or(r.scalar_start, |&(start, ..)| start)
        }
    }

    /// The scroll offset (in visual rows) that shows `cursor_row` inside a
    /// `max_visible`-row window, moving `scroll` as little as possible.
    #[must_use]
    pub fn scroll_to_show(&self, cursor_row: usize, scroll: usize, max_visible: usize) -> usize {
        let max_visible = max_visible.max(1);
        let scroll = scroll.min(self.rows.len().saturating_sub(max_visible));
        if cursor_row >= scroll + max_visible {
            cursor_row + 1 - max_visible
        } else if cursor_row < scroll {
            cursor_row
        } else {
            scroll
        }
    }
}

/// Inner (text) width of a composer rectangle `area_width` cells wide.
#[must_use]
pub fn inner_width(area_width: u16) -> usize {
    usize::from(area_width.saturating_sub(2)).max(1)
}

/// Compute the number of display rows needed for the input area (including borders).
/// Returns at least 3 (1 line + 2 border rows), up to MAX_INPUT_LINES + 2.
///
/// `area_width` is the width of the composer rectangle itself. Rows come from the
/// same whole-grapheme projection the paint uses (DF-060, Story 21.11), so
/// `CCC` in a 3-cell interior needs three rows and an exactly-full final row
/// reserves its caret row.
// Covers: FR16, UX-DR76
pub fn input_area_height(input: &str, area_width: u16) -> u16 {
    let rows = InputGeometry::new(input, inner_width(area_width)).row_count();
    (rows.clamp(1, MAX_INPUT_LINES) as u16) + 2
}

/// The scroll offset (visual rows) that shows the end of `input`: the last
/// `visible` rows of the composer rectangle `area`.
#[must_use]
pub fn tail_scroll(input: &str, area: Rect) -> usize {
    let visible = usize::from(area.height.saturating_sub(2)).max(1);
    InputGeometry::new(input, inner_width(area.width))
        .row_count()
        .saturating_sub(visible)
}

/// Estimate token count from text using character/word heuristic.
/// ~1 token per 4 characters, blended with ~1.3 tokens per word.
// Covers: UX-DR66
pub fn estimate_tokens(text: &str) -> usize {
    let chars = text.chars().count();
    let words = text.split_whitespace().count();
    std::cmp::max(chars / 4, (words as f64 * 1.3) as usize)
}

/// Render the text input area with multi-line support and cursor.
// Covers: FR16, UX-DR76, UX-DR66
pub fn render(
    frame: &mut Frame,
    area: Rect,
    input: &str,
    cursor_pos: usize,
    focus: FocusState,
    theme: &Theme,
    multiline_mode: bool,
    input_scroll_offset: usize,
    image_indicator: Option<&str>,
) {
    let is_focused = focus == FocusState::Input;
    let border_style = if is_focused {
        Style::default().fg(theme.colors.accent)
    } else {
        Style::default().fg(theme.colors.fg_muted)
    };

    // Build title with optional [ML] indicator
    let title = if multiline_mode {
        " Message [ML] "
    } else {
        " Message "
    };

    // Build token estimate as right-aligned title on bottom border
    let token_title = if input.chars().count() > 500 {
        let tokens = estimate_tokens(input);
        format!(" ~{} tokens ", tokens)
    } else {
        String::new()
    };

    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(title);

    // Build bottom border titles: image indicator (left) and token estimate (right)
    // Covers: FR112 (AC1)
    if let Some(indicator) = image_indicator {
        block = block.title_bottom(
            Line::from(format!(" {} ", indicator))
                .style(Style::default().fg(theme.colors.fg_muted)),
        );
    }

    if !token_title.is_empty() {
        block = block.title_bottom(
            Line::from(token_title).right_aligned().style(
                Style::default()
                    .fg(theme.colors.text_hint)
                    .add_modifier(Modifier::ITALIC),
            ),
        );
    }

    // One width-aware projection drives paint, scroll and the caret.
    let geometry = InputGeometry::new(input, inner_width(area.width));
    let max_visible = usize::from(area.height.saturating_sub(2)).max(1);
    let (cursor_row, cursor_col) = geometry.cursor(input, cursor_pos);
    let mut scroll = input_scroll_offset.min(geometry.row_count().saturating_sub(max_visible));
    if is_focused {
        scroll = geometry.scroll_to_show(cursor_row, scroll, max_visible);
    }

    let visible_lines: Vec<Line> = (scroll..geometry.row_count().min(scroll + max_visible))
        .map(|row| Line::from(geometry.row_text(input, row)))
        .collect();

    let widget = Paragraph::new(visible_lines)
        .style(
            Style::default()
                .fg(theme.colors.fg_primary)
                .bg(theme.colors.bg_primary),
        )
        .block(block);
    frame.render_widget(widget, area);

    // Set cursor position (inside the borders, on a glyph's leading cell).
    if is_focused {
        let visible_row = cursor_row.saturating_sub(scroll);
        let inner = area.width.saturating_sub(2);
        let clamped_col = u16::try_from(cursor_col)
            .unwrap_or(u16::MAX)
            .min(inner.saturating_sub(1));
        let clamped_row = u16::try_from(visible_row)
            .unwrap_or(u16::MAX)
            .min(area.height.saturating_sub(3));

        frame.set_cursor_position((
            area.x.saturating_add(clamped_col).saturating_add(1),
            area.y + 1 + clamped_row,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: &str = "👨\u{200d}👩\u{200d}👧";
    const C: &str = "界";
    const A: &str = "e\u{301}";

    #[test]
    fn display_width_height_packs_whole_graphemes_at_odd_inner_widths() {
        // inner width 3: CCC packs one glyph per row (2 cells + gap), not ceil(6/3).
        let ccc = C.repeat(3);
        assert_eq!(input_area_height(&ccc, 5), 5);
        assert_eq!(InputGeometry::new(&ccc, 3).row_count(), 3);
        // ASCII control at the same width.
        assert_eq!(InputGeometry::new("abcdefg", 3).row_count(), 3);
        // G and A are single clusters.
        assert_eq!(InputGeometry::new(&format!("{G}{G}"), 3).row_count(), 2);
        assert_eq!(InputGeometry::new(&A.repeat(2), 3).row_count(), 1);
        assert_eq!(
            InputGeometry::new(&A.repeat(3), 3).row_count(),
            2,
            "full row + caret slot"
        );
        // 30 CJK glyphs in a 60-column composer (inner 58): two rows.
        assert_eq!(input_area_height(&C.repeat(30), 60), 4);
    }

    #[test]
    fn display_width_height_reserves_the_final_caret_slot_and_clamps() {
        // Exactly full final row: the end-of-buffer slot needs a blank row.
        assert_eq!(InputGeometry::new("abc", 3).row_count(), 2);
        assert_eq!(InputGeometry::new("ab", 3).row_count(), 1);
        assert_eq!(
            InputGeometry::new("abc\nd", 3).row_count(),
            2,
            "mid-buffer full row has no slot"
        );
        // Explicit blank lines count.
        assert_eq!(InputGeometry::new("a\n\n\nb", 10).row_count(), 4);
        let long = "line\n".repeat(20);
        assert_eq!(
            usize::from(input_area_height(&long, 80)),
            MAX_INPUT_LINES + 2
        );
        assert_eq!(input_area_height("", 80), 3);
    }

    #[test]
    fn display_width_cursor_has_following_row_affinity_and_slot() {
        let text = "abcdef";
        let geometry = InputGeometry::new(text, 3);
        assert_eq!(geometry.cursor(text, 0), (0, 0));
        assert_eq!(geometry.cursor(text, 2), (0, 2));
        assert_eq!(
            geometry.cursor(text, 3),
            (1, 0),
            "soft-wrap boundary -> following row"
        );
        assert_eq!(geometry.cursor(text, 5), (1, 2));
        assert_eq!(
            geometry.cursor(text, 6),
            (2, 0),
            "end slot is the blank caret row"
        );
        assert_eq!(geometry.row_count(), 3);
        assert_eq!(geometry.cursor_at(text, 2, 0), 6);
        assert_eq!(
            geometry.cursor_at(text, 1, 9),
            5,
            "soft row clamps before its last glyph"
        );
    }

    #[test]
    fn display_width_cursor_never_lands_inside_a_cluster() {
        let text = format!("{G}x\n{G}");
        // scalars: G(0..5) x(5) \n(6) G(7..12)
        let geometry = InputGeometry::new(&text, 20);
        assert_eq!(geometry.cursor(&text, 0), (0, 0));
        assert_eq!(
            geometry.cursor(&text, 3),
            (0, 0),
            "mid-G snaps to the cluster start"
        );
        assert_eq!(geometry.cursor(&text, 5), (0, 2), "after G: 2 cells, not 6");
        assert_eq!(geometry.cursor(&text, 6), (0, 3));
        assert_eq!(geometry.cursor(&text, 7), (1, 0));
        assert_eq!(geometry.cursor(&text, 12), (1, 2));
        // vertical target inside C's two cells and before A's accent
        let cjk = format!("ab\n{C}{C}");
        let g = InputGeometry::new(&cjk, 20);
        assert_eq!(
            g.cursor_at(&cjk, 1, 1),
            3,
            "col 1 is inside the first C: its start"
        );
        assert_eq!(g.cursor_at(&cjk, 1, 2), 4);
        let accent = format!("x{A}\nabc");
        let g = InputGeometry::new(&accent, 20);
        assert_eq!(g.cursor_at(&accent, 0, 1), 1, "the accent belongs to e");
        assert_eq!(
            g.cursor_at(&accent, 0, 2),
            3,
            "past the line end = end of line"
        );
    }

    #[test]
    fn display_width_tabs_use_tab_stops_in_height_paint_and_caret() {
        let text = "a\tb";
        let geometry = InputGeometry::new(text, 20);
        assert_eq!(geometry.row_text(text, 0), "a       b");
        assert_eq!(geometry.cursor(text, 2), (0, 8));
        assert_eq!(geometry.cursor(text, 3), (0, 9));
        // The source keeps the tab; a tab that cannot fit wraps.
        assert_eq!(InputGeometry::new("abcdef\tx", 8).row_count(), 2);
    }

    #[test]
    fn display_width_caret_affinity_at_full_rows_is_pinned() {
        // End on a non-final soft-wrapped row lands on that row's last glyph
        // (its end position belongs to the following row).
        let text = "abcdef";
        let geometry = InputGeometry::new(text, 3);
        assert_eq!(geometry.cursor_at(text, 0, usize::MAX), 2);
        assert_eq!(geometry.cursor(text, 2), (0, 2));
        // A full row that ends in a newline: the end-of-line caret sits on the
        // last glyph of that row, not on a border and not on the next line.
        let text = "abc\nd";
        let geometry = InputGeometry::new(text, 3);
        assert_eq!(geometry.cursor(text, 3), (0, 2));
        assert_eq!(geometry.cursor_at(text, 0, usize::MAX), 3);
        assert_eq!(geometry.cursor(text, 4), (1, 0));
        // The same full row at the very end of the buffer: the caret has the
        // trailing blank slot row.
        let text = "abc";
        let geometry = InputGeometry::new(text, 3);
        assert_eq!(geometry.row_count(), 2);
        assert_eq!(geometry.cursor(text, 3), (1, 0));
        assert_eq!(geometry.cursor_at(text, 0, usize::MAX), 2);
        // A wide last glyph: the caret sits on its leading cell.
        let text = "a界\nb";
        let geometry = InputGeometry::new(text, 3);
        assert_eq!(geometry.cursor(text, 2), (0, 1));
    }
}
