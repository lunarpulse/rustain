//! K4 — shell cells (Story 21.11, AC4).
//!
//! Every test drives a real public renderer (`status_bar::render`,
//! `tab_bar::render_tab_bar`, `sidebar::render_history_panel`,
//! `search_bar::render`, `reverse_search::render`, `cross_search::render`,
//! `bookmark_list::render`, `command_palette::render`) into a `TestBackend`
//! buffer and reads the painted cells back. Expected values come from the
//! C/G/A fixtures and from independent observations of the painted row (for
//! example the measured left edge of the status bar), never from the
//! production clipping formulas.
//!
//! Each case has an ASCII positive control and exact-fit / fit-minus-one (one
//! over) boundaries at the real content budget. The palette is driven with the
//! exact strings the production constructors in `event_loop` register (model
//! display names with `Ctrl+X, M`, profile names with `(active)`); those
//! constructors are inline in the async start-up function and are not callable
//! from here.
//!
//! The transcript half of the highlight rule (`chat_pane::render_with_search`)
//! is covered by K3.

use super::support::*;

use ratatui::style::Modifier;
use rustain::adapters::tui::state::{
    CommandPaletteState, CrossSearchState, LogAwareness, ReverseSearchState, SearchState,
    SearchSubstate,
};
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::{
    bookmark_list, command_palette, cross_search, reverse_search, search_bar, sidebar, status_bar,
    tab_bar,
};
use rustain::domain::models::palette::{PaletteAction, PaletteEntry, PaletteScope};
use rustain::domain::models::tab::TabManager;
use rustain::domain::models::visual::DensityMode;
use rustain::domain::models::{MessageRole, PermissionMode, StatusState, now_unix};
use rustain::domain::services::cross_search::CrossSearchResult;
use rustain::domain::services::search::SearchMatch;
use rustain::domain::services::session_index::SessionSummary;
use unicode_width::UnicodeWidthStr;

use crate::render_helpers::{make_conversation, make_msg};

const WIDTHS: [u16; 3] = [60, 80, 130];
/// ASCII control first, then the three boundary clusters.
const UNITS: [&str; 4] = ["x", C, G, A];

fn cells(text: &str) -> usize {
    text.width()
}

/// Exactly `total` cells of `unit` (its whole clusters), padded with `x` when
/// the unit does not divide the total.
fn fill(unit: &str, total: usize) -> String {
    let mut out = unit.repeat(total / cells(unit));
    out.push_str(&"x".repeat(total % cells(unit)));
    assert_eq!(cells(&out), total);
    out
}

/// `prefix` followed by `h` filler so the result is exactly `total` cells.
fn hint_of(prefix: &str, total: usize) -> String {
    format!("{prefix}{}", "h".repeat(total - cells(prefix)))
}

/// Exact fit then one cell over for a clipped row. `body_of` paints the real
/// renderer with the given text and returns the visible body (the row-level
/// invariants — prefix, intact border, whole clusters — are asserted inside
/// it). Exact fit returns the text unchanged; one cell over ends in `marker`,
/// keeps a prefix of the text and uses at least `budget - 1` cells.
fn assert_fit_then_cut(unit: &str, budget: usize, marker: &str, body_of: &dyn Fn(&str) -> String) {
    let fit = fill(unit, budget);
    assert_eq!(body_of(&fit), fit, "{unit:?} exact fit at {budget}");
    let over = fill(unit, budget + 1);
    let body = body_of(&over);
    let kept = body
        .strip_suffix(marker)
        .unwrap_or_else(|| panic!("{unit:?} over {budget}: {body:?} lacks {marker:?}"));
    assert!(over.starts_with(kept), "{unit:?} over {budget}: {body:?}");
    assert!(
        cells(&body) <= budget && cells(&body) + 1 >= budget,
        "{unit:?} over {budget}: {} cells in {body:?}",
        cells(&body)
    );
}

/// Exact fit then one cell over (and far over) for a scrolling query:
/// exact fit is shown whole, longer queries show a tail of the query that
/// uses at least `budget - 1` cells.
fn assert_fit_then_tail(unit: &str, budget: usize, shown_of: &dyn Fn(&str) -> String) {
    let fit = fill(unit, budget);
    assert_eq!(shown_of(&fit), fit, "{unit:?} exact fit at {budget}");
    for total in [budget + 1, 3 * budget] {
        let query = fill(unit, total);
        let shown = shown_of(&query);
        assert!(query.ends_with(&shown), "{unit:?} {total}: {shown:?}");
        assert!(
            cells(&shown) <= budget && cells(&shown) + 1 >= budget,
            "{unit:?} {total}: {} cells of {budget}",
            cells(&shown)
        );
    }
}

// ── status bar ─────────────────────────────────────────────────────────────

fn status(
    width: u16,
    model: &str,
    hint: Option<&str>,
    agent: Option<&str>,
    awareness: LogAwareness,
) -> Painted {
    let theme = Theme::dark();
    paint(width, 1, |frame| {
        status_bar::render(
            frame,
            frame.area(),
            model,
            None,
            &StatusState::Idle,
            &theme,
            0,
            &[],
            0,
            20,
            PermissionMode::Normal,
            None,
            0,
            false,
            None,
            false,
            true,
            0,
            hint,
            0,
            agent,
            None,
            None,
            false,
            None,
            None,
            DensityMode::Focus,
            false,
            None,
            awareness,
            0,
        );
    })
}

#[test]
fn k4_status_hint_admission_and_log_edge_follow_cells() {
    for width in WIDTHS {
        for model in ["sonnet", "模型", "界x"] {
            // The left edge is measured from the real bar painted without a
            // hint (minus the Story 21.3 `^X menu` tail), then the hint budget is
            // what remains beside it.
            let bare = status(width, model, None, None, LogAwareness::Hidden);
            let left = cells(
                row_text(&bare.buffer, 0)
                    .trim_end_matches("^X menu")
                    .trim_end(),
            );
            for (awareness, tail) in [
                (LogAwareness::Hidden, " ^X menu"),
                (LogAwareness::Unseen(7), " log: 7 ^X menu"),
            ] {
                let region = usize::from(width) - tail.len();
                let budget = region - left - 1;
                for prefix in ["", C, G, A] {
                    let fit = hint_of(prefix, budget);
                    let painted = status(width, model, Some(&fit), None, awareness);
                    let row = row_text(&painted.buffer, 0);
                    assert!(
                        row.ends_with(&format!("{fit}{tail}")),
                        "exact fit must stay flush at the region edge: {model} {width} {fit:?} {row:?}"
                    );
                    assert_eq!(row_cells(&painted.buffer, 0), usize::from(width), "{row:?}");
                    assert_whole_clusters(&painted.buffer);

                    let over = hint_of(prefix, budget + 1);
                    let painted = status(width, model, Some(&over), None, awareness);
                    let row = row_text(&painted.buffer, 0);
                    assert!(
                        !row.contains("hh") && !row.contains(G) && !row.contains(A),
                        "one cell over must omit the hint: {model} {width} {over:?} {row:?}"
                    );
                    assert!(row.ends_with(tail), "reserved edge intact: {row:?}");
                    assert_eq!(row_cells(&painted.buffer, 0), usize::from(width), "{row:?}");
                    assert_whole_clusters(&painted.buffer);
                }
            }
        }
    }
}

#[test]
fn k4_status_agent_name_is_24_cells_with_the_marker_inside() {
    let cases: Vec<(String, String)> = vec![
        ("a".repeat(24), "a".repeat(24)),
        ("a".repeat(25), format!("{}…", "a".repeat(23))),
        (C.repeat(12), C.repeat(12)),
        (format!("{}x", C.repeat(12)), format!("{}…", C.repeat(11))),
        (C.repeat(13), format!("{}…", C.repeat(11))),
        (G.repeat(12), G.repeat(12)),
        (G.repeat(13), format!("{}…", G.repeat(11))),
        (A.repeat(24), A.repeat(24)),
        (A.repeat(25), format!("{}…", A.repeat(23))),
    ];
    // Story 21.3 A10: the `^X menu` tail (8 cells with its gap) is reserved
    // first, so the bar is painted 8 cells wider to keep the left region at
    // the width this test is about, and the tail is stripped before reading.
    for width in WIDTHS {
        for (name, shown) in &cases {
            let painted = status(width + 8, "m", None, Some(name), LogAwareness::Hidden);
            let row = row_text(&painted.buffer, 0);
            let body = row.trim_end_matches("^X menu").trim_end();
            assert!(
                body.ends_with(&format!("Agent: {shown}")),
                "{width} {name:?}: {row:?}"
            );
            assert_whole_clusters(&painted.buffer);
        }
    }
}

// ── tabs ───────────────────────────────────────────────────────────────────

fn tabs(specs: &[(&str, bool)]) -> TabManager {
    let mut manager = TabManager::default();
    for _ in 1..specs.len() {
        manager.create_tab();
    }
    for (tab, (title, read_only)) in manager.tabs_mut().iter_mut().zip(specs) {
        tab.conversation.title = (*title).to_string();
        tab.read_only = *read_only;
    }
    manager
}

fn tab_row(manager: &TabManager, active: usize, width: u16) -> Painted {
    let theme = Theme::dark();
    paint(width, 1, |frame| {
        tab_bar::render_tab_bar(manager, active, frame.area(), frame.buffer_mut(), &theme);
    })
}

#[test]
fn k4_tab_titles_clip_at_cells_and_the_hidden_count_is_exact() {
    for title in [C.repeat(20), G.repeat(12), A.repeat(30), "x".repeat(25)] {
        let manager = tabs(&[(title.as_str(), false), (title.as_str(), false)]);
        // Wide enough for both: measure the real segment.
        let wide = tab_row(&manager, 1, 130);
        let text = row_text(&wide.buffer, 0);
        let segments: Vec<&str> = text.split_whitespace().collect();
        assert_eq!(segments.len(), 2, "{title:?}: {text:?}");
        assert_eq!(segments[0], segments[1]);
        let segment = cells(segments[0]);
        // Brackets plus a 20-cell title budget: 21 or 22 cells (a wide glyph
        // may leave a one-cell gap); never the 80 cells of 20 wide scalars.
        assert!(
            (21..=22).contains(&segment),
            "{title:?}: {segment} cells in {text:?}"
        );
        assert!(segments[0].ends_with("…]"), "{title:?}: {text:?}");
        assert_whole_clusters(&wide.buffer);

        // Exact fit: two segments and two separators.
        let total = 2 * (segment + 1);
        let fit = tab_row(&manager, 1, u16::try_from(total).unwrap());
        let text = row_text(&fit.buffer, 0);
        assert_eq!(text.split_whitespace().count(), 2, "{title:?}: {text:?}");
        assert!(!text.contains('+'), "{title:?}: {text:?}");

        // One cell short: the inactive tab is hidden, the active one stays,
        // and the count says exactly one.
        let short = tab_row(&manager, 1, u16::try_from(total - 1).unwrap());
        let text = row_text(&short.buffer, 0);
        let segments: Vec<&str> = text.split_whitespace().collect();
        assert_eq!(segments.len(), 2, "{title:?}: {text:?}");
        assert_eq!(cells(segments[0]), segment, "{title:?}: {text:?}");
        assert_eq!(segments[1], "+1", "{title:?}: {text:?}");
        assert_whole_clusters(&short.buffer);
    }
}

#[test]
fn k4_tab_painter_keeps_clusters_continuations_and_styles() {
    let theme = Theme::dark();
    let title = format!("{C}{G}{A}");
    let manager = tabs(&[("one", false), (title.as_str(), true)]);
    for (active, fg, bold) in [
        (1usize, theme.colors.accent, true),
        (0, theme.colors.fg_secondary, false),
    ] {
        let painted = tab_row(&manager, active, 40);
        assert_eq!(
            row_text(&painted.buffer, 0),
            format!("[one] [\u{1F441} {C}{G}{A}]"),
            "active {active}"
        );
        // The cells holding a whole C/G/A cluster, left to right.
        let clusters: Vec<(u16, &str)> = (0..40u16)
            .filter_map(|x| {
                let symbol = painted.buffer.cell((x, 0)).unwrap().symbol();
                [C, G, A].contains(&symbol).then_some((x, symbol))
            })
            .collect();
        let symbols: Vec<&str> = clusters.iter().map(|(_, symbol)| *symbol).collect();
        assert_eq!(symbols, vec![C, G, A], "each cluster painted once");
        for (x, _) in &clusters {
            let cell = painted.buffer.cell((*x, 0)).unwrap();
            assert_eq!(cell.fg, fg, "cell {x} (active {active})");
            assert_eq!(
                cell.modifier.contains(Modifier::BOLD),
                bold,
                "cell {x} (active {active})"
            );
        }
        // A wide glyph owns its cell plus one blank continuation.
        for (x, _) in &clusters[..2] {
            assert_eq!(painted.buffer.cell((x + 1, 0)).unwrap().symbol(), " ");
        }
        assert_whole_clusters(&painted.buffer);
    }
}

#[test]
fn k4_tab_active_title_that_cannot_fit_is_cut_on_a_grapheme() {
    let wide = C.repeat(20);
    let family = G.repeat(20);
    let ascii = "x".repeat(40);
    for (title, width) in [(&wide, 12u16), (&family, 11), (&ascii, 12)] {
        let manager = tabs(&[(title.as_str(), false)]);
        let painted = tab_row(&manager, 0, width);
        let text = row_text(&painted.buffer, 0);
        assert!(text.starts_with('['), "{title:?}: {text:?}");
        assert!(text.ends_with('…'), "{title:?}: {text:?}");
        assert!(cells(&text) <= usize::from(width), "{title:?}: {text:?}");
        assert_eq!(row_cells(&painted.buffer, 0), usize::from(width));
        assert_whole_clusters(&painted.buffer);
        assert_no_partial_family(&text);
    }
}

// ── sidebar ────────────────────────────────────────────────────────────────

fn sidebar_panel(width: u16, title: &str) -> Painted {
    let theme = Theme::dark();
    let now = now_unix();
    let entry = SessionSummary::new("c1".into(), title.into(), now, now, 3);
    paint(width, 3, |frame| {
        sidebar::render_history_panel(frame.area(), frame.buffer_mut(), &[entry], 0, None, &theme);
    })
}

/// The title between the indicator and ` just now [3]`, after the row-level
/// invariants (whole clusters, no family fragment).
fn sidebar_body(width: u16, title: &str) -> String {
    let painted = sidebar_panel(width, title);
    let row = row_text(&painted.buffer, 1);
    let (body, _) = row
        .strip_prefix("│  ")
        .and_then(|rest| rest.split_once(" just now [3]"))
        .unwrap_or_else(|| panic!("{width} {title:?}: {row:?}"));
    assert_whole_clusters(&painted.buffer);
    assert_no_partial_family(&row);
    body.to_string()
}

#[test]
fn k4_sidebar_title_fits_its_budget_and_cuts_on_whole_graphemes() {
    // Row = 2-cell indicator + title + ` just now` + ` [3]`; the title budget
    // is the inner width minus 17.
    for width in [30u16, 60, 80, 130] {
        let budget = usize::from(width) - 2 - 17;
        for unit in UNITS {
            assert_fit_then_cut(unit, budget, "...", &|title| sidebar_body(width, title));
        }
    }
}

// ── search bar ─────────────────────────────────────────────────────────────

fn search(width: u16, query: &str) -> Painted {
    let mut state = SearchState::new();
    state.active = true;
    state.query = query.to_string();
    state.matches = vec![SearchMatch {
        message_index: 0,
        byte_start: 0,
        byte_end: 1,
    }];
    state.substate = SearchSubstate::Typing;
    let theme = Theme::dark();
    paint(width, 1, |frame| {
        search_bar::render(frame, frame.area(), &state, &theme, true);
    })
}

/// Paint one query and return the visible query after the invariants every
/// query shares: the counter keeps its cells and style at the right edge, the
/// caret follows the painted cells and never reaches the counter, the query
/// keeps its style.
fn search_shown(width: u16, query: &str) -> String {
    let theme = Theme::dark();
    let painted = search(width, query);
    let row = row_text(&painted.buffer, 0);
    assert!(row.starts_with("Search: "), "{row:?}");
    assert!(row.ends_with("1/1"), "counter survives: {width} {row:?}");
    assert_eq!(row_cells(&painted.buffer, 0), usize::from(width), "{row:?}");
    let shown = row["Search: ".len()..row.len() - "1/1".len()]
        .trim_end()
        .to_string();
    let caret = painted.cursor.expect("typing shows the caret");
    assert_eq!(usize::from(caret.x), 8 + cells(&shown), "{width} {query:?}");
    assert_eq!(caret.y, 0);
    assert!(usize::from(caret.x) < usize::from(width) - 3);
    let counter = painted.buffer.cell((width - 3, 0)).unwrap();
    assert_eq!(counter.symbol(), "1");
    assert_eq!(counter.fg, theme.colors.fg_muted);
    let first = painted.buffer.cell((8, 0)).unwrap();
    assert_eq!(first.fg, theme.colors.fg_primary);
    assert!(first.modifier.contains(Modifier::BOLD));
    assert_whole_clusters(&painted.buffer);
    shown
}

#[test]
fn k4_search_bar_reserves_the_counter_and_maps_the_caret() {
    for width in WIDTHS {
        // `Search: ` (8) + query + one gap cell + `1/1` (3).
        let budget = usize::from(width) - 8 - 1 - 3;
        for unit in UNITS {
            assert_fit_then_tail(unit, budget, &|query| search_shown(width, query));
        }
    }
    // A short mixed query is untouched and the caret follows its cells.
    let mixed = format!("{C}{G}{A}");
    assert_eq!(search_shown(60, &mixed), mixed);
}

// ── reverse search ─────────────────────────────────────────────────────────

fn reverse(width: u16, query: &str, entry: &str) -> Painted {
    let state = ReverseSearchState {
        active: true,
        query: query.to_string(),
        matches: vec![(0, entry.to_string())],
        selected_match: 0,
    };
    let theme = Theme::dark();
    paint(width, 4, |frame| {
        reverse_search::render(frame, frame.area(), &state, &theme, true);
    })
}

/// The query line's visible query, after the row-level invariants (caret on
/// the painted cells and off the border, border intact, whole clusters).
fn reverse_query_shown(width: u16, query: &str) -> String {
    let painted = reverse(width, query, "m");
    let row = row_text(&painted.buffer, 1);
    let shown = row
        .strip_prefix("│(reverse-search): ")
        .and_then(|rest| rest.strip_suffix('│'))
        .unwrap_or_else(|| panic!("{width} {query:?}: {row:?}"))
        .trim_end()
        .to_string();
    let caret = painted.cursor.expect("reverse search shows the caret");
    assert_eq!(
        usize::from(caret.x),
        1 + 18 + cells(&shown),
        "{width} {query:?}"
    );
    assert_eq!(caret.y, 1);
    assert!(
        usize::from(caret.x) < usize::from(width) - 1,
        "not on the border"
    );
    assert_whole_clusters(&painted.buffer);
    shown
}

/// The history row's entry between its quotes, with the right border intact.
fn reverse_entry_display(width: u16, entry: &str) -> String {
    let painted = reverse(width, "q", entry);
    let row = row_text(&painted.buffer, 2);
    let (display, _) = row
        .strip_prefix("│ ▸ \"")
        .and_then(|rest| rest.split_once('"'))
        .unwrap_or_else(|| panic!("{width} {entry:?}: {row:?}"));
    assert_eq!(
        painted.buffer.cell((width - 1, 2)).unwrap().symbol(),
        "│",
        "right border intact"
    );
    assert_whole_clusters(&painted.buffer);
    display.to_string()
}

#[test]
fn k4_reverse_search_query_scrolls_and_history_rows_fit_the_border() {
    for width in WIDTHS {
        for unit in UNITS {
            // Query line: inner - the 18-cell prefix - the caret cell.
            assert_fit_then_tail(unit, usize::from(width) - 2 - 18 - 1, &|query| {
                reverse_query_shown(width, query)
            });
            // History row `│ ▸ "<entry>"`: inner - 3 (` ▸ `) - 2 (quotes).
            assert_fit_then_cut(unit, usize::from(width) - 2 - 3 - 2, "…", &|entry| {
                reverse_entry_display(width, entry)
            });
        }
    }
    // A short mixed query is untouched and the caret follows its cells.
    let mixed = format!("{C}{G}{A}");
    assert_eq!(reverse_query_shown(60, &mixed), mixed);
}

// ── cross search ───────────────────────────────────────────────────────────

fn cross(width: u16, query: &str, title: &str, excerpt: &str) -> Painted {
    let mut state = CrossSearchState::new();
    state.active = true;
    state.query = query.to_string();
    state.results = vec![CrossSearchResult {
        conversation_id: "c1".into(),
        title: title.to_string(),
        excerpt: excerpt.to_string(),
        timestamp: 0,
        first_match_message_index: 0,
    }];
    let theme = Theme::dark();
    paint(width, 12, |frame| {
        cross_search::render(frame, frame.area(), &state, &theme, true);
    })
}

/// The query row's visible query, after the row-level invariants (caret on
/// the painted cells, border intact, whole clusters).
fn cross_query_shown(width: u16, query: &str) -> String {
    let painted = cross(width, query, "t", "plain");
    let row = row_text(&painted.buffer, 1);
    let shown = row
        .strip_prefix("│/ ")
        .and_then(|rest| rest.strip_suffix('│'))
        .unwrap_or_else(|| panic!("{width} {query:?}: {row:?}"))
        .trim_end()
        .to_string();
    let caret = painted.cursor.expect("cross search shows the caret");
    assert_eq!(
        usize::from(caret.x),
        1 + 2 + cells(&shown),
        "{width} {query:?}"
    );
    assert_eq!(caret.y, 1);
    assert_whole_clusters(&painted.buffer);
    shown
}

/// Body of a result row (`y` 3 = title, 4 = excerpt) between its prefix and
/// the intact right border.
fn cross_body(width: u16, y: u16, prefix: &str, title: &str, excerpt: &str) -> String {
    let painted = cross(width, "zz", title, excerpt);
    let row = row_text(&painted.buffer, y);
    let body = row
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_suffix('│'))
        .unwrap_or_else(|| panic!("{width} y{y}: {row:?}"))
        .trim_end()
        .to_string();
    assert_whole_clusters(&painted.buffer);
    body
}

#[test]
fn k4_cross_search_query_title_and_excerpt_fit_the_real_budgets() {
    // Sidebar-column widths plus the full-width supported sizes.
    for width in [30u16, 40, 60, 80] {
        let inner = usize::from(width) - 2;
        for unit in UNITS {
            // Query row `│/ <query>`: inner - `/ ` - the caret cell.
            assert_fit_then_tail(unit, inner - 2 - 1, &|query| {
                cross_query_shown(width, query)
            });
            // Title row `│▸ <title>`.
            assert_fit_then_cut(unit, inner - 2, "...", &|text| {
                cross_body(width, 3, "│▸ ", text, "e")
            });
            // Excerpt row `│    <excerpt>`.
            assert_fit_then_cut(unit, inner - 4, "...", &|text| {
                cross_body(width, 4, "│    ", "t", text)
            });
        }
    }
}

#[test]
fn k4_cross_search_highlight_paints_the_whole_original_grapheme() {
    let bold = |painted: &Painted, x: u16| {
        painted
            .buffer
            .cell((x, 4))
            .unwrap()
            .modifier
            .contains(Modifier::BOLD)
    };
    // `👨` is the first scalar of G; the whole family must carry the style.
    let painted = cross(40, "\u{1F468}", "t", &format!("x{G}y"));
    assert!(row_text(&painted.buffer, 4).starts_with(&format!("│    x{G}y")));
    assert!(!bold(&painted, 5), "x");
    assert_eq!(painted.buffer.cell((6, 4)).unwrap().symbol(), G);
    assert!(bold(&painted, 6), "whole family bold");
    assert!(!bold(&painted, 8), "y");
    // A match ending at the `e` of its combining accent (the overlay only
    // searches queries of two or more bytes).
    let painted = cross(40, "xe", "t", &format!("x{A}y"));
    assert!(row_text(&painted.buffer, 4).starts_with(&format!("│    x{A}y")));
    assert_eq!(painted.buffer.cell((6, 4)).unwrap().symbol(), A);
    assert!(bold(&painted, 5), "matched x");
    assert!(bold(&painted, 6), "accented cluster bold");
    assert!(!bold(&painted, 7), "y");
    // Overlapping logical matches (both ZWJ joiners inside one family): the
    // family is painted once and no byte is duplicated or dropped.
    let painted = cross(40, "\u{200d}", "t", &format!("x{G}y{G}"));
    assert!(row_text(&painted.buffer, 4).starts_with(&format!("│    x{G}y{G}")));
    assert_eq!(count_cluster(&screen(&painted.buffer), G), 2);
    assert!(
        bold(&painted, 6) && bold(&painted, 9),
        "both families whole"
    );
    assert!(!bold(&painted, 5) && !bold(&painted, 8));
    // ASCII positive control.
    let painted = cross(40, "bc", "t", "abcd");
    assert!(!bold(&painted, 5) && bold(&painted, 6) && bold(&painted, 7) && !bold(&painted, 8));
    assert_whole_clusters(&painted.buffer);
}

// ── bookmarks ──────────────────────────────────────────────────────────────

fn bookmarks(width: u16, text: &str) -> Painted {
    let mut message = make_msg("m0", MessageRole::User);
    message.content = text.to_string();
    let conversation = make_conversation(vec![message], vec![]);
    let theme = Theme::dark();
    paint(width, 5, |frame| {
        bookmark_list::render(frame, frame.area(), &conversation, &[0], 0, &theme);
    })
}

fn bookmark_body(width: u16, text: &str) -> String {
    let painted = bookmarks(width, text);
    let row = row_text(&painted.buffer, 2);
    let body = row
        .strip_prefix("│▸ msg 0: ")
        .and_then(|rest| rest.strip_suffix('│'))
        .unwrap_or_else(|| panic!("{width} {text:?}: {row:?}"))
        .trim_end()
        .to_string();
    assert_whole_clusters(&painted.buffer);
    assert_no_partial_family(&row);
    body
}

#[test]
fn k4_bookmark_rows_use_the_real_prefix_budget() {
    for width in WIDTHS {
        // inner - `▸ ` - `msg 0: `
        let budget = usize::from(width) - 2 - 2 - 7;
        for unit in UNITS {
            assert_fit_then_cut(unit, budget, "...", &|text| bookmark_body(width, text));
        }
    }
}

// ── command palette ────────────────────────────────────────────────────────

fn palette_entry(name: &str, shortcut: &str, scope: PaletteScope) -> PaletteEntry {
    PaletteEntry {
        name: name.to_string(),
        description: "d".to_string(),
        shortcut: Some(shortcut.to_string()),
        scope,
        action: PaletteAction::SwitchProfile("p".to_string()),
    }
}

/// Column at which the shortcut text starts on the first result row.
fn shortcut_column(width: u16, entry: PaletteEntry) -> u16 {
    let shortcut = entry.shortcut.clone().unwrap();
    let mut state = CommandPaletteState::new();
    state.active = true;
    state.filtered_entries = vec![entry];
    let theme = Theme::dark();
    let painted = paint(width, 30, |frame| {
        command_palette::render(frame, frame.area(), &state, &theme);
    });
    let mut x = 0u16;
    let mut symbols: Vec<(u16, String)> = Vec::new();
    while x < width {
        let symbol = painted.buffer.cell((x, 10)).unwrap().symbol().to_string();
        let step = u16::try_from(cells(&symbol).max(1)).unwrap();
        symbols.push((x, symbol));
        x += step;
    }
    symbols
        .iter()
        .enumerate()
        .find(|(i, _)| {
            let tail: String = symbols[*i..].iter().map(|(_, s)| s.as_str()).collect();
            tail.starts_with(&shortcut)
        })
        .map(|(_, (x, _))| *x)
        .unwrap_or_else(|| panic!("shortcut {shortcut:?} not painted in {symbols:?}"))
}

#[test]
fn k4_palette_shortcut_column_matches_the_ascii_control() {
    // Model entries: `{display} ({provider})` with `Ctrl+X, M`; profile
    // entries: `> {name}` with `(active)`. The overlay is 48 cells wide at 80
    // columns and clips the shortcut field of names wider than 29 cells, so
    // those names are checked at 100 and 130 columns only.
    let models_fit_80 = [
        "模型 (openai)".to_string(),
        format!("{G} (p)"),
        format!("{A}{A}{A}"),
        C.repeat(14),
        format!("{}x", C.repeat(14)),
    ];
    let models_wide_only = [C.repeat(15), C.repeat(16), format!("{}x", G.repeat(15))];
    let profiles_fit_80 = [
        format!("> {A}{G}"),
        format!("> {C}{G}{A}"),
        format!("> {}", C.repeat(13)),
    ];
    let profiles_wide_only = [format!("> {}", C.repeat(16))];
    for width in [80u16, 100, 130] {
        for name in &models_fit_80 {
            assert_same_column(width, name, "Ctrl+X, M", PaletteScope::Model);
        }
        for name in &profiles_fit_80 {
            assert_same_column(width, name, "(active)", PaletteScope::Profile);
        }
    }
    for width in [100u16, 130] {
        for name in &models_wide_only {
            assert_same_column(width, name, "Ctrl+X, M", PaletteScope::Model);
        }
        for name in &profiles_wide_only {
            assert_same_column(width, name, "(active)", PaletteScope::Profile);
        }
    }
}

/// The shortcut of `display` starts at the same column as that of an ASCII
/// name of the same cell width.
fn assert_same_column(width: u16, display: &str, shortcut: &str, scope: PaletteScope) {
    let control = "x".repeat(cells(display));
    let column = shortcut_column(width, palette_entry(display, shortcut, scope));
    let expected = shortcut_column(width, palette_entry(&control, shortcut, scope));
    assert_eq!(column, expected, "{width} {scope:?} {display:?}");
}
