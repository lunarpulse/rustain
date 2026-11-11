//! K2 (local host): the composer's height, wrap, caret, scroll and edits agree
//! when driven through the real front doors: `app::handle_input` events, then
//! `compute_layout` -> `input_area_height` and the real `input_box::render`,
//! reading the backend cursor and the painted rows. The attached host's
//! counterpart lives beside its private key/paste/draw seams in
//! `infrastructure::runtime::attach_loop::display_width_tests`.
use ratatui::layout::{Position, Rect};
use rustain::adapters::tui::{
    app::{InputAction, handle_input},
    layout::compute_layout,
    state::TuiState,
    theme::Theme,
    widgets::input_box,
};
use rustain::domain::events::{DomainInputEvent, DomainKey};
use rustain::domain::models::FocusState;

use super::support::*;

struct Drawn {
    rows: Vec<String>,
    caret: Position,
    input_area: Rect,
}

fn state_at(width: u16, height: u16) -> TuiState {
    let mut state = TuiState::new(width, height);
    state.focus = FocusState::Input;
    state
}

fn key(state: &mut TuiState, key: DomainKey) -> InputAction {
    handle_input(state, &DomainInputEvent::SpecialKey(key))
}

fn paste(state: &mut TuiState, text: &str) {
    assert_eq!(
        handle_input(state, &DomainInputEvent::Paste(text.to_owned())),
        InputAction::Consumed
    );
}

fn resize(state: &mut TuiState, width: u16, height: u16) {
    handle_input(state, &DomainInputEvent::Resize(width, height));
}

/// The production local composer draw: `compute_layout` decides the rectangle
/// (and so the height), `input_box::render` paints it with the state's scroll.
fn draw(state: &TuiState) -> Drawn {
    let (width, height) = (state.terminal_width, state.terminal_height);
    let theme = Theme::dark();
    let mut input_area = Rect::default();
    let painted = paint(width, height, |frame| {
        let layout = compute_layout(frame.area(), &theme, &state.input_buffer, 1, state.monitor)
            .expect("supported size");
        input_area = layout.input_area;
        input_box::render(
            frame,
            layout.input_area,
            &state.input_buffer,
            state.cursor_position,
            state.focus.clone(),
            &theme,
            false,
            state.input_scroll_offset,
            None,
        );
    });
    Drawn {
        rows: rows(&painted.buffer),
        caret: painted.cursor.expect("focused composer sets a caret"),
        input_area,
    }
}

fn text_rows(drawn: &Drawn) -> &[String] {
    let top = usize::from(drawn.input_area.y) + 1;
    let bottom = usize::from(drawn.input_area.y + drawn.input_area.height) - 1;
    &drawn.rows[top..bottom]
}

#[test]
fn k2_cjk_wraps_and_the_caret_follows_in_a_60_column_composer() {
    let mut state = state_at(60, 24);
    paste(&mut state, &C.repeat(30));
    let drawn = draw(&state);
    assert_eq!(drawn.input_area.height, 4, "two text rows + borders");
    let rows = text_rows(&drawn);
    assert_eq!(rows[0].matches(C).count(), 29, "{:?}", drawn.rows);
    assert_eq!(rows[1].matches(C).count(), 1, "the continuation paints");
    assert_eq!(
        (drawn.caret.x, drawn.caret.y),
        (drawn.input_area.x + 3, drawn.input_area.y + 2),
        "caret after the 30th glyph on the continuation row, inside the border"
    );
    // ASCII positive control at the same width.
    let mut ascii = state_at(60, 24);
    paste(&mut ascii, &"a".repeat(70));
    let drawn = draw(&ascii);
    assert_eq!(drawn.input_area.height, 4);
    assert_eq!(text_rows(&drawn)[0].matches('a').count(), 58);
    assert_eq!(text_rows(&drawn)[1].matches('a').count(), 12);
}

#[test]
fn k2_exact_full_final_row_gets_a_blank_caret_row() {
    let mut state = state_at(60, 24);
    paste(&mut state, &"a".repeat(58));
    let drawn = draw(&state);
    assert_eq!(
        drawn.input_area.height, 4,
        "58 cells fill the row; the caret needs one more"
    );
    assert_eq!(
        (drawn.caret.x, drawn.caret.y),
        (drawn.input_area.x + 1, drawn.input_area.y + 2)
    );
    // One cell fewer needs no slot row.
    let mut shy = state_at(60, 24);
    paste(&mut shy, &"a".repeat(57));
    assert_eq!(draw(&shy).input_area.height, 3);
}

#[test]
fn k2_grapheme_navigation_and_edits_never_split_g_or_a() {
    let mut state = state_at(80, 24);
    let text = format!("a{G}{A}b");
    paste(&mut state, &text);
    let scalars = text.chars().count();
    assert_eq!(state.cursor_position, scalars);
    // Left walks cluster by cluster: b | A | G | a.
    let mut stops = Vec::new();
    for _ in 0..4 {
        key(&mut state, DomainKey::Left);
        stops.push(state.cursor_position);
    }
    assert_eq!(stops, [scalars - 1, scalars - 3, 1, 0]);
    for expected in [1, 6, 8] {
        key(&mut state, DomainKey::Right);
        assert_eq!(state.cursor_position, expected);
    }
    // Backspace after A removes the whole accent cluster, then G.
    key(&mut state, DomainKey::Backspace);
    assert_eq!(state.input_buffer, format!("a{G}b"));
    key(&mut state, DomainKey::Backspace);
    assert_eq!(state.input_buffer, "ab");
    // Delete removes the cluster at the caret.
    let mut state = state_at(80, 24);
    paste(&mut state, &format!("{G}{A}x"));
    key(&mut state, DomainKey::Home);
    assert_eq!(state.cursor_position, 0);
    key(&mut state, DomainKey::Delete);
    assert_eq!(state.input_buffer, format!("{A}x"));
    key(&mut state, DomainKey::Delete);
    assert_eq!(state.input_buffer, "x");
    // The rendered caret sits after G at 2 cells, not 6.
    let mut caret = state_at(80, 24);
    paste(&mut caret, &format!("{G}x"));
    key(&mut caret, DomainKey::Left);
    let drawn = draw(&caret);
    assert_eq!(drawn.caret.x, drawn.input_area.x + 1 + 2);
}

#[test]
fn k2_up_and_down_move_by_visual_rows_and_stay_on_grapheme_stops() {
    let mut state = state_at(60, 24);
    paste(&mut state, &C.repeat(45)); // 90 cells: rows of 29 and 16 glyphs
    assert_eq!(state.cursor_position, 45);
    key(&mut state, DomainKey::Up);
    assert_eq!(
        state.cursor_position, 16,
        "same column (32 cells) one row up"
    );
    key(&mut state, DomainKey::Down);
    assert_eq!(state.cursor_position, 45);
    // Home goes to the start of the second visual row; Up from column 0 lands
    // on column 0 of the first row.
    key(&mut state, DomainKey::Home);
    assert_eq!(
        state.cursor_position, 29,
        "Home: start of the second visual row"
    );
    key(&mut state, DomainKey::Up);
    assert_eq!(
        state.cursor_position, 0,
        "Up from column 0 of row 2 lands at column 0 of row 1"
    );
    // On the first visual row, with no newline in the draft, Up recalls history
    // exactly as before the visual-row cutover (a soft wrap is not a newline).
    let mut recall = state_at(80, 24);
    recall.input_history.push("older entry".to_owned());
    paste(&mut recall, &"a".repeat(79)); // 78-cell inner width: wraps to two rows
    assert_eq!(draw(&recall).input_area.height, 4);
    key(&mut recall, DomainKey::Home);
    key(&mut recall, DomainKey::Up);
    assert_eq!(recall.cursor_position, 0, "row 2 -> row 1 is a caret move");
    assert_eq!(recall.input_buffer, "a".repeat(79));
    key(&mut recall, DomainKey::Up);
    assert_eq!(
        recall.input_buffer, "older entry",
        "first row + no newline: history"
    );
    assert_eq!(recall.cursor_position, "older entry".chars().count());
    // A target column inside C's two cells lands on C's start.
    let mut mixed = state_at(80, 24);
    mixed.multiline_mode = true;
    paste(&mut mixed, &format!("ab\n{C}{C}"));
    mixed.cursor_position = 1; // before b, column 1
    key(&mut mixed, DomainKey::Down);
    assert_eq!(
        mixed.cursor_position, 3,
        "column 1 is inside the first C: its start"
    );
}

#[test]
fn k2_monitor_sidebar_narrows_the_composer_that_height_and_wrap_use() {
    let mut state = state_at(130, 30);
    state.monitor = true;
    paste(&mut state, &C.repeat(45)); // 90 cells: fits 128, not the ~91-column main rect
    let drawn = draw(&state);
    assert_eq!(
        drawn.input_area.height, 4,
        "wraps in the sidebar-narrowed rectangle"
    );
    assert!(drawn.input_area.width < 130);
    assert_eq!(
        text_rows(&drawn)[1].matches(C).count(),
        1,
        "{:?}",
        drawn.rows
    );
    // Without the sidebar the same draft fits one 128-cell row.
    let mut plain = state_at(130, 30);
    paste(&mut plain, &C.repeat(45));
    assert_eq!(draw(&plain).input_area.height, 3);
}

#[test]
fn k2_resize_rewraps_and_keeps_the_caret_visible() {
    let mut state = state_at(130, 30);
    paste(&mut state, &C.repeat(45));
    assert_eq!(draw(&state).input_area.height, 3);
    resize(&mut state, 60, 24);
    state.terminal_height = 24;
    let drawn = draw(&state);
    assert_eq!(drawn.input_area.height, 4);
    assert_eq!(drawn.caret.y, drawn.input_area.y + 2);
}

#[test]
fn k2_more_than_eight_rows_scroll_in_visual_rows() {
    let mut state = state_at(60, 24);
    state.multiline_mode = true;
    let body: String = (0..12).map(|i| format!("row{i:02}\n")).collect::<String>() + "last";
    paste(&mut state, &body);
    let drawn = draw(&state);
    assert_eq!(drawn.input_area.height, 10, "eight text rows + borders");
    let rows = text_rows(&drawn);
    assert!(rows[7].contains("last"), "{:?}", drawn.rows);
    assert!(
        rows[0].contains("row05"),
        "the tail window starts at row 5: {rows:?}"
    );
    assert_eq!(drawn.caret.y, drawn.input_area.y + 8);
    // Moving up past the window scrolls back by one visual row.
    for _ in 0..8 {
        key(&mut state, DomainKey::Up);
    }
    let drawn = draw(&state);
    assert!(text_rows(&drawn)[0].contains("row04"), "{:?}", drawn.rows);
    // A soft-wrapped logical line counts as several visual rows for scrolling.
    let mut wrapped = state_at(60, 24);
    paste(&mut wrapped, &"w".repeat(58 * 10));
    let drawn = draw(&wrapped);
    assert_eq!(drawn.input_area.height, 10);
    assert_eq!(
        drawn.caret.y,
        drawn.input_area.y + 8,
        "the end slot row is the last visible row"
    );
}

#[test]
fn k2_blank_lines_and_tabs_have_one_projection() {
    let mut state = state_at(80, 24);
    state.multiline_mode = true;
    paste(&mut state, "a\n\n\tb");
    let drawn = draw(&state);
    assert_eq!(drawn.input_area.height, 5, "three rows: a, blank, tabbed b");
    let rows = text_rows(&drawn);
    assert!(
        rows[2].contains("        b"),
        "tab expands to the next stop: {rows:?}"
    );
    assert_eq!(drawn.caret.x, drawn.input_area.x + 1 + 9);
    // The source keeps the tab byte.
    assert_eq!(state.input_buffer, "a\n\n\tb");
}

#[test]
fn k2_small_widget_boundary_control_packs_whole_glyphs_at_inner_width_three() {
    // Five-column outer rect -> inner width 3. Not reachable through
    // compute_layout (it rejects terminals below 60); this drives the widget.
    let ccc = C.repeat(3);
    assert_eq!(
        input_box::input_area_height(&ccc, 5),
        5,
        "three text rows + borders"
    );
    let theme = Theme::dark();
    let painted = paint(5, 5, |frame| {
        input_box::render(
            frame,
            frame.area(),
            &ccc,
            3,
            FocusState::Input,
            &theme,
            false,
            0,
            None,
        );
    });
    let rows = rows(&painted.buffer);
    assert_eq!(
        [&rows[1], &rows[2], &rows[3]].map(|row| row.matches(C).count()),
        [1, 1, 1],
        "{rows:?}"
    );
    assert_whole_clusters(&painted.buffer);
}

#[test]
fn k2_history_autocomplete_and_original_bytes_are_preserved() {
    let mut state = state_at(80, 24);
    let draft = format!("line {G}{A}\ttab 界");
    paste(&mut state, &draft);
    match key(&mut state, DomainKey::Enter) {
        InputAction::SubmitMessage(text) => {
            assert_eq!(text, draft, "submission carries the original source bytes")
        }
        other => panic!("expected SubmitMessage, got {other:?}"),
    }
    assert!(state.input_buffer.is_empty());
    // History recall puts the cursor at the end of the recalled draft.
    key(&mut state, DomainKey::Up);
    assert_eq!(state.input_buffer, draft);
    assert_eq!(state.cursor_position, draft.chars().count());
    // A recalled wide draft paints and keeps its caret in the box.
    let drawn = draw(&state);
    assert!(drawn.caret.x < drawn.input_area.x + drawn.input_area.width - 1);
}
