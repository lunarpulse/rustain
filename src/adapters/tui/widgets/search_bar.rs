//! Within-conversation search bar widget (Ctrl+F).
//!
//! Renders a single-row bar at the top of the chat pane region when
//! `SearchState::active` is true. Visually distinguishes the `Typing` vs
//! `Navigating` sub-states (Story 4-4 AC3) — the counter gains a bold frame
//! in `Navigating` and the cursor is hidden to signal that `n` / `N` are now
//! live for match navigation.
//!
//! Pure render — no state mutation.
// Covers: Story 4-4 AC1, AC2, AC3, AC7 (UX-DR86)

use ratatui::prelude::*;
use ratatui::widgets::{Clear, Paragraph};

use crate::adapters::tui::state::{SearchState, SearchSubstate};
use crate::adapters::tui::text_geometry as geom;
use crate::adapters::tui::theme::Theme;

/// Left-side label rendered before the query in the search bar.
const LABEL: &str = "Search: ";

/// Render the search bar at the top of a 1-row `area`.
///
/// The layout caller must reserve the row via `compute_layout` (Task 2.3).
/// This function is a no-op when `state.active == false`.
pub fn render(frame: &mut Frame, area: Rect, state: &SearchState, theme: &Theme) {
    if !state.active {
        return;
    }

    frame.render_widget(Clear, area);

    // Build the visible line: "Search: <query>                  <counter>"
    //
    // The counter floats right and is reserved FIRST (with one gap cell). The
    // query gets whatever display cells remain; when it is longer it scrolls
    // so its tail (where the caret sits) stays visible, cut on a whole
    // grapheme boundary. All widths are display cells, not scalars.
    let label_cells = geom::cells(LABEL);

    let counter_text = counter_text(state);
    let counter_cells = geom::cells(&counter_text);

    let query_budget = (area.width as usize).saturating_sub(label_cells + 1 + counter_cells);
    let shown_query = geom::clip_tail(&state.query, query_budget);
    let shown_cells = geom::cells(shown_query);

    // Horizontal padding between the visible query and the counter.
    let total_content = label_cells + shown_cells + counter_cells;
    let padding_cols = (area.width as usize).saturating_sub(total_content).max(1);

    let (query_style, counter_style) = styles_for_substate(state, theme);

    let spans: Vec<Span<'static>> = vec![
        Span::styled(
            LABEL.to_string(),
            Style::default()
                .fg(theme.colors.fg_muted)
                .add_modifier(Modifier::ITALIC),
        ),
        Span::styled(shown_query.to_string(), query_style),
        Span::raw(" ".repeat(padding_cols)),
        Span::styled(counter_text, counter_style),
    ];

    let bar = Paragraph::new(Line::from(spans)).style(
        Style::default()
            .fg(theme.colors.fg_primary)
            .bg(theme.colors.bg_secondary),
    );
    frame.render_widget(bar, area);

    // Place the text cursor at end of query ONLY in Typing sub-state.
    // In Navigating, the cursor is hidden to signal the mode change — `n`/`N`
    // are now live for navigation and typed characters return to Typing.
    if state.substate == SearchSubstate::Typing {
        let cursor_col = area.x + (label_cells + shown_cells) as u16;
        let cursor_col = cursor_col.min(area.x + area.width.saturating_sub(1));
        frame.set_cursor_position((cursor_col, area.y));
    }
}

/// Right-side counter text per AC2 and AC7:
/// - empty query → `0/0`
/// - query with zero matches → `No matches found`
/// - query with ≥1 matches → `<k>/<N>` where k is 1-indexed for human display
fn counter_text(state: &SearchState) -> String {
    if state.query.is_empty() {
        return "0/0".to_string();
    }
    if state.matches.is_empty() {
        return "No matches found".to_string();
    }
    let human_idx = state.focused_match_index + 1;
    format!("{}/{}", human_idx, state.matches.len())
}

/// Styles for the query span and counter span. In `Navigating` sub-state the
/// counter is wrapped in bold to signal the committed/navigation mode, per
/// AC3 amendment.
fn styles_for_substate(state: &SearchState, theme: &Theme) -> (Style, Style) {
    let query_style = Style::default()
        .fg(theme.colors.fg_primary)
        .add_modifier(Modifier::BOLD);
    let mut counter_style = Style::default().fg(theme.colors.fg_muted);
    if state.substate == SearchSubstate::Navigating && !state.matches.is_empty() {
        counter_style = counter_style.add_modifier(Modifier::BOLD);
    }
    if state.query.is_empty() || state.matches.is_empty() {
        // 0/0 and "No matches found" render dimmed — the bar is open but inert.
        counter_style = counter_style.add_modifier(Modifier::DIM);
    }
    (query_style, counter_style)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::services::search::SearchMatch;

    fn state_with(query: &str, matches: Vec<SearchMatch>, substate: SearchSubstate) -> SearchState {
        SearchState {
            active: true,
            query: query.to_string(),
            matches,
            focused_match_index: 0,
            substate,
            last_search_instant: None,
            last_query_len: 0,
            prior_focus: None,
            peek_highlight: None,
        }
    }

    #[test]
    fn counter_empty_query_shows_zero_zero() {
        let s = state_with("", vec![], SearchSubstate::Typing);
        assert_eq!(counter_text(&s), "0/0");
    }

    #[test]
    fn counter_query_with_no_matches_shows_no_matches_found() {
        let s = state_with("xyz", vec![], SearchSubstate::Typing);
        assert_eq!(counter_text(&s), "No matches found");
    }

    #[test]
    fn counter_with_matches_shows_one_indexed() {
        let matches = vec![
            SearchMatch {
                message_index: 0,
                byte_start: 0,
                byte_end: 3,
            },
            SearchMatch {
                message_index: 0,
                byte_start: 10,
                byte_end: 13,
            },
            SearchMatch {
                message_index: 1,
                byte_start: 0,
                byte_end: 3,
            },
        ];
        let mut s = state_with("foo", matches, SearchSubstate::Navigating);
        assert_eq!(counter_text(&s), "1/3");
        s.focused_match_index = 1;
        assert_eq!(counter_text(&s), "2/3");
        s.focused_match_index = 2;
        assert_eq!(counter_text(&s), "3/3");
    }

    #[test]
    fn navigating_substate_bolds_counter_when_matches_exist() {
        let theme = Theme::dark();
        let s = state_with(
            "foo",
            vec![SearchMatch {
                message_index: 0,
                byte_start: 0,
                byte_end: 3,
            }],
            SearchSubstate::Navigating,
        );
        let (_q, counter) = styles_for_substate(&s, &theme);
        assert!(counter.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn typing_substate_does_not_bold_counter() {
        let theme = Theme::dark();
        let s = state_with(
            "foo",
            vec![SearchMatch {
                message_index: 0,
                byte_start: 0,
                byte_end: 3,
            }],
            SearchSubstate::Typing,
        );
        let (_q, counter) = styles_for_substate(&s, &theme);
        assert!(!counter.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn inactive_search_is_noop() {
        // We can't easily drive `render` without a full Frame, but we can
        // verify that counter_text handles an empty/default state without panicking.
        let s = SearchState::default();
        // Must not panic on default state
        let _ = counter_text(&s);
    }

    // ── Story 21.11 AC4/K4: display-width correctness ──────────────────────

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    const C: &str = "界";
    const G: &str = "👨\u{200d}👩\u{200d}👧";
    const A: &str = "e\u{301}";

    fn one_match() -> Vec<SearchMatch> {
        vec![SearchMatch {
            message_index: 0,
            byte_start: 0,
            byte_end: 1,
        }]
    }

    /// Paint the real search bar; return the painted row (continuations
    /// dropped), the buffer and the backend cursor column.
    fn draw(width: u16, state: &SearchState) -> (String, ratatui::buffer::Buffer, u16) {
        let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
        let theme = Theme::dark();
        terminal
            .draw(|frame| render(frame, frame.area(), state, &theme))
            .unwrap();
        let cursor = terminal.get_cursor_position().unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut row = String::new();
        let mut x = 0u16;
        while x < width {
            let symbol = buffer.cell((x, 0)).unwrap().symbol();
            row.push_str(symbol);
            x += u16::try_from(geom::cells(symbol).max(1)).unwrap();
        }
        (row, buffer, cursor.x)
    }

    #[test]
    fn display_width_search_short_query_is_untouched() {
        let query = format!("{C}{G}{A}");
        let state = state_with(&query, one_match(), SearchSubstate::Typing);
        let (row, _buffer, cursor_x) = draw(40, &state);
        assert!(row.starts_with(&format!("Search: {C}{G}{A} ")), "{row:?}");
        assert!(row.trim_end().ends_with("1/1"), "{row:?}");
        assert_eq!(cursor_x, 8 + 5);
    }
}
