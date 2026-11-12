//! Shared fixtures for the 21-14 keystones. Every key travels as a crossterm event through the
//! local host's decoder (`handle_input` / `handle_operator_input`), `/tour` goes through the
//! production parser and the one call the event loop's arm makes, a due card opens through the
//! tick the event loop runs, and every assertion reads painted cells, the one frame on the stack
//! or the state file. No sleeps: time is a `MockClock`, the state file lives in a tempdir.
#![allow(dead_code)]

use crossterm::event::{Event, KeyEvent, KeyEventKind, KeyModifiers};
use rustain::adapters::tui::app::{
    InputAction, convert_crossterm_event, handle_operator_input, submit_message_for_test,
};
use rustain::adapters::tui::keymap::Key;
use rustain::adapters::tui::tour::{self, Phase, TourStep};
use rustain::domain::models::{FocusState, MouseConfig, Turn};
use std::path::PathBuf;

pub use crate::conformance_21_5_tool_inspector::support::{Fixture, prose};

/// A conversation with prose only (no tool call), the composer focused.
pub fn plain(width: u16, height: u16) -> Fixture {
    let mut turn = Turn::new("fixture-model".into(), 0);
    prose(&mut turn, "A short conversation.");
    let mut f = Fixture::new(width, height, turn);
    f.h.state.focus = FocusState::Input;
    f
}

/// A scratch config directory with a state file path inside it.
pub struct Config {
    pub dir: tempfile::TempDir,
    pub file: PathBuf,
}

impl Config {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("scratch config dir");
        let file = dir.path().join("state.toml");
        Self { dir, file }
    }

    pub fn text(&self) -> String {
        std::fs::read_to_string(&self.file).unwrap_or_default()
    }

    pub fn done(&self) -> bool {
        rustain::infrastructure::tui_state_file::load(&self.file).tour_done
    }
}

/// The local launch: the offer installed the way `event_loop::run` installs it.
pub fn launch_with_offer(f: &mut Fixture, config: &Config) {
    tour::install_at_startup(&mut f.h.state, true, Ok(config.file.clone()), false);
}

/// `/tour` through the production parser and the call the event loop's arm makes.
pub fn slash_tour(f: &mut Fixture) {
    f.h.state.input_buffer = "/tour".into();
    let action = submit_message_for_test(&mut f.h.state);
    assert_eq!(
        action,
        InputAction::ExecuteCommand {
            name: "tour".into(),
            args: None
        },
        "/tour must use the production command parser"
    );
    tour::command(&mut f.h.state);
    f.paint();
}

/// One tick of the event loop (not streaming), then one completed draw.
pub fn tick(f: &mut Fixture) {
    tour::tick(&mut f.h.state, false);
    f.paint();
}

/// A tick that opens the due card: the typing grace has long passed on a fresh fixture.
pub fn card_opens(f: &mut Fixture) {
    tick(f);
    assert_eq!(
        top_title(f).as_deref().map(|t| t.starts_with("Tour")),
        Some(true)
    );
}

pub fn top_title(f: &Fixture) -> Option<String> {
    f.h.state
        .modal_stack
        .top()
        .map(|frame| frame.title().to_owned())
}

pub fn step(f: &Fixture) -> Option<(TourStep, Phase)> {
    f.h.state.tour.as_ref().map(|tour| (tour.step, tour.phase))
}

/// The painted rows, read cell by cell (the harness text helper splits wide glyphs by bytes).
pub fn rows(f: &Fixture) -> Vec<String> {
    let buffer = f.buffer();
    let area = buffer.area;
    (area.y..area.bottom())
        .map(|y| {
            (area.x..area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

/// The card's text as one whitespace-normalised string: borders dropped, wrapped rows joined,
/// so a sentence that wrapped is still found whole.
pub fn card_text(f: &Fixture) -> String {
    rows(f)
        .iter()
        .map(|row| row.replace(['│', '┌', '┐', '└', '┘', '─'], " "))
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The painted row that holds `needle`.
pub fn row_with(f: &Fixture, needle: &str) -> Option<String> {
    rows(f).into_iter().find(|row| row.contains(needle))
}

/// A real key through the local front door, then one draw.
pub fn press(f: &mut Fixture, key: Key) -> InputAction {
    let action = f.press(key);
    f.paint();
    action
}

pub fn press_all(f: &mut Fixture, keys: &[Key]) {
    for key in keys {
        press(f, *key);
    }
}

/// Type `text` into the composer through the decoder.
pub fn type_text(f: &mut Fixture, text: &str) {
    for c in text.chars() {
        f.press(Key::Char(c));
    }
}

/// A raw terminal event through the local host's ownership filter and converter (the way the
/// event loop feeds `handle_operator_input`).
pub fn raw(f: &mut Fixture, event: KeyEvent) -> InputAction {
    let raw = Event::Key(event);
    let Some(domain) = convert_crossterm_event(&raw, &MouseConfig::default()) else {
        return InputAction::Ignored;
    };
    handle_operator_input(&mut f.h.state, &raw, &domain).unwrap_or(InputAction::Ignored)
}

pub fn key_event(
    code: crossterm::event::KeyCode,
    modifiers: KeyModifiers,
    kind: KeyEventKind,
) -> KeyEvent {
    KeyEvent::new_with_kind(code, modifiers, kind)
}

/// The newest toast text, if any.
pub fn newest_notice(
    f: &Fixture,
) -> Option<(
    rustain::adapters::tui::notifications::NoticeSeverity,
    String,
)> {
    f.h.state
        .notifications
        .records()
        .next_back()
        .map(|record| (record.severity, record.message.clone()))
}
