//! Shared fixtures and observers for the 21-11 display-width conformance
//! module. Everything reads the real rendered `Buffer`; nothing here builds an
//! expected row by hand.
#![allow(dead_code)]

use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Position;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Family emoji ZWJ sequence: one grapheme, two cells under the pinned policy.
pub const G: &str = "👨\u{200d}👩\u{200d}👧";
/// Wide CJK glyph: one grapheme, two cells.
pub const C: &str = "界";
/// Combining accent: one grapheme, one cell.
pub const A: &str = "e\u{301}";

/// What a draw closure produced: the painted buffer and the cursor position
/// the frame asked the backend to show (if any).
pub struct Painted {
    pub buffer: Buffer,
    pub cursor: Option<Position>,
}

/// Paint with a real `Terminal<TestBackend>` and read the backend back.
pub fn paint(width: u16, height: u16, draw: impl FnOnce(&mut Frame)) -> Painted {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(draw).unwrap();
    let cursor = terminal
        .get_cursor_position()
        .ok()
        .filter(|p| terminal.backend().cursor_visible() && p.x < width && p.y < height);
    Painted {
        buffer: terminal.backend().buffer().clone(),
        cursor,
    }
}

/// Glyph symbols of one buffer row, continuation cells of wide glyphs dropped.
pub fn glyphs(buffer: &Buffer, y: u16) -> Vec<String> {
    let area = buffer.area;
    let mut out = Vec::new();
    let mut x = area.left();
    while x < area.right() {
        let Some(cell) = buffer.cell(Position::new(x, y)) else {
            break;
        };
        let symbol = cell.symbol();
        out.push(symbol.to_owned());
        x = x.saturating_add(u16::try_from(symbol.width().max(1)).unwrap_or(1));
    }
    out
}

/// One buffer row as text (continuations dropped), trailing blanks trimmed.
pub fn row_text(buffer: &Buffer, y: u16) -> String {
    glyphs(buffer, y).concat().trim_end().to_owned()
}

/// Every buffer row as text, in order.
pub fn rows(buffer: &Buffer) -> Vec<String> {
    (buffer.area.top()..buffer.area.bottom())
        .map(|y| row_text(buffer, y))
        .collect()
}

/// All rows joined by `\n`.
pub fn screen(buffer: &Buffer) -> String {
    rows(buffer).join("\n")
}

/// Display cells occupied by the glyphs of one row (blanks included).
pub fn row_cells(buffer: &Buffer, y: u16) -> usize {
    glyphs(buffer, y).iter().map(|s| s.width().max(1)).sum()
}

/// Assert that no row of the buffer starts or ends inside an extended
/// grapheme cluster (no dangling ZWJ / variation selector / combining mark),
/// which is how a partial `G` or `A` shows up after a scalar clip.
pub fn assert_whole_clusters(buffer: &Buffer) {
    for y in buffer.area.top()..buffer.area.bottom() {
        let text = glyphs(buffer, y).concat();
        let trimmed = text.trim_end_matches(' ');
        assert!(
            !trimmed.ends_with('\u{200d}'),
            "row {y} ends with a dangling ZWJ: {trimmed:?}"
        );
        if let Some(first) = trimmed.chars().next() {
            assert!(
                !matches!(first, '\u{200d}' | '\u{fe0f}' | '\u{0300}'..='\u{036f}'),
                "row {y} starts with a cluster fragment: {trimmed:?}"
            );
        }
        for g in trimmed.graphemes(true) {
            if g.chars().count() > 1 {
                assert!(
                    g == G || g == A || g.width() > 0,
                    "row {y}: unexpected cluster {g:?}"
                );
            }
        }
    }
}

/// Count how many whole `needle` clusters appear in a string of rows.
pub fn count_cluster(haystack: &str, needle: &str) -> usize {
    haystack.graphemes(true).filter(|g| *g == needle).count()
}

/// The visible text of `rows` must not hold a partial `G`: a family member
/// emoji not followed by its joiner chain.
pub fn assert_no_partial_family(text: &str) {
    for g in text.graphemes(true) {
        if g.contains('👨') || g.contains('👩') || g.contains('👧') {
            assert_eq!(g, G, "partial family cluster {g:?} in {text:?}");
        }
    }
}
