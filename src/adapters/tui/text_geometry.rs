//! Shared terminal text geometry (Story 21.11, AD-2114).
//!
//! Four units stay distinct in the TUI and this module owns the conversions
//! between the display-facing ones:
//!
//! * **byte offsets** index into the UTF-8 source and bound payloads;
//! * **scalar indices** are the stored source cursor (`TuiState.cursor_position`);
//! * **grapheme clusters** (extended, `unicode-segmentation`) are the only legal
//!   clip, wrap and cursor stops;
//! * **cells** are terminal columns, measured with the default
//!   `UnicodeWidthStr::width` policy over a whole string, never `width_cjk` and
//!   never a sum of scalar widths.
//!
//! Every primitive returns borrowed slices where it can and never splits a
//! grapheme. Callers keep their own marker, whitespace and style policy; no
//! primitive here changes source text.

use std::borrow::Cow;

use ratatui::text::{Line, Span};

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Display cells of `text` under the pinned string-width policy.
#[inline]
#[must_use]
pub fn cells(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Longest prefix of whole graphemes that fits in `budget` cells.
///
/// A budget of zero returns the empty string. A first grapheme wider than a
/// positive budget is not returned (no partial glyph is ever painted).
#[must_use]
pub fn clip(text: &str, budget: usize) -> &str {
    if budget == 0 {
        return "";
    }
    let mut used = 0usize;
    for (start, grapheme) in text.grapheme_indices(true) {
        let width = cells(grapheme);
        if used.saturating_add(width) > budget {
            return &text[..start];
        }
        used += width;
    }
    text
}

/// Longest suffix of whole graphemes that fits in `budget` cells.
#[must_use]
pub fn clip_tail(text: &str, budget: usize) -> &str {
    if budget == 0 {
        return "";
    }
    let mut used = 0usize;
    for (start, grapheme) in text.grapheme_indices(true).rev() {
        let width = cells(grapheme);
        if used.saturating_add(width) > budget {
            return &text[start + grapheme.len()..];
        }
        used += width;
    }
    text
}

/// Fit `text` into `budget` cells, ending in `marker` when it had to be cut.
///
/// The marker is included in the budget and is emitted only when it fits with
/// at least one cell of content to spare; otherwise the result is a plain
/// whole-grapheme prefix (possibly empty). Text that already fits is returned
/// unchanged and borrowed.
#[must_use]
pub fn ellipsize<'a>(text: &'a str, budget: usize, marker: &str) -> Cow<'a, str> {
    if cells(text) <= budget {
        return Cow::Borrowed(text);
    }
    let marker_cells = cells(marker);
    if budget <= marker_cells {
        return Cow::Borrowed(clip(text, budget));
    }
    let head = clip(text, budget - marker_cells);
    let mut out = String::with_capacity(head.len() + marker.len());
    out.push_str(head);
    out.push_str(marker);
    Cow::Owned(out)
}

/// `n` spaces; one allocation, no per-cell work.
#[inline]
#[must_use]
pub fn spaces(n: usize) -> String {
    " ".repeat(n)
}

/// Pad `text` on the right with spaces to `width` cells. Longer text is
/// returned unchanged (callers clip first when they need a bound).
#[must_use]
pub fn pad_right(text: &str, width: usize) -> String {
    let used = cells(text);
    let mut out = String::with_capacity(text.len() + width.saturating_sub(used));
    out.push_str(text);
    out.extend(std::iter::repeat_n(' ', width.saturating_sub(used)));
    out
}

/// Pad `text` on the left with spaces to `width` cells.
#[must_use]
pub fn pad_left(text: &str, width: usize) -> String {
    let used = cells(text);
    let mut out = String::with_capacity(text.len() + width.saturating_sub(used));
    out.extend(std::iter::repeat_n(' ', width.saturating_sub(used)));
    out.push_str(text);
    out
}

/// Clip to `width` cells (no marker) and pad the remainder with spaces so the
/// result is exactly `width` cells unless one wide grapheme leaves a one-cell
/// gap that is also padded.
#[must_use]
pub fn fit_exact(text: &str, width: usize) -> String {
    pad_right(clip(text, width), width)
}

/// One physical row of a hard (grapheme-packed) wrap, as a byte range of the
/// source plus its cell width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackedRow {
    pub start: usize,
    pub end: usize,
    pub cells: usize,
}

/// Pack the graphemes of a newline-free `text` into rows of at most `width`
/// cells. A grapheme wider than `width` is kept whole as its own oversized
/// row (A3), so the iteration always makes progress. `width == 0` behaves as
/// one cell. An empty string yields one empty row.
#[must_use]
pub fn pack_rows(text: &str, width: usize) -> Vec<PackedRow> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut start = 0usize;
    let mut used = 0usize;
    let mut end = 0usize;
    for (at, grapheme) in text.grapheme_indices(true) {
        let w = cells(grapheme);
        if used > 0 && used + w > width {
            rows.push(PackedRow {
                start,
                end,
                cells: used,
            });
            start = at;
            used = 0;
        }
        used += w;
        end = at + grapheme.len();
    }
    rows.push(PackedRow {
        start,
        end,
        cells: used,
    });
    rows
}

/// [`pack_rows`] projected to borrowed slices.
#[must_use]
pub fn wrap_hard(text: &str, width: usize) -> Vec<&str> {
    pack_rows(text, width)
        .into_iter()
        .map(|row| &text[row.start..row.end])
        .collect()
}

/// Scalar index of the grapheme boundary strictly before `index`, or 0.
#[must_use]
pub fn prev_boundary(text: &str, index: usize) -> usize {
    let mut previous = 0usize;
    let mut scalars = 0usize;
    for grapheme in text.graphemes(true) {
        if scalars >= index {
            return previous;
        }
        previous = scalars;
        scalars += grapheme.chars().count();
    }
    previous.min(index)
}

/// Scalar index of the grapheme boundary strictly after `index`, or the end.
#[must_use]
pub fn next_boundary(text: &str, index: usize) -> usize {
    let mut scalars = 0usize;
    for grapheme in text.graphemes(true) {
        scalars += grapheme.chars().count();
        if scalars > index {
            return scalars;
        }
    }
    scalars
}

/// Snap a scalar index back to the start of the grapheme containing it.
#[must_use]
pub fn snap_boundary(text: &str, index: usize) -> usize {
    let mut scalars = 0usize;
    for grapheme in text.graphemes(true) {
        let next = scalars + grapheme.chars().count();
        if next > index {
            return scalars;
        }
        scalars = next;
    }
    scalars
}

/// Byte offset of the scalar index `index` (clamped to `text.len()`).
#[must_use]
pub fn byte_of_scalar(text: &str, index: usize) -> usize {
    text.char_indices()
        .nth(index)
        .map_or(text.len(), |(byte, _)| byte)
}

/// Remove the last grapheme cluster of `text`; returns whether one was removed.
pub fn pop_grapheme(text: &mut String) -> bool {
    let Some((start, _)) = text.grapheme_indices(true).next_back() else {
        return false;
    };
    text.truncate(start);
    true
}

/// Expand the byte range `[start, end)` outward to whole-grapheme bounds.
#[must_use]
pub fn expand_to_graphemes(text: &str, start: usize, end: usize) -> (usize, usize) {
    let mut lo = start;
    let mut hi = end;
    for (at, grapheme) in text.grapheme_indices(true) {
        let stop = at + grapheme.len();
        if stop <= start {
            continue;
        }
        if at >= end {
            break;
        }
        lo = lo.min(at);
        hi = hi.max(stop);
    }
    (lo, hi)
}

/// Display tab stop shared by the composer and the markdown renderer (tabs in
/// code, HTML blocks and literal text expand to the next multiple of this).
pub const TAB_STOP: usize = 8;

/// `text` with every tab expanded to the next [`TAB_STOP`] boundary (the first
/// cell of `text` is column `start`) and every other control character
/// dropped. Control characters are not painted by the terminal backend, so
/// keeping them would make measured and painted widths disagree. Text with no
/// control character is returned borrowed.
#[must_use]
pub fn expand_tabs(text: &str, start: usize) -> Cow<'_, str> {
    if !text.chars().any(char::is_control) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + TAB_STOP);
    let mut column = start;
    for grapheme in text.graphemes(true) {
        if grapheme == "\t" {
            let advance = TAB_STOP - column % TAB_STOP;
            out.extend(std::iter::repeat_n(' ', advance));
            column += advance;
        } else if grapheme.chars().next().is_some_and(char::is_control) {
            continue;
        } else {
            out.push_str(grapheme);
            column += cells(grapheme);
        }
    }
    Cow::Owned(out)
}

/// The part of `line` that spans display cells `from_cell..from_cell + width`:
/// the only horizontal slicer. Span styles are kept; a wide grapheme cut by
/// either edge becomes spaces for its visible cells, so no half glyph is ever
/// painted. Rows shorter than the window are returned unpadded.
#[must_use]
pub fn slice_line(line: &Line<'_>, from_cell: usize, width: usize) -> Line<'static> {
    let end = from_cell.saturating_add(width);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut col = 0usize;
    for span in &line.spans {
        let mut text = String::new();
        for grapheme in span.content.graphemes(true) {
            if col >= end {
                break;
            }
            let cluster = cells(grapheme);
            if cluster == 0 {
                if col >= from_cell {
                    text.push_str(grapheme);
                }
                continue;
            }
            let next = col + cluster;
            if next > from_cell {
                if col >= from_cell && next <= end {
                    text.push_str(grapheme);
                } else {
                    text.push_str(&spaces(next.min(end) - col.max(from_cell)));
                }
            }
            col = next;
        }
        if !text.is_empty() {
            spans.push(Span::styled(text, span.style));
        }
    }
    let mut sliced = Line::from(spans).style(line.style);
    sliced.alignment = line.alignment;
    sliced
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: &str = "👨\u{200d}👩\u{200d}👧";
    const C: &str = "界";
    const A: &str = "e\u{301}";

    #[test]
    fn display_width_policy_is_string_width_not_scalar_sum() {
        assert_eq!(cells(G), 2, "ZWJ family is one two-cell cluster");
        assert_eq!(cells(C), 2);
        assert_eq!(cells(A), 1);
        let scalar_sum: usize = G
            .chars()
            .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
            .sum();
        assert_eq!(scalar_sum, 6, "the scalar sum is the wrong authority");
    }

    #[test]
    fn display_width_clip_returns_whole_graphemes_only() {
        for (text, budget, want) in [
            ("abc", 2, "ab"),
            ("abc", 0, ""),
            ("abc", 3, "abc"),
            ("abc", 9, "abc"),
            ("界界界", 3, "界"),
            ("界界界", 4, "界界"),
            ("界界界", 1, ""),
            ("e\u{301}x", 1, "e\u{301}"),
        ] {
            assert_eq!(clip(text, budget), want, "{text:?} @ {budget}");
        }
        let text = format!("{G}xx");
        assert_eq!(clip(&text, 3), format!("{G}x"));
        assert_eq!(clip(&text, 1), "", "a lone wide cluster is not split");
        assert_eq!(clip(&text, 2), G);
        assert_eq!(clip(&text, 0), "");
    }

    #[test]
    fn display_width_clip_tail_keeps_the_end_whole() {
        let text = format!("xx{G}");
        assert_eq!(clip_tail(&text, 2), G);
        assert_eq!(clip_tail(&text, 3), text.as_str()[1..].to_string());
        assert_eq!(clip_tail("abc", 2), "bc");
        assert_eq!(clip_tail("abc", 0), "");
        assert_eq!(clip_tail("界界", 3), "界");
    }

    #[test]
    fn display_width_ellipsize_budgets_the_marker_and_never_emits_it_outside() {
        assert_eq!(ellipsize("hello", 5, "…"), "hello");
        assert_eq!(ellipsize("hello world", 6, "…"), "hello…");
        assert_eq!(ellipsize("hello world", 6, "..."), "hel...");
        assert_eq!(ellipsize("hello world", 0, "…"), "");
        assert_eq!(ellipsize("hello world", 1, "…"), "h");
        assert_eq!(
            ellipsize("hello world", 3, "..."),
            "hel",
            "no room beyond marker"
        );
        let text = format!("{G}xx");
        let fitted = ellipsize(&text, 3, "…");
        assert_eq!(fitted, format!("{G}…"));
        assert!(cells(&fitted) <= 3);
        let tight = ellipsize(&text, 2, "…");
        assert_eq!(
            tight,
            "…".to_string(),
            "G leaves no room beside the marker, G is not split"
        );
        assert!(cells(&tight) <= 2);
        let cjk = "界".repeat(10);
        for budget in 0..22 {
            let out = ellipsize(&cjk, budget, "…");
            assert!(cells(&out) <= budget, "budget {budget}: {out:?}");
        }
    }

    #[test]
    fn display_width_padding_uses_cells() {
        assert_eq!(pad_right(C, 4), "界  ");
        assert_eq!(pad_right(A, 3), "e\u{301}  ");
        assert_eq!(pad_right(G, 2), G);
        assert_eq!(pad_left(C, 4), "  界");
        assert_eq!(fit_exact("界界界", 5), "界界 ");
        assert_eq!(cells(&fit_exact(&format!("{G}xx"), 3)), 3);
        assert_eq!(cells(&fit_exact("abc", 7)), 7);
    }

    #[test]
    fn display_width_pack_rows_never_splits_and_always_progresses() {
        assert_eq!(
            wrap_hard("CCC".replace('C', C).as_str(), 3),
            ["界", "界", "界"]
        );
        assert_eq!(wrap_hard("界界界", 4), ["界界", "界"]);
        assert_eq!(wrap_hard("abcde", 2), ["ab", "cd", "e"]);
        assert_eq!(wrap_hard("", 5), [""]);
        let text = format!("{G}xx");
        assert_eq!(wrap_hard(&text, 3), [format!("{G}x"), "x".to_string()]);
        assert_eq!(wrap_hard("e\u{301}e\u{301}", 1), ["e\u{301}", "e\u{301}"]);
        // A3: a grapheme wider than the row stays one oversized row, and the
        // wrap terminates with every source byte present.
        let double = format!("{G}{G}");
        assert_eq!(wrap_hard(&double, 1), [G, G]);
        assert_eq!(wrap_hard("界界", 0), ["界", "界"]);
        for width in 1..8 {
            let source = format!("a{C}b{A}c{G}d");
            let rows = wrap_hard(&source, width);
            assert_eq!(rows.concat(), source);
            for row in rows {
                let oversized = row.graphemes(true).count() == 1;
                assert!(cells(row) <= width || oversized, "{row:?} @ {width}");
            }
        }
    }

    #[test]
    fn display_width_boundaries_are_scalar_indexed_and_grapheme_aligned() {
        let text = format!("a{G}{A}b");
        // scalars: a(0) G(1..6) A(6..8) b(8)
        assert_eq!(next_boundary(&text, 0), 1);
        assert_eq!(next_boundary(&text, 1), 6);
        assert_eq!(next_boundary(&text, 6), 8);
        assert_eq!(next_boundary(&text, 8), 9);
        assert_eq!(next_boundary(&text, 9), 9);
        assert_eq!(prev_boundary(&text, 9), 8);
        assert_eq!(prev_boundary(&text, 8), 6);
        assert_eq!(prev_boundary(&text, 6), 1);
        assert_eq!(prev_boundary(&text, 1), 0);
        assert_eq!(prev_boundary(&text, 0), 0);
        assert_eq!(snap_boundary(&text, 3), 1);
        assert_eq!(snap_boundary(&text, 7), 6);
        assert_eq!(snap_boundary(&text, 6), 6);
        assert_eq!(snap_boundary(&text, 99), 9);
        assert_eq!(byte_of_scalar("a界b", 2), 4);
        assert_eq!(byte_of_scalar("a界b", 99), 5);
    }

    #[test]
    fn display_width_expand_to_graphemes_covers_the_whole_cluster() {
        let text = format!("x{G}y");
        let inside = (1, 1 + "👨".len());
        assert_eq!(
            expand_to_graphemes(&text, inside.0, inside.1),
            (1, 1 + G.len())
        );
        let accent = "xe\u{301}y";
        assert_eq!(expand_to_graphemes(accent, 1, 2), (1, 4));
        assert_eq!(expand_to_graphemes("abc", 1, 2), (1, 2));
        assert_eq!(expand_to_graphemes("abc", 0, 3), (0, 3));
    }

    #[test]
    fn display_width_expand_tabs_stops_at_eight_and_drops_other_controls() {
        assert!(matches!(expand_tabs("plain 界", 0), Cow::Borrowed(_)));
        assert_eq!(expand_tabs("\tx", 0), "        x");
        assert_eq!(expand_tabs("ab\tx", 0), "ab      x");
        assert_eq!(expand_tabs("12345678\tx", 0), "12345678        x");
        assert_eq!(expand_tabs("\tx", 3), "     x");
        assert_eq!(expand_tabs("界\tx", 0), "界      x");
        assert_eq!(expand_tabs("a\u{7}b\u{1b}[c\r\n", 0), "ab[c");
        for text in ["\t\t", "a\tb\tc", &format!("{G}\t{A}\t")] {
            let out = expand_tabs(text, 0);
            assert!(!out.chars().any(char::is_control));
        }
        assert_eq!(cells(&expand_tabs(&format!("{G}\tx"), 0)), 9);
    }

    #[test]
    fn story21_13_slice_line_never_paints_half_a_glyph_and_keeps_styles() {
        use ratatui::style::{Color, Style};
        let plain = |line: &Line<'_>| {
            line.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        };
        let wide = Line::from(vec![
            Span::styled("ab", Style::default().fg(Color::Red)),
            Span::raw(format!("{C}{C}{G}{A}z")),
        ]);
        // cells: a0 b1 界2-3 界4-5 G6-7 é8 z9
        assert_eq!(
            plain(&slice_line(&wide, 0, 3)),
            "ab ",
            "cut right edge blanks"
        );
        assert_eq!(
            plain(&slice_line(&wide, 3, 3)),
            format!(" {C}"),
            "cut left edge blanks"
        );
        assert_eq!(plain(&slice_line(&wide, 6, 3)), format!("{G}{A}"));
        assert_eq!(
            plain(&slice_line(&wide, 7, 3)),
            format!(" {A}z"),
            "a ZWJ cluster is cut as one"
        );
        assert_eq!(plain(&slice_line(&wide, 9, 8)), "z");
        assert_eq!(
            plain(&slice_line(&wide, 40, 8)),
            "",
            "past the end is empty"
        );
        assert_eq!(slice_line(&wide, 0, 2).spans[0].style.fg, Some(Color::Red));
        for from in 0..12 {
            for width in 0..12 {
                assert!(cells(&plain(&slice_line(&wide, from, width))) <= width);
            }
        }
    }
}
