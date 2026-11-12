//! The tour's ambient guidance (Story 21.14 D13, D14) while a step waits for its real action.
//!
//! The coach line is one borderless row over the top row of the Stage (`Tour <k>/5 ·
//! <instruction> · /tour`); it reserves nothing, paints only inside the chat pane, yields to a
//! toast that reaches its row, and is painted before the frames so any frame dims it. The
//! highlight adds `REVERSED | BOLD` to the cells of the step's target after the status row is
//! painted: symbols are never touched, so it works in monochrome and `NO_COLOR`, and a target
//! that is not painted is not highlighted. Text is located by matching the generated string inside
//! its owner's rectangle only, never across the screen.

use ratatui::prelude::*;
use ratatui::widgets::{Clear, Paragraph};

use crate::adapters::tui::decision_queue;
use crate::adapters::tui::keymap::Host;
use crate::adapters::tui::keymap::render::StatusTail;
use crate::adapters::tui::keymap::render_tips::inspect_tip;
use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::text_geometry as geom;
use crate::adapters::tui::tour::{Phase, TourStep, copy};
use crate::adapters::tui::widgets::{mission_strip, toast_layer};
use crate::domain::models::Conversation;

/// Between the segments of the coach line.
const SEPARATOR: &str = " \u{B7} ";

/// What the highlight adds to its target cells.
const MARK: Modifier = Modifier::REVERSED.union(Modifier::BOLD);

/// The rectangles the highlight searches, one owner each.
#[derive(Debug, Clone, Copy)]
pub struct Targets {
    pub chat_pane: Rect,
    pub input_area: Rect,
    pub status_bar: Rect,
    pub mission_strip: Option<Rect>,
}

/// The coach line for a pane `width` cells wide as `(label, tail)`: the whole line, else without
/// the trailing `/tour`, else the instruction ellipsized grapheme-safely, never below the label;
/// `None` when even the label does not fit.
#[must_use]
pub fn fit(label: &str, instruction: &str, slash: &str, width: usize) -> Option<(String, String)> {
    let label_cells = geom::cells(label);
    if width < label_cells {
        return None;
    }
    let short = format!("{SEPARATOR}{instruction}");
    if !slash.is_empty() {
        let full = format!("{short}{SEPARATOR}{slash}");
        if label_cells + geom::cells(&full) <= width {
            return Some((label.to_owned(), full));
        }
    }
    if label_cells + geom::cells(&short) <= width {
        return Some((label.to_owned(), short));
    }
    let budget = width.saturating_sub(label_cells + geom::cells(SEPARATOR));
    if budget >= 2 {
        let kept = geom::ellipsize(instruction, budget, "\u{2026}");
        return Some((label.to_owned(), format!("{SEPARATOR}{kept}")));
    }
    Some((label.to_owned(), String::new()))
}

/// The fitted coach text of the last paint, keyed by what it depends on (step, pane width and the
/// composer's multi-line mode, which picks the send key): the text is rebuilt only when one of
/// them changes, never per repaint.
struct Cached {
    key: (TourStep, u16, bool),
    line: Option<(String, String)>,
}

thread_local! {
    static CACHE: std::cell::RefCell<Option<Cached>> = const { std::cell::RefCell::new(None) };
    static BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many times this thread rebuilt the coach text (tests pin that unchanged repaints do not).
#[must_use]
pub fn text_builds() -> usize {
    BUILDS.with(std::cell::Cell::get)
}

/// Paint the coach line over the top row of `pane` while a step is armed. Nothing is painted
/// without a tour, in any other phase, for a pane of no rows, or when a toast holds the row.
pub fn paint_coach(frame: &mut Frame<'_>, state: &TuiState, pane: Rect) {
    let Some(tour) = state.tour.as_ref() else {
        return;
    };
    if tour.phase != Phase::Armed
        || pane.width == 0
        || pane.height == 0
        || toast_layer::reaches_top_row(pane, &state.notifications)
    {
        return;
    }
    let key = (tour.step, pane.width, state.multiline_mode);
    CACHE.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.as_ref().is_none_or(|cached| cached.key != key) {
            BUILDS.with(|builds| builds.set(builds.get() + 1));
            let label = format!("Tour {}/{}", tour.step.number(), TourStep::COUNT);
            let instruction = copy::coach_instruction(tour.step, state.multiline_mode);
            let line = fit(
                &label,
                &instruction,
                &copy::slash_command(),
                usize::from(pane.width),
            );
            *slot = Some(Cached { key, line });
        }
        let Some((label, tail)) = slot.as_ref().and_then(|cached| cached.line.as_ref()) else {
            return;
        };
        let colors = &state.theme.colors;
        let row = Rect::new(pane.x, pane.y, pane.width, 1);
        frame.render_widget(Clear, row);
        let line = Line::from(vec![
            Span::styled(
                label.as_str(),
                Style::default()
                    .fg(colors.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(tail.as_str(), Style::default().fg(colors.fg_muted)),
        ]);
        frame.render_widget(
            Paragraph::new(line).style(Style::default().bg(colors.bg_surface)),
            row,
        );
    });
}

/// Highlight the armed step's target after every base row and the status row are painted: the
/// composer's top border (send), the `[Enter] Inspect` cue in the Stage, the status cue of a
/// waiting decision, the mission strip's text (else the status row's leader-menu text) for the
/// hub step, the leader-menu text for help. Only with an empty stack.
pub fn paint_highlight(
    frame: &mut Frame<'_>,
    state: &TuiState,
    targets: &Targets,
    host: Host,
    stage: Option<&Conversation>,
) {
    let Some(tour) = state.tour.as_ref() else {
        return;
    };
    if tour.phase != Phase::Armed || !state.modal_stack.is_empty() {
        return;
    }
    let buffer = frame.buffer_mut();
    let menu = || {
        StatusTail::Menu
            .text()
            .and_then(|text| find(buffer, targets.status_bar, &text))
    };
    let target = match tour.step {
        TourStep::Offer => None,
        TourStep::Send => {
            let input = targets.input_area;
            (input.width > 0 && input.height > 0)
                .then(|| Rect::new(input.x, input.y, input.width, 1))
        }
        TourStep::Inspect => find_tool_cue(buffer, state, targets.chat_pane),
        TourStep::Decide => decision_queue::status_cue(decision_queue::waiting(state))
            .and_then(|cue| find(buffer, targets.status_bar, &cue)),
        TourStep::Hub => targets
            .mission_strip
            .and_then(|strip| {
                let facts = mission_strip::facts(state, host, stage);
                let text = mission_strip::text(&facts, host, strip.width);
                find(buffer, strip, &text)
            })
            .or_else(menu),
        TourStep::Help => menu(),
    };
    if let Some(rect) = target {
        buffer.set_style(rect, Style::default().add_modifier(MARK));
    }
}

/// The `[Enter] Inspect` cue of the focused **tool call**: the Stage paints it as a dim span at
/// the end of the focused item's `▸` header line. Code/table blocks and reasoning headers paint
/// the identical text but Enter opens another frame there, and transcript prose may spell it, so
/// the match needs a focused tool id (neither a block nor a reasoning id), the focus marker earlier
/// on the row and dim cue cells.
fn find_tool_cue(buffer: &ratatui::buffer::Buffer, state: &TuiState, pane: Rect) -> Option<Rect> {
    use crate::domain::models::turn::{is_block_item_id, is_reasoning_item_id};
    let id = state.focused_item_id.as_deref()?;
    if is_block_item_id(id) || is_reasoning_item_id(id) {
        return None;
    }
    let cue = inspect_tip()?;
    (pane.y..pane.bottom()).find_map(|y| {
        let hit = find(buffer, Rect::new(pane.x, y, pane.width, 1), &cue)?;
        let marked = (pane.x..hit.x).any(|x| {
            buffer
                .cell((x, y))
                .is_some_and(|cell| cell.symbol() == "\u{25B8}")
        });
        (marked
            && buffer
                .cell((hit.x, hit.y))?
                .modifier
                .contains(Modifier::DIM))
        .then_some(hit)
    })
}

/// The rectangle of the rightmost occurrence of `needle` inside `area`, one cell per character
/// (every generated target is single-width text; anything else is not located).
fn find(buffer: &ratatui::buffer::Buffer, area: Rect, needle: &str) -> Option<Rect> {
    let count = u16::try_from(needle.chars().count()).ok()?;
    if count == 0 || usize::from(count) != geom::cells(needle) || area.width < count {
        return None;
    }
    for y in area.y..area.bottom() {
        for x in (area.x..=area.right() - count).rev() {
            let hit = needle.char_indices().enumerate().all(|(offset, (at, ch))| {
                let width = u16::try_from(offset).unwrap_or(u16::MAX);
                buffer
                    .cell((x + width, y))
                    .is_some_and(|cell| cell.symbol() == &needle[at..at + ch.len_utf8()])
            });
            if hit {
                return Some(Rect::new(x, y, count, 1));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::keymap::Key;
    use crate::adapters::tui::tour;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn armed(step_presses: usize) -> TuiState {
        let mut state = TuiState::new(80, 24);
        tour::command(&mut state);
        for _ in 0..step_presses {
            tour::on_key(&mut state, Key::Char('n'), Host::Local);
        }
        tour::on_key(&mut state, Key::Enter, Host::Local);
        assert_eq!(state.tour.as_ref().unwrap().phase, Phase::Armed);
        state
    }

    fn row_text(buffer: &ratatui::buffer::Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol().to_owned())
            .collect()
    }

    fn marked(buffer: &ratatui::buffer::Buffer, y: u16) -> Vec<u16> {
        (0..buffer.area.width)
            .filter(|&x| buffer[(x, y)].modifier.contains(MARK))
            .collect()
    }

    #[test]
    fn story21_14_the_coach_line_drops_the_slash_then_ellipsizes_never_below_the_label() {
        let (label, instruction) = ("Tour 3/5", "Answer a decision with Ctrl+X, Q");
        let line = |width| fit(label, instruction, "/tour", width).map(|(a, b)| a + &b);
        assert_eq!(
            line(80).unwrap(),
            "Tour 3/5 \u{B7} Answer a decision with Ctrl+X, Q \u{B7} /tour"
        );
        assert_eq!(
            line(43).unwrap(),
            "Tour 3/5 \u{B7} Answer a decision with Ctrl+X, Q"
        );
        let cut = line(30).unwrap();
        assert!(
            cut.starts_with("Tour 3/5 \u{B7} Answer") && cut.ends_with('\u{2026}'),
            "{cut}"
        );
        assert!(geom::cells(&cut) <= 30, "{cut}");
        assert_eq!(line(10).unwrap(), "Tour 3/5");
        assert_eq!(line(8).unwrap(), "Tour 3/5");
        assert_eq!(line(7), None, "never below the label");
        for width in 8..120 {
            let text = line(width).unwrap();
            assert!(geom::cells(&text) <= width, "{width}: {text}");
            assert!(text.starts_with("Tour 3/5"), "{width}: {text}");
        }
    }

    #[test]
    fn story21_14_the_coach_line_paints_the_top_pane_row_only_while_a_step_is_armed() {
        let state = armed(0);
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        let pane = Rect::new(0, 1, 80, 8);
        terminal
            .draw(|frame| paint_coach(frame, &state, pane))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        assert!(row_text(&buffer, 1).starts_with("Tour 1/5 \u{B7} Send a message with "));
        assert!(row_text(&buffer, 1).trim_end().ends_with("/tour"));
        for y in [0u16, 2, 8, 9, 10, 11] {
            assert!(row_text(&buffer, y).trim().is_empty(), "row {y} untouched");
        }
        // The card is open: nothing is armed, nothing is painted.
        let mut card = TuiState::new(80, 24);
        tour::command(&mut card);
        card.tour.as_mut().unwrap().phase = Phase::Card;
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| paint_coach(frame, &card, pane))
            .unwrap();
        assert!(row_text(terminal.backend().buffer(), 1).trim().is_empty());
    }

    #[test]
    fn story21_14_the_highlight_marks_exactly_the_located_cells_and_keeps_every_symbol() {
        let state = armed(4);
        let targets = Targets {
            chat_pane: Rect::new(0, 0, 40, 6),
            input_area: Rect::new(0, 8, 40, 3),
            status_bar: Rect::new(0, 7, 40, 1),
            mission_strip: None,
        };
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(Paragraph::new("Normal   ^X menu"), targets.status_bar);
                paint_highlight(frame, &state, &targets, Host::Local, None);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(
            marked(buffer, 7),
            (9..16).collect::<Vec<u16>>(),
            "`^X menu`"
        );
        assert!(row_text(buffer, 7).starts_with("Normal   ^X menu"));
        for y in (0..12).filter(|&y| y != 7) {
            assert!(marked(buffer, y).is_empty(), "row {y}");
        }
    }

    #[test]
    fn story21_14_a_target_that_is_not_painted_is_not_highlighted() {
        let state = armed(2);
        let targets = Targets {
            chat_pane: Rect::new(0, 0, 40, 6),
            input_area: Rect::new(0, 8, 40, 3),
            status_bar: Rect::new(0, 7, 40, 1),
            mission_strip: None,
        };
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(Paragraph::new("Normal   ^X menu"), targets.status_bar);
                paint_highlight(frame, &state, &targets, Host::Local, None);
            })
            .unwrap();
        for y in 0..12 {
            assert!(marked(terminal.backend().buffer(), y).is_empty(), "row {y}");
        }
    }

    #[test]
    fn story21_14_the_hub_step_marks_the_strip_text_else_the_menu_text() {
        let state = armed(3);
        assert_eq!(state.tour.as_ref().unwrap().step, TourStep::Hub);
        let strip = Rect::new(0, 6, 40, 1);
        let targets = |mission_strip| Targets {
            chat_pane: Rect::new(0, 0, 40, 6),
            input_area: Rect::new(0, 8, 40, 3),
            status_bar: Rect::new(0, 7, 40, 1),
            mission_strip,
        };
        let paint = |mission_strip| {
            let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
            terminal
                .draw(|frame| {
                    frame.render_widget(Paragraph::new("^X s Mission Control"), strip);
                    frame.render_widget(Paragraph::new("Normal   ^X menu"), Rect::new(0, 7, 40, 1));
                    paint_highlight(frame, &state, &targets(mission_strip), Host::Local, None);
                })
                .unwrap();
            terminal.backend().buffer().clone()
        };
        let with_strip = paint(Some(strip));
        assert_eq!(
            marked(&with_strip, 6),
            (0..20).collect::<Vec<u16>>(),
            "the strip's text"
        );
        assert!(marked(&with_strip, 7).is_empty(), "not the status row");
        // The strip yielded its row (`None`): the generated menu text is the target instead.
        let yielded = paint(None);
        assert!(marked(&yielded, 6).is_empty());
        assert_eq!(marked(&yielded, 7), (9..16).collect::<Vec<u16>>());
        // A strip rect that does not hold the painted text highlights nothing of its own.
        let torn = Rect::new(0, 5, 40, 1);
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(Paragraph::new("^X s Mission Control"), strip);
                paint_highlight(frame, &state, &targets(Some(torn)), Host::Local, None);
            })
            .unwrap();
        assert!(marked(terminal.backend().buffer(), 6).is_empty());
        assert!(marked(terminal.backend().buffer(), 5).is_empty());
    }

    #[test]
    fn story21_14_unchanged_repaints_reuse_the_coach_text_and_a_change_rebuilds_once() {
        let mut state = armed(0);
        let paint = |state: &TuiState, width: u16| {
            let mut terminal = Terminal::new(TestBackend::new(width, 6)).unwrap();
            terminal
                .draw(|frame| paint_coach(frame, state, Rect::new(0, 1, width, 4)))
                .unwrap();
            row_text(terminal.backend().buffer(), 1)
        };
        let first = paint(&state, 80);
        let base = text_builds();
        for _ in 0..5 {
            assert_eq!(paint(&state, 80), first, "same text on every repaint");
        }
        assert_eq!(text_builds(), base, "unchanged repaints build nothing");
        paint(&state, 60);
        assert_eq!(text_builds(), base + 1, "a width change rebuilds once");
        paint(&state, 60);
        assert_eq!(text_builds(), base + 1);
        state.multiline_mode = true;
        assert!(
            paint(&state, 60).contains("Send a message with"),
            "multi-line send key"
        );
        assert_eq!(text_builds(), base + 2, "the send-key mode rebuilds once");
        state.tour.as_mut().unwrap().step = TourStep::Help;
        paint(&state, 60);
        assert_eq!(text_builds(), base + 3, "a step change rebuilds once");
    }

    #[test]
    fn story21_14_find_takes_the_rightmost_match_in_its_own_rect() {
        let mut terminal = Terminal::new(TestBackend::new(30, 3)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(
                    Paragraph::new("^X menu hint ^X menu"),
                    Rect::new(0, 1, 30, 1),
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(
            find(buffer, Rect::new(0, 0, 30, 3), "^X menu"),
            Some(Rect::new(13, 1, 7, 1))
        );
        assert_eq!(
            find(buffer, Rect::new(0, 0, 12, 3), "^X menu"),
            Some(Rect::new(0, 1, 7, 1))
        );
        assert_eq!(find(buffer, Rect::new(0, 0, 6, 3), "^X menu"), None);
        assert_eq!(find(buffer, Rect::new(0, 0, 30, 1), "^X menu"), None);
    }
}
