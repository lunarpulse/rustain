use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::adapters::tui::state::ReverseSearchState;
use crate::adapters::tui::text_geometry as geom;
use crate::adapters::tui::theme::Theme;

/// Maximum visible matches in the reverse search popup.
const MAX_VISIBLE_MATCHES: usize = 5;

/// Render the reverse search popup above the input box.
/// `show_caret` controls the painted query caret; no hardware cursor is placed.
/// Returns its painted outer rectangle, or `None` when no cells are painted.
// Covers: UX-DR74 (reverse search popup)
pub fn render(
    frame: &mut Frame,
    area: Rect,
    state: &ReverseSearchState,
    theme: &Theme,
    show_caret: bool,
) -> Option<Rect> {
    if !state.is_open() {
        return None;
    }
    let selected_match = state
        .selected_match
        .expect("open reverse search has a selection");

    let mut lines: Vec<Line> = Vec::new();

    // Query line
    let query_prefix = "(reverse-search): ";
    let query_prefix_cells = geom::cells(query_prefix);
    let inner_width = usize::from(area.width).saturating_sub(2); // borders
    let view = state
        .query
        .view(inner_width.saturating_sub(query_prefix_cells));
    let query_style = Style::default()
        .fg(theme.colors.fg_primary)
        .add_modifier(Modifier::BOLD);
    lines.push(Line::from(vec![
        Span::styled(
            query_prefix,
            Style::default()
                .fg(theme.colors.fg_muted)
                .add_modifier(Modifier::ITALIC),
        ),
        Span::styled(view.before, query_style),
        Span::styled(
            view.caret,
            if show_caret {
                query_style.add_modifier(Modifier::REVERSED)
            } else {
                query_style
            },
        ),
        Span::styled(view.after, query_style),
    ]));

    // Match lines
    if state.matches.is_empty() && !state.query.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no matches",
            Style::default().fg(theme.colors.fg_muted),
        )));
    } else {
        let scroll = selected_match.saturating_sub(MAX_VISIBLE_MATCHES - 1);
        for (i, (_idx, entry)) in state
            .matches
            .iter()
            .skip(scroll)
            .take(MAX_VISIBLE_MATCHES)
            .enumerate()
        {
            let display_idx = i + scroll;
            let prefix = if display_idx == selected_match {
                " ▸ "
            } else {
                "   "
            };
            let style = if display_idx == selected_match {
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
    (!overlay_area.is_empty()).then_some(overlay_area)
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
            query: query.into(),
            matches: matches
                .iter()
                .enumerate()
                .map(|(i, m)| (i, (*m).to_string()))
                .collect(),
            selected_match: Some(0),
        }
    }

    /// Paint the real popup in a terminal exactly as tall as the popup
    /// (1 query row, `matches` rows, 2 borders). Returns glyph rows (wide
    /// glyph continuations dropped) and the painted buffer.
    fn draw(width: u16, state: &ReverseSearchState) -> (Vec<String>, ratatui::buffer::Buffer) {
        let rows = 1 + state.matches.len().max(1) as u16 + 2;
        let mut terminal = Terminal::new(TestBackend::new(width, rows)).unwrap();
        let theme = Theme::dark();
        terminal
            .draw(|frame| {
                let _ = render(frame, frame.area(), state, &theme, true);
            })
            .unwrap();
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
        (glyph_rows, buffer)
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
            let (rows, buffer) = draw(40, &state("q", &[entry.as_str()]));
            assert!(
                rows[2].starts_with(&format!("│ ▸ \"{shown}\"")),
                "entry {entry:?}: {:?}",
                rows[2]
            );
            assert_eq!(buffer.cell((39, 2)).unwrap().symbol(), "│", "{entry:?}");
            assert_eq!(geom::cells(&rows[2]), 40, "{:?}", rows[2]);
            assert!(rows[1].starts_with("│(reverse-search): q "));
            assert_eq!(buffer[(20, 1)].symbol(), " ");
            assert!(buffer[(20, 1)].modifier.contains(Modifier::REVERSED));
        }
    }

    #[test]
    fn reverse_search_query_paints_only_the_owned_interior_grapheme_caret() {
        use crate::adapters::tui::keymap::table_input::FieldCmd;

        for grapheme in [C, G, A] {
            let mut state = state(&format!("x{grapheme}z"), &["hit"]);
            state.query.edit(FieldCmd::LineStart);
            state.query.edit(FieldCmd::CursorRight);
            for show_caret in [false, true] {
                let mut terminal = Terminal::new(TestBackend::new(40, 4)).unwrap();
                terminal
                    .draw(|frame| {
                        let _ = render(frame, frame.area(), &state, &Theme::dark(), show_caret);
                    })
                    .unwrap();
                let buffer = terminal.backend().buffer();
                assert_eq!(buffer[(19, 1)].symbol(), "x");
                assert_eq!(buffer[(20, 1)].symbol(), grapheme);
                assert_eq!(
                    buffer[(20, 1)].modifier.contains(Modifier::REVERSED),
                    show_caret,
                );
                assert_eq!(buffer[(20 + geom::cells(grapheme) as u16, 1)].symbol(), "z");
                assert!(!buffer[(19, 1)].modifier.contains(Modifier::REVERSED));
                assert_eq!(buffer[(39, 1)].symbol(), "│");
            }
        }
    }

    #[test]
    fn reverse_search_query_view_keeps_whole_grapheme_tail_inside_narrow_border() {
        for (width, query, shown) in [
            (28, C.repeat(5), C.repeat(3)),
            (28, G.repeat(5), G.repeat(3)),
            (28, A.repeat(10), A.repeat(7)),
            (28, format!("abc{C}{G}{A}"), format!("bc{C}{G}{A}")),
            (23, C.repeat(5), C.to_string()),
            (22, C.repeat(5), String::new()),
            (21, C.repeat(5), String::new()),
        ] {
            let (rows, buffer) = draw(width, &state(&query, &["hit"]));
            assert!(rows[1].starts_with(&format!("│(reverse-search): {shown} ")));
            assert_eq!(geom::cells(&rows[1]), usize::from(width));
            let caret_x = 19 + geom::cells(&shown) as u16;
            assert_eq!(buffer[(caret_x, 1)].symbol(), " ");
            assert!(buffer[(caret_x, 1)].modifier.contains(Modifier::REVERSED));
            assert!(caret_x < width - 1);
            assert_eq!(buffer[(width - 1, 1)].symbol(), "│");
        }
        let (rows, buffer) = draw(20, &state(C, &["hit"]));
        assert_eq!(geom::cells(&rows[1]), 20);
        assert_eq!(buffer[(19, 1)].symbol(), "│");
        assert!((0..20).all(|x| !buffer[(x, 1)].modifier.contains(Modifier::REVERSED)));
    }
}
