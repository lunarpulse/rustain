//! Table layout (D10): a box-drawn grid, one physical row per table row.
//!
//! Every width decision goes through `text_geometry` (AD-2114): columns are
//! measured in cells, cut cells end in `…` at a grapheme boundary, and the grid
//! never exceeds the pane. When even the minimum column widths do not fit, the
//! table collapses to a single muted summary line (no key, KG-2).

use std::borrow::Cow;

use ratatui::prelude::*;

use super::layout::clean_prose;
use super::parse::Align;
use super::transform::{StyledInline, StyledTable};
use crate::adapters::tui::text_geometry as geom;

/// Columns never shrink below this many cells.
const MIN_COL: usize = 3;
/// Marker for a cut cell.
const ELLIPSIS: &str = "…";

/// One cell's styled runs, control characters already dropped.
struct Cell<'a> {
    runs: Vec<(Cow<'a, str>, Style)>,
    cells: usize,
}

impl<'a> Cell<'a> {
    fn new(inlines: &'a [StyledInline]) -> Self {
        let runs: Vec<(Cow<'a, str>, Style)> = inlines
            .iter()
            .map(|inline| match inline {
                StyledInline::Text(span) => (clean_prose(&span.content), span.style),
                StyledInline::SoftBreak(style) => (Cow::Borrowed(" "), *style),
                StyledInline::HardBreak => (Cow::Borrowed(" "), Style::default()),
            })
            .collect();
        let cells = runs.iter().map(|(text, _)| geom::cells(text)).sum();
        Self { runs, cells }
    }

    /// The runs cut to `budget` cells (`…` when anything was cut) and the
    /// cells they occupy.
    fn fit(&self, budget: usize) -> (Vec<Span<'static>>, usize) {
        if self.cells <= budget {
            let spans = self
                .runs
                .iter()
                .map(|(text, style)| Span::styled(text.to_string(), *style))
                .collect();
            return (spans, self.cells);
        }
        let mut left = budget.saturating_sub(geom::cells(ELLIPSIS));
        let mut used = 0usize;
        let mut spans = Vec::new();
        let mut last = Style::default();
        for (text, style) in &self.runs {
            let kept = geom::clip(text, left);
            if !kept.is_empty() {
                spans.push(Span::styled(kept.to_owned(), *style));
                last = *style;
            }
            let kept_cells = geom::cells(kept);
            used += kept_cells;
            left -= kept_cells;
            if kept.len() < text.len() {
                break;
            }
        }
        spans.push(Span::styled(ELLIPSIS, last));
        (spans, used + geom::cells(ELLIPSIS))
    }
}

/// Column widths in cells for `natural` widths that do not fit `budget`
/// (`budget >= natural.len() * MIN_COL`): columns at or below [`MIN_COL`] keep
/// their width; the others share what is left in proportion to their natural
/// width, rounded by largest remainder, never below [`MIN_COL`].
fn shrink(natural: &[usize], budget: usize) -> Vec<usize> {
    let mut widths = natural.to_vec();
    let mut flexible: Vec<usize> = (0..natural.len())
        .filter(|&i| natural[i] > MIN_COL)
        .collect();
    loop {
        let fixed: usize = (0..natural.len())
            .filter(|i| !flexible.contains(i))
            .map(|i| widths[i])
            .sum();
        let available = budget.saturating_sub(fixed);
        let total: usize = flexible.iter().map(|&i| natural[i]).sum();
        let below: Vec<usize> = flexible
            .iter()
            .copied()
            .filter(|&i| available * natural[i] / total.max(1) < MIN_COL)
            .collect();
        if below.is_empty() {
            let mut shares: Vec<(usize, usize)> = Vec::with_capacity(flexible.len());
            let mut given = 0usize;
            for &i in &flexible {
                let product = available * natural[i];
                widths[i] = (product / total.max(1)).min(natural[i]);
                given += widths[i];
                shares.push((i, product % total.max(1)));
            }
            shares.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            for (i, _) in shares {
                if given >= available {
                    break;
                }
                if widths[i] < natural[i] {
                    widths[i] += 1;
                    given += 1;
                }
            }
            return widths;
        }
        for i in below {
            widths[i] = MIN_COL;
            flexible.retain(|&f| f != i);
        }
    }
}

fn rule(left: &str, mid: &str, right: &str, widths: &[usize], style: Style) -> Line<'static> {
    let mut text = String::from(left);
    for (i, width) in widths.iter().enumerate() {
        if i > 0 {
            text.push_str(mid);
        }
        text.push_str(&"─".repeat(width + 2));
    }
    text.push_str(right);
    Line::from(Span::styled(text, style))
}

fn row_line(
    cells: &[Cell<'_>],
    aligns: &[Align],
    widths: &[usize],
    border: Style,
) -> Line<'static> {
    let empty = Cell {
        runs: Vec::new(),
        cells: 0,
    };
    let mut spans = vec![Span::styled("│", border)];
    for (column, &width) in widths.iter().enumerate() {
        let (content, used) = cells.get(column).unwrap_or(&empty).fit(width);
        let free = width - used.min(width);
        let before = match aligns.get(column).copied().unwrap_or(Align::Left) {
            Align::Left => 0,
            Align::Center => free / 2,
            Align::Right => free,
        };
        spans.push(Span::raw(geom::spaces(1 + before)));
        spans.extend(content);
        spans.push(Span::raw(geom::spaces(free - before + 1)));
        spans.push(Span::styled("│", border));
    }
    Line::from(spans)
}

/// Lay `table` out at `width` cells.
#[must_use]
pub fn render(
    table: &StyledTable,
    width: usize,
    border: Style,
    muted: Style,
) -> Vec<Line<'static>> {
    let columns = table.aligns.len();
    if columns == 0 {
        return Vec::new();
    }
    let head: Vec<Cell<'_>> = table.head.iter().map(|c| Cell::new(c)).collect();
    let body: Vec<Vec<Cell<'_>>> = table
        .rows
        .iter()
        .map(|row| row.iter().map(|c| Cell::new(c)).collect())
        .collect();
    let mut natural = vec![0usize; columns];
    for row in std::iter::once(&head).chain(&body) {
        for (column, cell) in row.iter().enumerate().take(columns) {
            natural[column] = natural[column].max(cell.cells);
        }
    }
    // Chrome: one border per column plus the closing one, and a pad cell each side.
    let chrome = 3 * columns + 1;
    let widths = if natural.iter().sum::<usize>() + chrome <= width {
        natural
    } else if columns * MIN_COL + chrome <= width {
        shrink(&natural, width - chrome)
    } else {
        let summary = format!("table · {columns} cols × {} rows", body.len());
        let shown = geom::ellipsize(&summary, width, ELLIPSIS);
        return vec![Line::from(Span::styled(shown.into_owned(), muted))];
    };
    let mut lines = Vec::with_capacity(body.len() + 4);
    lines.push(rule("┌", "┬", "┐", &widths, border));
    lines.push(row_line(&head, &table.aligns, &widths, border));
    lines.push(rule("├", "┼", "┤", &widths, border));
    for row in &body {
        lines.push(row_line(row, &table.aligns, &widths, border));
    }
    lines.push(rule("└", "┴", "┘", &widths, border));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::markdown::{RenderOptions, render as render_markdown};
    use crate::adapters::tui::theme::Theme;

    fn rows(markdown: &str, width: usize) -> Vec<String> {
        render_markdown(markdown, width, &Theme::dark(), &RenderOptions::completed())
            .lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn shrink_keeps_small_columns_and_shares_the_rest_by_largest_remainder() {
        // 10 and 48 share 33 cells: 5.69 and 27.31 → the larger remainder wins.
        assert_eq!(shrink(&[10, 48], 33), [6, 27]);
        // A column at or under MIN_COL keeps its natural width.
        assert_eq!(shrink(&[2, 50, 50], 20), [2, 9, 9]);
        // A share below MIN_COL is raised to MIN_COL and the rest re-shared.
        assert_eq!(shrink(&[4, 1000], 12), [3, 9]);
        // Never more than the budget, never below the minimum.
        for budget in 9..40 {
            let widths = shrink(&[7, 30, 4, 100], budget.max(12));
            assert!(widths.iter().sum::<usize>() <= budget.max(12), "{widths:?}");
            assert!(widths.iter().all(|&w| w >= MIN_COL), "{widths:?}");
        }
    }

    #[test]
    fn a_cell_cut_at_a_wide_grapheme_still_ends_in_one_ellipsis_and_fits() {
        let rows = rows("| a |\n|---|\n| 界界界界界界 |", 9);
        assert_eq!(rows[3], "│ 界界… │");
        assert!(rows.iter().all(|r| geom::cells(r) <= 9));
    }

    #[test]
    fn control_characters_in_cells_never_reach_a_span() {
        let rows = rows("| a\u{1b}[31m\u{7} |\n|---|\n| x\ty\u{9b} |", 40);
        for row in &rows {
            assert!(!row.chars().any(char::is_control), "{row:?}");
        }
        assert_eq!(rows[1], "│ a[31m │");
        assert_eq!(rows[3], "│ x y   │");
    }
}
