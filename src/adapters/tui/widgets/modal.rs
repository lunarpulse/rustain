//! Shared modal geometry and height limiting for the model picker and the
//! sign-in overlay, so neither loses its borders, header, rows or footer on a
//! small terminal.

use ratatui::prelude::*;
use unicode_width::UnicodeWidthStr;

/// One part of a modal body, in display order.
pub(crate) enum ModalPart {
    /// Kept whole while anything fits; at worst only the first header line.
    Header(Vec<Line<'static>>),
    /// Shrinks to one line ending in `…`, then is dropped (oldest first),
    /// only while the rows would otherwise get fewer than `min(3, rows)` lines.
    Notice(Vec<Line<'static>>),
    /// The scrolling list; `selected` is the selected entry's line range.
    Rows {
        lines: Vec<Line<'static>>,
        selected: (usize, usize),
    },
    /// Always kept, last.
    Footer(Vec<Line<'static>>),
}

/// Centers a `width`×`height` rectangle in `area`, clamped to it.
pub(crate) fn modal_area(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let x = area.x + (area.width - width) / 2;
    let y = area.y + (area.height - height) / 2;
    Rect::new(x, y, width, height)
}

/// One line ending in `…`, at most `width` cells.
fn ellipsize(lines: &[Line<'static>], width: usize) -> Line<'static> {
    let first = lines.first().cloned().unwrap_or_default();
    let style = first
        .spans
        .first()
        .map(|span| span.style)
        .unwrap_or_default();
    let text: String = first
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    let mut out = String::new();
    for ch in text.chars() {
        if (out.as_str().width() + ch.to_string().as_str().width()) + 1 > width.max(1) {
            break;
        }
        out.push(ch);
    }
    out.push('…');
    Line::from(Span::styled(out, style))
}

/// Lays `parts` into at most `height` lines of `width` cells: headers and the
/// footer stay, the rows keep at least `min(3, rows)` lines (never fewer than
/// one) by shrinking then dropping notices, and the rows scroll so the
/// selected entry stays visible. With no rows at all, notices still give way
/// (oldest first) until everything fits.
pub(crate) fn fit_modal(parts: Vec<ModalPart>, height: usize, width: usize) -> Vec<Line<'static>> {
    enum Slot {
        Fixed(Vec<Line<'static>>),
        Notice(Vec<Line<'static>>),
        Rows,
    }
    let mut slots = Vec::new();
    let mut footer = Vec::new();
    let mut rows = Vec::new();
    let mut selected = (0, 0);
    for part in parts {
        match part {
            ModalPart::Header(lines) => slots.push(Slot::Fixed(lines)),
            ModalPart::Notice(lines) if !lines.is_empty() => slots.push(Slot::Notice(lines)),
            ModalPart::Notice(_) => {}
            ModalPart::Rows {
                lines,
                selected: range,
            } => {
                rows = lines;
                selected = range;
                slots.push(Slot::Rows);
            }
            ModalPart::Footer(lines) => footer.extend(lines),
        }
    }
    let used = |slots: &[Slot]| -> usize {
        slots
            .iter()
            .map(|slot| match slot {
                Slot::Fixed(lines) | Slot::Notice(lines) => lines.len(),
                Slot::Rows => 0,
            })
            .sum::<usize>()
            + footer.len()
    };
    let wanted = rows.len().min(3);
    // 1. Shrink notices to one line, oldest first.
    for index in 0..slots.len() {
        if used(&slots) + wanted <= height {
            break;
        }
        if let Slot::Notice(lines) = &mut slots[index]
            && lines.len() > 1
        {
            *lines = vec![ellipsize(lines, width)];
        }
    }
    // 2. Drop notices, oldest first.
    for index in 0..slots.len() {
        if used(&slots) + wanted <= height {
            break;
        }
        if let Slot::Notice(lines) = &mut slots[index] {
            lines.clear();
        }
    }
    // 3. Last resort: trim headers to their first line, last header first.
    for index in (0..slots.len()).rev() {
        if used(&slots) + rows.len().min(1) <= height {
            break;
        }
        if let Slot::Fixed(lines) = &mut slots[index] {
            lines.truncate(1);
        }
    }
    let row_count = height.saturating_sub(used(&slots));
    // Show the selected entry's last line, but never scroll past its first.
    let start = selected.1.saturating_sub(row_count).min(selected.0);
    let mut out = Vec::new();
    for slot in slots {
        match slot {
            Slot::Fixed(lines) | Slot::Notice(lines) => out.extend(lines),
            Slot::Rows => out.extend(rows.iter().skip(start).take(row_count).cloned()),
        }
    }
    // Never more than `height`: the footer survives any overflow.
    out.truncate(height.saturating_sub(footer.len()));
    out.extend(footer.into_iter().take(height));
    out
}

/// Word-wrap text by terminal cells; oversized words use grapheme-safe wrapping.
pub(crate) fn wrapped(text: &str, style: Style, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for logical in text.split('\n') {
        let mut line = String::new();
        let mut cells = 0;
        for word in logical.split_whitespace() {
            let word_cells = word.width();
            if cells > 0 && cells + 1 + word_cells > width {
                lines.push(Line::from(Span::styled(std::mem::take(&mut line), style)));
                cells = 0;
            }
            if word_cells > width {
                let (parts, _) = super::delimited_body::wrap_text(word, width, usize::MAX);
                lines.extend(
                    parts
                        .into_iter()
                        .map(|part| Line::from(Span::styled(part, style))),
                );
            } else {
                if cells > 0 {
                    line.push(' ');
                    cells += 1;
                }
                line.push_str(word);
                cells += word_cells;
            }
        }
        if !line.is_empty() || logical.is_empty() {
            lines.push(Line::from(Span::styled(line, style)));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(prefix: &str, n: usize) -> Vec<Line<'static>> {
        (0..n).map(|i| Line::from(format!("{prefix}{i}"))).collect()
    }
    fn text(out: &[Line<'static>]) -> Vec<String> {
        out.iter().map(|line| line.to_string()).collect()
    }

    #[test]
    fn everything_fits_unchanged() {
        let out = fit_modal(
            vec![
                ModalPart::Header(lines("h", 2)),
                ModalPart::Rows {
                    lines: lines("r", 3),
                    selected: (0, 1),
                },
                ModalPart::Notice(lines("n", 2)),
                ModalPart::Footer(lines("f", 1)),
            ],
            20,
            30,
        );
        assert_eq!(text(&out), ["h0", "h1", "r0", "r1", "r2", "n0", "n1", "f0"]);
    }

    #[test]
    fn notices_shrink_then_drop_oldest_first_to_keep_three_rows() {
        let parts = || {
            vec![
                ModalPart::Header(lines("h", 1)),
                ModalPart::Notice(lines("a", 3)),
                ModalPart::Rows {
                    lines: lines("r", 10),
                    selected: (9, 10),
                },
                ModalPart::Notice(lines("b", 3)),
                ModalPart::Footer(lines("f", 1)),
            ]
        };
        // 1 + 3 + 3 + 1 = 8 fixed; height 9 leaves 1 row → shrink the oldest notice.
        let out = fit_modal(parts(), 9, 30);
        assert_eq!(
            text(&out),
            ["h0", "a0…", "r7", "r8", "r9", "b0", "b1", "b2", "f0"]
        );
        // Height 6: shrinking leaves 2 rows → drop the oldest notice.
        let out = fit_modal(parts(), 6, 30);
        assert_eq!(text(&out), ["h0", "r7", "r8", "r9", "b0…", "f0"]);
        assert!(fit_modal(parts(), 4, 30).len() <= 4);
    }

    #[test]
    fn footer_and_first_header_survive_any_height() {
        let out = fit_modal(
            vec![
                ModalPart::Header(lines("h", 4)),
                ModalPart::Rows {
                    lines: lines("r", 5),
                    selected: (2, 3),
                },
                ModalPart::Footer(lines("f", 1)),
            ],
            3,
            30,
        );
        assert_eq!(text(&out), ["h0", "r2", "f0"]);
    }

    #[test]
    fn without_rows_notices_still_give_way_and_the_latest_survives() {
        let parts = || {
            vec![
                ModalPart::Notice(lines("a", 3)),
                ModalPart::Header(lines("h", 1)),
                ModalPart::Notice(lines("p", 1)),
                ModalPart::Footer(lines("f", 1)),
            ]
        };
        // 3 + 1 + 1 + 1 = 6 lines: the oldest notice shrinks first.
        assert_eq!(text(&fit_modal(parts(), 4, 30)), ["a0…", "h0", "p0", "f0"]);
        // Then it is dropped; the latest notice (live progress) stays.
        assert_eq!(text(&fit_modal(parts(), 3, 30)), ["h0", "p0", "f0"]);
    }

    #[test]
    fn modal_area_is_centered_and_clamped() {
        let area = Rect::new(0, 0, 60, 16);
        assert_eq!(modal_area(area, 40, 12), Rect::new(10, 2, 40, 12));
        assert_eq!(modal_area(area, 100, 40), area);
    }
}
