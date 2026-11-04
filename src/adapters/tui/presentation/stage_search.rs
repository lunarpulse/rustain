//! Within-conversation Stage-top search renderer (Ctrl+F).
//!
//! The compositor owns presence and placement; this pure renderer distinguishes
//! Typing from Navigating through the counter style. The query caret is painted
//! only for the active Typing owner; this renderer never places a hardware cursor.
// Covers: Story 4-4 AC1, AC2, AC3, AC7 (UX-DR86)

use ratatui::prelude::*;
use ratatui::widgets::{Clear, Paragraph};

use crate::adapters::tui::state::{SearchState, SearchSubstate};
use crate::adapters::tui::text_geometry as geom;
use crate::adapters::tui::theme::Theme;

/// Render a search line into the supplied Stage-top row.
///
/// `label` includes any ancestor breadcrumb and the trailing query separator.
/// `show_caret` enables the painted query caret only in Typing.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    state: &SearchState,
    theme: &Theme,
    label: &str,
    show_caret: bool,
) {
    frame.render_widget(Clear, area);

    // Build the visible line: "Search: <query>                  <counter>"
    //
    // Reserve the counter first. The field's EOL caret supplies the separator
    // gap instead of consuming an extra query cell; interior carets leave a gap.
    let label_cells = geom::cells(label);

    let counter_text = counter_text(state);
    let counter_cells = geom::cells(&counter_text);

    let query_budget = (area.width as usize).saturating_sub(label_cells + counter_cells);
    let view = state.query.view_with_gap(query_budget, 1);
    let shown_cells = geom::cells(view.before) + geom::cells(view.caret) + geom::cells(view.after);

    // Horizontal padding between the visible query and the counter.
    let total_content = label_cells + shown_cells + counter_cells;
    let padding_cols = (area.width as usize).saturating_sub(total_content);

    let (query_style, counter_style) = styles_for_substate(state, theme);
    let (ancestors, current_label) = label
        .strip_suffix("Search: ")
        .filter(|ancestors| !ancestors.is_empty())
        .map_or(("", label), |ancestors| (ancestors, "Search: "));

    let spans = vec![
        Span::styled(ancestors, Style::default().fg(theme.colors.fg_secondary)),
        Span::styled(
            current_label,
            Style::default()
                .fg(if ancestors.is_empty() {
                    theme.colors.fg_muted
                } else {
                    theme.colors.fg_primary
                })
                .add_modifier(Modifier::ITALIC),
        ),
        Span::styled(view.before, query_style),
        Span::styled(
            view.caret,
            if show_caret && state.substate == SearchSubstate::Typing {
                query_style.add_modifier(Modifier::REVERSED)
            } else {
                query_style
            },
        ),
        Span::styled(view.after, query_style),
        Span::raw(" ".repeat(padding_cols)),
        Span::styled(counter_text, counter_style),
    ];

    let bar = Paragraph::new(Line::from(spans)).style(
        Style::default()
            .fg(theme.colors.fg_primary)
            .bg(theme.colors.bg_secondary),
    );
    frame.render_widget(bar, area);
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
            query: query.into(),
            matches,
            focused_match_index: 0,
            substate,
            last_search_instant: None,
            last_query_len: 0,
            peek_highlight: None,
            ..SearchState::default()
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
    fn default_search_counter_is_safe() {
        let s = SearchState::default();
        assert_eq!(counter_text(&s), "0/0");
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
    /// dropped) and the painted buffer.
    fn draw(width: u16, state: &SearchState) -> (String, ratatui::buffer::Buffer) {
        let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
        let theme = Theme::dark();
        terminal
            .draw(|frame| render(frame, frame.area(), state, &theme, "Search: ", true))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut row = String::new();
        let mut x = 0u16;
        while x < width {
            let symbol = buffer.cell((x, 0)).unwrap().symbol();
            row.push_str(symbol);
            x += u16::try_from(geom::cells(symbol).max(1)).unwrap();
        }
        (row, buffer)
    }

    #[test]
    fn display_width_search_short_query_is_untouched() {
        let query = format!("{C}{G}{A}");
        let state = state_with(&query, one_match(), SearchSubstate::Typing);
        let (row, buffer) = draw(40, &state);
        assert!(row.starts_with(&format!("Search: {C}{G}{A} ")), "{row:?}");
        assert!(row.trim_end().ends_with("1/1"), "{row:?}");
        let caret = &buffer[(8 + 5, 0)];
        assert_eq!(caret.symbol(), " ");
        assert!(caret.modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn search_query_paints_interior_grapheme_caret_only_for_the_typing_owner() {
        use crate::adapters::tui::keymap::table_input::FieldCmd;

        for grapheme in [C, G, A] {
            for substate in [SearchSubstate::Typing, SearchSubstate::Navigating] {
                let mut state = state_with(&format!("x{grapheme}z"), one_match(), substate);
                state.query.edit(FieldCmd::LineStart);
                state.query.edit(FieldCmd::CursorRight);
                for show_caret in [false, true] {
                    let mut terminal = Terminal::new(TestBackend::new(40, 1)).unwrap();
                    terminal
                        .draw(|frame| {
                            render(
                                frame,
                                frame.area(),
                                &state,
                                &Theme::dark(),
                                "Search: ",
                                show_caret,
                            );
                        })
                        .unwrap();
                    let buffer = terminal.backend().buffer();
                    assert_eq!(buffer[(8, 0)].symbol(), "x");
                    assert_eq!(buffer[(9, 0)].symbol(), grapheme);
                    assert_eq!(
                        buffer[(9, 0)].modifier.contains(Modifier::REVERSED),
                        show_caret && substate == SearchSubstate::Typing,
                    );
                    assert_eq!(buffer[(9 + geom::cells(grapheme) as u16, 0)].symbol(), "z");
                    assert!(!buffer[(8, 0)].modifier.contains(Modifier::REVERSED));
                    assert_eq!(buffer[(37, 0)].symbol(), "1");
                    assert_eq!(buffer[(38, 0)].symbol(), "/");
                    assert_eq!(buffer[(39, 0)].symbol(), "1");
                    assert_eq!(
                        buffer[(37, 0)].modifier.contains(Modifier::BOLD),
                        substate == SearchSubstate::Navigating,
                    );
                }
            }
        }
    }

    #[test]
    fn search_query_view_keeps_whole_grapheme_tail_and_counter_on_narrow_rows() {
        for (width, query, shown) in [
            (20, C.repeat(5), C.repeat(4)),
            (20, G.repeat(5), G.repeat(4)),
            (20, A.repeat(10), A.repeat(8)),
            (20, format!("abc{C}{G}{A}"), format!("abc{C}{G}{A}")),
            (15, C.repeat(5), C.to_string()),
            (14, C.repeat(5), C.to_string()),
            (13, C.repeat(5), String::new()),
        ] {
            let state = state_with(&query, one_match(), SearchSubstate::Typing);
            let (row, buffer) = draw(width, &state);
            assert!(row.starts_with(&format!("Search: {shown} ")), "{row:?}");
            assert!(row.ends_with(" 1/1"), "{row:?}");
            assert_eq!(geom::cells(&row), usize::from(width));
            let caret_x = 8 + geom::cells(&shown) as u16;
            assert_eq!(buffer[(caret_x, 0)].symbol(), " ");
            assert!(buffer[(caret_x, 0)].modifier.contains(Modifier::REVERSED));
            assert!(
                caret_x < width - 3,
                "the caret gap remains before the counter"
            );
        }
        let state = state_with(C, one_match(), SearchSubstate::Typing);
        let (row, buffer) = draw(12, &state);
        assert_eq!(row, "Search:  1/1");
        assert!(buffer[(8, 0)].modifier.contains(Modifier::REVERSED));
        let (row, buffer) = draw(11, &state);
        assert_eq!(row, "Search: 1/1");
        assert!((0..11).all(|x| !buffer[(x, 0)].modifier.contains(Modifier::REVERSED)));
    }

    #[test]
    fn search_ancestor_label_and_inert_counters_keep_their_painted_content() {
        for (query, counter) in [("", "0/0"), ("missing", "No matches found")] {
            let state = state_with(query, vec![], SearchSubstate::Typing);
            let mut terminal = Terminal::new(TestBackend::new(60, 1)).unwrap();
            terminal
                .draw(|frame| {
                    render(
                        frame,
                        frame.area(),
                        &state,
                        &Theme::dark(),
                        "Parent › Search: ",
                        true,
                    );
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            let row = (0..60).map(|x| buffer[(x, 0)].symbol()).collect::<String>();
            assert!(row.starts_with(&format!("Parent › Search: {query} ")));
            assert!(row.ends_with(counter));
            let caret_x = geom::cells("Parent › Search: ") + geom::cells(query);
            assert_eq!(buffer[(caret_x as u16, 0)].symbol(), " ");
            assert!(
                buffer[(caret_x as u16, 0)]
                    .modifier
                    .contains(Modifier::REVERSED)
            );
            assert_eq!(buffer[(0, 0)].fg, Theme::dark().colors.fg_secondary);
            assert_eq!(buffer[(9, 0)].fg, Theme::dark().colors.fg_primary);
            assert!(buffer[(59, 0)].modifier.contains(Modifier::DIM));
        }
    }
}
