//! K7 — the coach line and the step highlight (AC9, AC10, AC13, D13, D14). The production frame
//! pipeline (`event_loop::render` through `Fixture::paint`) paints every frame; assertions read
//! painted cells, their modifiers and the layout the production `compute_layout` returns.
use super::support::*;
use crate::conformance_21_5_tool_inspector::support::{call, rows as stage_rows};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use rustain::adapters::tui::keymap::{Host, Key};
use rustain::adapters::tui::layout::AppLayout;
use rustain::adapters::tui::notification_log::LogFacts;
use rustain::adapters::tui::notifications::{self, NoticeSource};
use rustain::adapters::tui::presentation::{self, Shell};
use rustain::adapters::tui::tour::{Phase, TourStep, copy};
use rustain::adapters::tui::widgets::tour_coach;
use rustain::domain::models::{FocusState, InvocationStatus, Turn, WriteDiffState};
use serde_json::json;

const MARK: Modifier = Modifier::REVERSED.union(Modifier::BOLD);
const STEPS: [TourStep; 5] = [
    TourStep::Send,
    TourStep::Inspect,
    TourStep::Decide,
    TourStep::Hub,
    TourStep::Help,
];
/// Words the native drivers wait on and the strip's own grammar: the coach line never says them.
const FORBIDDEN: [&str; 9] = [
    "Ready",
    "Needs you",
    "Read-only.",
    "Decision:",
    "waiting",
    "^X s Mission Control",
    "Agents ",
    "Tasks ",
    "Decisions ",
];

fn armed_at(f: &mut Fixture, number: usize) {
    slash_tour(f);
    for _ in 1..number {
        press(f, Key::Char('n'));
    }
    press(f, Key::Enter);
    assert_eq!(step(f), Some((STEPS[number - 1], Phase::Armed)));
    assert!(f.h.state.modal_stack.is_empty());
}

fn row_of(buffer: &Buffer, y: u16) -> String {
    (buffer.area.x..buffer.area.right())
        .map(|x| buffer[(x, y)].symbol())
        .collect()
}

fn text_rows(buffer: &Buffer) -> Vec<String> {
    (buffer.area.y..buffer.area.bottom())
        .map(|y| row_of(buffer, y))
        .collect()
}

/// The columns of row `y` whose cells carry the highlight modifiers.
fn marked(f: &Fixture, y: u16) -> Vec<u16> {
    let buffer = f.buffer();
    (buffer.area.x..buffer.area.right())
        .filter(|&x| buffer[(x, y)].modifier.contains(MARK))
        .collect()
}

fn any_marked(f: &Fixture) -> bool {
    let area = f.buffer().area;
    (area.y..area.bottom()).any(|y| !marked(f, y).is_empty())
}

/// The painted row's text inside `[from, to)` columns.
fn slice(buffer: &Buffer, y: u16, from: u16, to: u16) -> String {
    (from..to).map(|x| buffer[(x, y)].symbol()).collect()
}

/// The columns a located needle covers in row `y` (rightmost match).
fn span_of(f: &Fixture, y: u16, needle: &str) -> Vec<u16> {
    let buffer = f.buffer();
    let width = needle.chars().count() as u16;
    for x in (0..=buffer.area.width - width).rev() {
        if slice(buffer, y, x, x + width) == needle {
            return (x..x + width).collect();
        }
    }
    panic!(
        "{needle:?} is not painted on row {y}: {:?}",
        row_of(buffer, y)
    );
}

#[test]
fn coach_text_is_generated_per_step_at_the_viewports_with_the_monitor_off_and_on() {
    for (width, height, monitor) in [
        (60u16, 16u16, false),
        (80, 24, false),
        (130, 30, false),
        (130, 30, true),
    ] {
        for (index, step) in STEPS.iter().enumerate() {
            let mut f = plain(width, height);
            f.h.state.monitor = monitor;
            f.paint();
            let before = text_rows(f.buffer());
            armed_at(&mut f, index + 1);
            let layout = f.layout();
            let pane = layout.chat_pane;
            let buffer = f.buffer();
            let row = slice(buffer, pane.y, pane.x, pane.right());
            let instruction = copy::coach_instruction(*step, false);
            let label = format!("Tour {}/5", index + 1);
            assert!(
                row.starts_with(&format!("{label} \u{B7} {instruction}")),
                "{width}x{height} monitor={monitor} step {}: {row:?}",
                index + 1
            );
            assert!(
                row.trim_end().ends_with("/tour"),
                "the slash text closes the line when it fits: {row:?}"
            );
            for word in FORBIDDEN {
                assert!(!row.contains(word), "{word:?} in {row:?}");
            }
            // The coach row never reaches the monitor column, the strip row or the status row.
            let after = text_rows(buffer);
            for y in 0..height {
                let (old, new) = (&before[usize::from(y)], &after[usize::from(y)]);
                if y == pane.y {
                    assert_eq!(
                        slice(buffer, y, pane.right(), buffer.area.right()),
                        old.chars()
                            .skip(usize::from(pane.right()))
                            .collect::<String>(),
                        "row {y}: only the chat pane changes"
                    );
                } else {
                    assert_eq!(old, new, "row {y} is untouched by the coach line");
                }
            }
            if let Some(strip) = layout.mission_strip {
                assert_eq!(before[usize::from(strip.y)], after[usize::from(strip.y)]);
            }
        }
    }
}

#[test]
fn coach_line_never_changes_stage_geometry() {
    for (width, height, monitor) in [(60u16, 16u16, false), (80, 24, false), (130, 30, true)] {
        let mut f = plain(width, height);
        f.h.state.monitor = monitor;
        f.paint();
        let layout_before = rects(&f.layout());
        let cache_before = f.cache();
        let content_before = f.h.state.total_content_height;
        let view_before = f.view();
        armed_at(&mut f, 1);
        assert_eq!(
            rects(&f.layout()),
            layout_before,
            "compute_layout is unchanged"
        );
        assert_eq!(f.cache(), cache_before, "height caches are unchanged");
        assert_eq!(f.h.state.total_content_height, content_before);
        assert_eq!(f.view(), view_before, "scroll and anchor are unchanged");
        // The line is on the first row of the chat pane itself, not on a row taken from it.
        let pane = f.layout().chat_pane;
        assert!(
            row_of(f.buffer(), pane.y).contains("Tour 1/5"),
            "the coach line owns the top row of the chat pane"
        );
    }
}

fn rects(layout: &AppLayout) -> Vec<Option<Rect>> {
    vec![
        Some(layout.chat_pane),
        layout.mission_strip,
        Some(layout.status_bar),
        Some(layout.input_area),
        layout.tab_bar,
        layout.sidebar,
    ]
}

#[test]
fn the_coach_line_exists_only_while_a_step_is_armed() {
    let mut f = plain(80, 24);
    f.paint();
    let pane = f.layout().chat_pane;
    assert!(
        !row_of(f.buffer(), pane.y).contains("Tour"),
        "no tour, no line"
    );
    slash_tour(&mut f);
    assert!(
        !row_of(f.buffer(), pane.y).contains("Tour 1/5"),
        "the card is open: the line is absent"
    );
    press(&mut f, Key::Enter);
    assert!(row_of(f.buffer(), pane.y).contains("Tour 1/5"));
    // A real send completes the step: the next card is due, the line is gone.
    f.h.state.input_buffer = "hello".into();
    rustain::adapters::tui::app::submit_message_for_test(&mut f.h.state);
    f.paint();
    assert_eq!(step(&f), Some((TourStep::Inspect, Phase::CardDue)));
    assert!(!row_of(f.buffer(), pane.y).contains("Tour 1/5"));
}

/// Compose a one-row chat pane (a pane short enough for a toast to reach its top row).
fn compose_short_pane(f: &mut Fixture) -> Buffer {
    let area = f.buffer().area;
    let layout = AppLayout {
        chat_pane: Rect::new(0, 0, area.width, 1),
        mission_strip: None,
        status_bar: Rect::new(0, 1, area.width, 1),
        input_area: Rect::new(0, 2, area.width, 3),
        tab_bar: None,
        sidebar: None,
    };
    let state = &f.h.state;
    f.h.terminal
        .draw(|frame| {
            let shell = Shell::new(frame.area(), &layout);
            presentation::compose(
                frame,
                state,
                &shell,
                Host::Local,
                &LogFacts::default(),
                &mut presentation::occlusion::OpaqueRects::new(shell.chat_pane),
                |_| {},
            );
        })
        .unwrap();
    f.buffer().clone()
}

#[test]
fn the_coach_line_yields_to_a_toast_on_its_row() {
    let mut f = plain(60, 16);
    armed_at(&mut f, 1);
    let bare = compose_short_pane(&mut f);
    assert!(
        row_of(&bare, 0).starts_with("Tour 1/5"),
        "no toast: the line shows"
    );
    notifications::info(
        &mut f.h.state,
        NoticeSource::Help,
        "A toast on the top row".to_owned(),
    );
    let with_toast = compose_short_pane(&mut f);
    let row = row_of(&with_toast, 0);
    assert!(row.contains("A toast on the top row"), "{row:?}");
    assert!(!row.contains("Tour 1/5"), "the line yields: {row:?}");
}

#[test]
fn the_coach_line_is_dimmed_under_any_frame_and_the_highlight_stays_off() {
    // 130 columns: a centred frame leaves the first cells of the pane's top row uncovered.
    let mut f = plain(130, 30);
    armed_at(&mut f, 1);
    assert!(any_marked(&f), "the bare shell highlights the target");
    press_all(&mut f, &[Key::Ctrl('x'), Key::Char('?')]);
    assert!(!f.h.state.modal_stack.is_empty(), "help is open");
    let pane = f.layout().chat_pane;
    let cell = &f.buffer()[(pane.x, pane.y)];
    assert_eq!(
        cell.symbol(),
        "T",
        "the coach line is still painted underneath"
    );
    assert!(
        cell.modifier.contains(Modifier::DIM),
        "a frame dims the coach line"
    );
    assert!(!any_marked(&f), "no highlight with a frame open");
}

#[test]
fn highlight_only_on_the_bare_shell() {
    let mut f = plain(80, 24);
    armed_at(&mut f, 1);
    let composer = f.layout().input_area;
    assert_eq!(
        marked(&f, composer.y),
        (composer.x..composer.right()).collect::<Vec<u16>>(),
        "bare shell: the composer's top border row"
    );
    // The palette (a frame) hides it; closing it brings it back.
    press(&mut f, Key::Ctrl('p'));
    assert!(!any_marked(&f), "frame open");
    press(&mut f, Key::Esc);
    assert_eq!(
        marked(&f, composer.y),
        (composer.x..composer.right()).collect::<Vec<u16>>()
    );
    // Without a tour nothing is highlighted anywhere.
    let mut idle = plain(80, 24);
    idle.paint();
    assert!(!any_marked(&idle));
}

#[test]
fn the_highlight_keeps_every_symbol_and_marks_only_its_target() {
    let mut f = plain(80, 24);
    f.paint();
    let before = text_rows(f.buffer());
    armed_at(&mut f, 1);
    let composer = f.layout().input_area;
    let pane = f.layout().chat_pane;
    let after = text_rows(f.buffer());
    for (y, (old, new)) in before.iter().zip(&after).enumerate() {
        if y == usize::from(pane.y) {
            continue; // the coach line replaces this row's text
        }
        assert_eq!(old, new, "row {y}: symbols are byte-identical");
        if y != usize::from(composer.y) {
            assert!(marked(&f, y as u16).is_empty(), "row {y} is not the target");
        }
    }
}

#[test]
fn step_two_highlights_the_inspect_cue_only_when_a_tool_line_paints_it() {
    // Prose first, so the tool line sits below the pane's top row (the coach line's row).
    let mut turn = Turn::new("fixture-model".into(), 0);
    prose(&mut turn, &stage_rows(30));
    let id = call(
        &mut turn,
        "Bash",
        json!({"command": "echo hi"}),
        InvocationStatus::Success,
        Some(("OUT", WriteDiffState::NotAWrite)),
    );
    let mut f = Fixture::new(80, 24, turn);
    armed_at(&mut f, 2);
    f.h.state.focus = FocusState::Input;
    f.paint();
    assert!(!any_marked(&f), "no focused tool line: no highlight");
    f.h.state.focus = FocusState::Chat;
    f.focus_call(&id);
    // The highlight reads the focus the previous frame resolved: one more frame settles it.
    f.paint();
    let buffer = f.buffer();
    let pane = f.layout().chat_pane;
    let y = (pane.y..pane.bottom())
        .find(|&y| row_of(buffer, y).contains("[Enter] Inspect"))
        .expect("the focused tool line paints its generated cue");
    assert_eq!(marked(&f, y), span_of(&f, y, "[Enter] Inspect"));
    for other in (0..24).filter(|&other| other != y) {
        assert!(marked(&f, other).is_empty(), "row {other}");
    }
}

#[test]
fn step_three_highlights_the_status_cue_only_when_a_decision_waits() {
    use rustain::adapters::tui::decision_queue::{
        self, Arrival, DecisionDraft, DecisionKind, Raised,
    };
    let mut f = plain(80, 24);
    armed_at(&mut f, 3);
    assert!(!any_marked(&f), "nothing waits: nothing is highlighted");
    f.h.state.input_buffer = "draft".into();
    assert_eq!(
        decision_queue::raise(
            &mut f.h.state,
            DecisionDraft::new(DecisionKind::Budget, "budget detail"),
            Arrival::Automatic,
        ),
        Raised::Waiting
    );
    f.h.state.input_buffer.clear();
    f.paint();
    let status = f.layout().status_bar.y;
    assert!(row_of(f.buffer(), status).contains("Needs you (1) ^X q"));
    assert_eq!(
        marked(&f, status),
        span_of(&f, status, "Needs you (1) ^X q")
    );
}

#[test]
fn step_four_highlights_the_strip_text_and_never_paints_a_fallback_for_a_yielded_strip() {
    let mut f = plain(80, 24);
    armed_at(&mut f, 4);
    let layout = f.layout();
    let strip = layout.mission_strip.expect("80x24 reserves the strip");
    assert_eq!(
        marked(&f, strip.y),
        span_of(&f, strip.y, "^X s Mission Control"),
        "the strip's painted text"
    );
    assert!(
        marked(&f, layout.status_bar.y).is_empty(),
        "not the status row"
    );
    // A full composer at 60x16 takes the strip's row away: the target is the status row's
    // generated `^X menu` text when it is painted, and nothing at all when it is not (a target
    // that is not painted is not highlighted; no fallback paint).
    let mut f = plain(60, 16);
    armed_at(&mut f, 4);
    let mut draft = String::new();
    while f.layout().mission_strip.is_some() {
        draft.push_str("a line of the draft\n");
        f.h.state.input_buffer = draft.clone();
        assert!(draft.len() < 400, "the composer never filled the screen");
    }
    f.paint();
    let layout = f.layout();
    assert!(layout.mission_strip.is_none(), "the strip yielded its row");
    let status = layout.status_bar.y;
    if row_of(f.buffer(), status).contains("^X menu") {
        assert_eq!(marked(&f, status), span_of(&f, status, "^X menu"));
    } else {
        assert!(
            marked(&f, status).is_empty(),
            "the menu text is not painted here"
        );
    }
    assert!(!any_marked_outside(&f, status), "nothing else is marked");
}

fn any_marked_outside(f: &Fixture, row: u16) -> bool {
    let area = f.buffer().area;
    (area.y..area.bottom()).any(|y| y != row && !marked(f, y).is_empty())
}

#[test]
fn step_five_highlights_the_leader_menu_text() {
    let mut f = plain(80, 24);
    armed_at(&mut f, 5);
    let status = f.layout().status_bar.y;
    assert_eq!(marked(&f, status), span_of(&f, status, "^X menu"));
    assert!(!any_marked_outside(&f, status));
}

#[test]
fn the_highlight_is_modifiers_only_so_monochrome_and_no_color_still_show_it() {
    let mut f = plain(80, 24);
    armed_at(&mut f, 5);
    let status = f.layout().status_bar.y;
    let x = *marked(&f, status).first().expect("a marked cell");
    let cell = &f.buffer()[(x, status)];
    assert!(cell.modifier.contains(Modifier::REVERSED), "reverse video");
    assert!(cell.modifier.contains(Modifier::BOLD), "bold");
}

#[test]
fn fit_never_overflows_and_drops_in_the_documented_order() {
    let instruction = copy::coach_instruction(TourStep::Decide, false);
    for width in 8..=200usize {
        let (label, tail) =
            tour_coach::fit("Tour 3/5", &instruction, "/tour", width).expect("fits the label");
        let text = format!("{label}{tail}");
        assert!(
            rustain::adapters::tui::text_geometry::cells(&text) <= width,
            "{width}: {text}"
        );
        assert!(text.starts_with("Tour 3/5"), "{width}: {text}");
        if text.ends_with("/tour") {
            assert!(text.contains(&instruction), "the slash goes first: {text}");
        }
    }
    assert!(tour_coach::fit("Tour 3/5", &instruction, "/tour", 7).is_none());
}

#[test]
fn the_offer_and_step_cards_keep_title_and_rail_at_the_three_viewports() {
    for (width, height) in [(60u16, 16u16), (80, 24), (130, 30)] {
        let mut f = plain(width, height);
        slash_tour(&mut f);
        for number in 1..=5 {
            let text = card_text(&f);
            let rail = rows(&f)
                .into_iter()
                .find(|row| row.contains("[Enter] Continue"));
            assert!(rail.is_some(), "{width}x{height} card {number}: {text}");
            assert!(
                text.contains(&format!("Tour \u{B7} {number} of 5")),
                "{text}"
            );
            if number < 5 {
                press(&mut f, Key::Char('n'));
            }
        }
    }
}

/// The painted rows (by index) that carry the highlight.
fn marked_rows(f: &Fixture) -> Vec<u16> {
    let area = f.buffer().area;
    (area.y..area.bottom())
        .filter(|&y| !marked(f, y).is_empty())
        .collect()
}

#[test]
fn step_two_never_marks_prose_text_that_spells_the_cue() {
    // A literal cue in the transcript, no tool call at all: nothing is highlighted.
    let mut turn = Turn::new("fixture-model".into(), 0);
    prose(
        &mut turn,
        "Intro line.\n\nPress [Enter] Inspect to look closer.",
    );
    let mut f = Fixture::new(80, 24, turn);
    f.h.state.focus = FocusState::Chat;
    armed_at(&mut f, 2);
    f.h.state.focus = FocusState::Chat;
    f.paint();
    assert!(
        row_of_text(&f, "[Enter] Inspect").is_some(),
        "the prose spells the cue"
    );
    assert!(!any_marked(&f), "prose text is not a tool-call target");
}

#[test]
fn step_two_marks_only_the_focused_tool_cue_not_prose_above_it() {
    let mut turn = Turn::new("fixture-model".into(), 0);
    prose(
        &mut turn,
        &format!("Press [Enter] Inspect now.\n\n{}", stage_rows(30)),
    );
    let id = call(
        &mut turn,
        "Bash",
        json!({"command": "echo hi"}),
        InvocationStatus::Success,
        Some(("OUT", WriteDiffState::NotAWrite)),
    );
    let mut f = Fixture::new(80, 24, turn);
    armed_at(&mut f, 2);
    f.h.state.focus = FocusState::Chat;
    f.focus_call(&id);
    f.paint();
    let rows = marked_rows(&f);
    assert_eq!(rows.len(), 1, "exactly the tool line's cue: {rows:?}");
    let y = rows[0];
    assert!(
        row_of(f.buffer(), y).contains("Bash"),
        "{}",
        row_of(f.buffer(), y)
    );
    assert_eq!(marked(&f, y), span_of(&f, y, "[Enter] Inspect"));
}

#[test]
fn step_two_never_marks_a_code_block_cue() {
    use crate::conformance_21_13_block_viewer::support as block_support;
    let (turn, ids) = block_support::mixed_turn();
    let mut f = Fixture::new(80, 24, turn);
    armed_at(&mut f, 2);
    f.h.state.focus = FocusState::Chat;
    block_support::focus(&mut f, &ids[0]);
    f.paint();
    assert!(
        row_of_text(&f, "[Enter] Inspect").is_some(),
        "the focused code block paints the identical cue"
    );
    assert!(
        !any_marked(&f),
        "Enter on a block opens S-BLOCK: not the step's action"
    );
}

fn row_of_text(f: &Fixture, needle: &str) -> Option<u16> {
    let area = f.buffer().area;
    (area.y..area.bottom()).find(|&y| row_of(f.buffer(), y).contains(needle))
}

/// AC13: the one production card painter fed CJK and emoji text (title, body, wide glyphs at
/// every wrap boundary) inside the production frame rect at the three viewports. The title, a
/// body row and the rail survive, nothing is painted outside the rect, and the status row and
/// strip rows stay uncovered.
#[test]
fn cjk_and_emoji_cards_keep_title_body_and_rail_inside_their_frame_at_the_three_viewports() {
    use rustain::adapters::tui::widgets::tour_card;
    let squash = |text: &str| text.split_whitespace().collect::<String>();
    let wide = "界界界界界界界界界界界界界界界界界界界界界界界界界界界界界界界界界界界界界界界";
    let paragraphs = vec![
        format!("{wide} 🚀🚀🚀🚀🚀🚀 mixed 日本語とemoji 👩‍💻 text"),
        "ひらがなカタカナ 한국어 😀".to_owned(),
    ];
    for (width, height) in [(60u16, 16u16), (80, 24), (130, 30)] {
        for (title, step) in [
            ("Tour 界🚀", TourStep::Offer),
            ("Tour · 界 of 5 🚀", TourStep::Send),
        ] {
            let mut f = plain(width, height);
            f.paint();
            let layout = f.layout();
            let area = f.buffer().area;
            let rect = presentation::geometry::frame_rect(presentation::geometry::modal_region(
                &layout, area,
            ));
            let blank = ratatui::buffer::Cell::default();
            let theme = f.h.state.theme.clone();
            let items = copy::rail_items(step);
            f.h.terminal
                .draw(|frame| {
                    // The base is repainted by the production pipeline before; here only the card.
                    tour_card::paint(frame, rect, &theme, title, &paragraphs, items.clone());
                })
                .unwrap();
            let after = f.buffer().clone();
            let label = format!("{width}x{height} {title}");
            let shown = |y: u16| shown_row(&after, y);
            assert!(
                squash(&shown(rect.y)).contains(&squash(title)),
                "{label}: title on the frame rail: {:?}",
                shown(rect.y)
            );
            let body = squash(&shown(rect.y + 1).replace(['│', '┌', '┐'], ""));
            assert!(!body.is_empty(), "{label}: a body row");
            let rail = squash(&shown(rect.bottom() - 2));
            assert!(rail.contains("[Enter]Continue"), "{label}: rail `{rail}`");
            assert!(rect.right() <= area.right() && rect.bottom() <= area.bottom());
            for y in area.y..area.bottom() {
                for x in area.x..area.right() {
                    let inside =
                        x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom();
                    if !inside {
                        assert_eq!(
                            after[(x, y)],
                            blank,
                            "{label}: cell ({x},{y}) outside the card changed"
                        );
                    }
                }
            }
            let status = layout.status_bar.y;
            assert!(
                rect.bottom() <= status,
                "{label}: the card never covers the status row"
            );
            if let Some(strip) = layout.mission_strip {
                assert!(rect.bottom() <= strip.y, "{label}: nor the strip row");
            }
        }
    }
}

/// A painted row as a reader sees it: the cell after a double-width glyph is its continuation and
/// carries no text of its own.
fn shown_row(buffer: &Buffer, y: u16) -> String {
    let mut out = String::new();
    let mut skip = false;
    for x in buffer.area.x..buffer.area.right() {
        if skip {
            skip = false;
            continue;
        }
        let symbol = buffer[(x, y)].symbol();
        skip = rustain::adapters::tui::text_geometry::cells(symbol) > 1;
        out.push_str(symbol);
    }
    out
}
