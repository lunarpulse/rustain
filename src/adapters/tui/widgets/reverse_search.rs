use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::adapters::tui::state::ReverseSearchState;
use crate::adapters::tui::text_geometry as geom;
use crate::adapters::tui::theme::Theme;

/// Maximum visible matches in the reverse search overlay.
const MAX_VISIBLE_MATCHES: usize = 5;

/// Render the reverse search overlay above the input box.
// Covers: UX-DR74 (reverse search overlay)
pub fn render(frame: &mut Frame, area: Rect, state: &ReverseSearchState, theme: &Theme) {
    if !state.active {
        return;
    }

    let mut lines: Vec<Line> = Vec::new();

    // Query line
    let query_prefix = "(reverse-search): ";
    let query_prefix_cells = geom::cells(query_prefix);
    let inner_width = usize::from(area.width).saturating_sub(2); // borders
    // One cell stays free after the query for the caret (never on the border).
    let shown_query = geom::clip_tail(
        &state.query,
        inner_width.saturating_sub(query_prefix_cells + 1),
    );
    lines.push(Line::from(vec![
        Span::styled(
            query_prefix,
            Style::default()
                .fg(theme.colors.fg_muted)
                .add_modifier(Modifier::ITALIC),
        ),
        Span::styled(
            shown_query.to_string(),
            Style::default()
                .fg(theme.colors.fg_primary)
                .add_modifier(Modifier::BOLD),
        ),
    ]));

    // Match lines
    if state.matches.is_empty() && !state.query.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no matches",
            Style::default().fg(theme.colors.fg_muted),
        )));
    } else {
        let scroll = state.selected_match.saturating_sub(MAX_VISIBLE_MATCHES - 1);
        for (i, (_idx, entry)) in state
            .matches
            .iter()
            .skip(scroll)
            .take(MAX_VISIBLE_MATCHES)
            .enumerate()
        {
            let display_idx = i + scroll;
            let prefix = if display_idx == state.selected_match {
                " ▸ "
            } else {
                "   "
            };
            let style = if display_idx == state.selected_match {
                Style::default().fg(theme.colors.accent)
            } else {
                Style::default().fg(theme.colors.fg_secondary)
            };
            // Truncate entry to fit by display cells on a whole-grapheme
            // boundary, with the `…` inside the budget. The budget is the
            // real content width: inside the borders, after the row prefix
            // and between the two quote marks.
            let max_len = inner_width.saturating_sub(geom::cells(prefix) + 2);
            let display = geom::ellipsize(entry, max_len, "…");
            lines.push(Line::from(vec![
                Span::styled(prefix.to_string(), style),
                Span::styled(format!("\"{}\"", display), style),
            ]));
        }
    }

    let height = lines.len() as u16 + 2; // +2 for borders
    let overlay_height = height.min(area.height);
    let overlay_area = Rect {
        x: area.x,
        y: area.y + area.height - overlay_height,
        width: area.width,
        height: overlay_height,
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.colors.accent))
        .title(" Reverse Search ");

    let widget = Paragraph::new(lines).block(block).style(
        Style::default()
            .fg(theme.colors.fg_primary)
            .bg(theme.colors.bg_secondary),
    );

    frame.render_widget(Clear, overlay_area);
    frame.render_widget(widget, overlay_area);

    // Show cursor at end of query on the query line (row 1 inside border)
    // Covers: UX-DR74 (AC4 filter input cursor)
    // The query scrolls so its tail stays visible; the caret follows the
    // display cells actually painted (whole graphemes, never a scalar count).
    let cursor_col = (query_prefix_cells + geom::cells(shown_query)) as u16 + 1; // +1 for left border
    let cursor_x =
        (overlay_area.x + cursor_col).min(overlay_area.x + overlay_area.width.saturating_sub(2));
    let cursor_y = overlay_area.y + 1; // +1 for top border
    frame.set_cursor_position((cursor_x, cursor_y));
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    // ── Story 21.11 AC4/K4: display-width correctness ──────────────────────

    const C: &str = "界";
    const G: &str = "👨\u{200d}👩\u{200d}👧";
    const A: &str = "e\u{301}";

    fn state(query: &str, matches: &[&str]) -> ReverseSearchState {
        ReverseSearchState {
            active: true,
            query: query.to_string(),
            matches: matches
                .iter()
                .enumerate()
                .map(|(i, m)| (i, (*m).to_string()))
                .collect(),
            selected_match: 0,
        }
    }

    /// Paint the real overlay in a terminal exactly as tall as the overlay
    /// (1 query row, `matches` rows, 2 borders). Returns glyph rows (wide
    /// glyph continuations dropped), the buffer and the backend caret.
    fn draw(
        width: u16,
        state: &ReverseSearchState,
    ) -> (Vec<String>, ratatui::buffer::Buffer, (u16, u16)) {
        let rows = 1 + state.matches.len().max(1) as u16 + 2;
        let mut terminal = Terminal::new(TestBackend::new(width, rows)).unwrap();
        let theme = Theme::dark();
        terminal
            .draw(|frame| render(frame, frame.area(), state, &theme))
            .unwrap();
        let cursor = terminal.get_cursor_position().unwrap();
        let buffer = terminal.backend().buffer().clone();
        let glyph_rows = (0..rows)
            .map(|y| {
                let mut row = String::new();
                let mut x = 0u16;
                while x < width {
                    let symbol = buffer.cell((x, y)).unwrap().symbol();
                    row.push_str(symbol);
                    x += u16::try_from(geom::cells(symbol).max(1)).unwrap();
                }
                row
            })
            .collect();
        (glyph_rows, buffer, (cursor.x, cursor.y))
    }

    #[test]
    fn display_width_reverse_search_history_rows_fit_inside_the_border() {
        // 40 columns: 38 inner - 3 (` ▸ `) - 2 (quotes) = 33 cells per entry.
        let cases: Vec<(String, String)> = vec![
            // (history entry, entry as painted between the quotes)
            ("x".repeat(33), "x".repeat(33)),
            ("x".repeat(34), format!("{}…", "x".repeat(32))),
            (C.repeat(16), C.repeat(16)),
            (format!("{}x", C.repeat(16)), format!("{}x", C.repeat(16))),
            (format!("{}xx", C.repeat(16)), format!("{}…", C.repeat(16))),
            (C.repeat(17), format!("{}…", C.repeat(16))),
            (format!("{}x", G.repeat(16)), format!("{}x", G.repeat(16))),
            (G.repeat(17), format!("{}…", G.repeat(16))),
            (A.repeat(33), A.repeat(33)),
            (A.repeat(34), format!("{}…", A.repeat(32))),
            (format!("{C}{G}{A}"), format!("{C}{G}{A}")),
        ];
        for (entry, shown) in cases {
            let (rows, buffer, _cursor) = draw(40, &state("q", &[entry.as_str()]));
            assert!(
                rows[2].starts_with(&format!("│ ▸ \"{shown}\"")),
                "entry {entry:?}: {:?}",
                rows[2]
            );
            assert_eq!(buffer.cell((39, 2)).unwrap().symbol(), "│", "{entry:?}");
            assert_eq!(geom::cells(&rows[2]), 40, "{:?}", rows[2]);
        }
    }
}
