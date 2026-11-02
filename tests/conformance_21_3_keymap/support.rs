//! Shared fixtures for the 21-3 keymap conformance module. Every leg drives a
//! key through the REAL front door: crossterm event -> `convert_crossterm_event`
//! -> `app::handle_input` (never a hand-built `DomainInputEvent`/`InputAction`).
#![allow(dead_code)]

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use rustain::adapters::tui::app::{InputAction, convert_crossterm_event, handle_input};
use rustain::adapters::tui::keymap::Key;
use rustain::adapters::tui::state::TuiState;
use rustain::domain::events::DomainInputEvent;
use rustain::domain::models::MouseConfig;

/// The `KeyEvent` a terminal produces for `key` (crossterm 0.29 decoding of the
/// canonical PTY bytes: `0x08` is `Ctrl+H`, `0x0a` is `Ctrl+J`, ...).
pub fn key_event(key: Key) -> KeyEvent {
    let none = KeyModifiers::NONE;
    match key {
        Key::Char(c) => KeyEvent::new(
            KeyCode::Char(c),
            if c.is_ascii_uppercase() {
                KeyModifiers::SHIFT
            } else {
                none
            },
        ),
        Key::Ctrl(c) => KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL),
        Key::Alt(c) => KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT),
        Key::Enter => KeyEvent::new(KeyCode::Enter, none),
        Key::ShiftEnter => KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
        Key::CtrlEnter => KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
        Key::AltEnter => KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
        Key::Esc => KeyEvent::new(KeyCode::Esc, none),
        Key::Tab => KeyEvent::new(KeyCode::Tab, none),
        Key::ShiftTab => KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
        Key::Backspace => KeyEvent::new(KeyCode::Backspace, none),
        Key::Delete => KeyEvent::new(KeyCode::Delete, none),
        Key::Up => KeyEvent::new(KeyCode::Up, none),
        Key::Down => KeyEvent::new(KeyCode::Down, none),
        Key::Left => KeyEvent::new(KeyCode::Left, none),
        Key::Right => KeyEvent::new(KeyCode::Right, none),
        Key::Home => KeyEvent::new(KeyCode::Home, none),
        Key::End => KeyEvent::new(KeyCode::End, none),
        Key::PageUp => KeyEvent::new(KeyCode::PageUp, none),
        Key::PageDown => KeyEvent::new(KeyCode::PageDown, none),
        Key::F(n) => KeyEvent::new(KeyCode::F(n), none),
        Key::Other => KeyEvent::new(KeyCode::Insert, none),
        Key::Combo(c, bits) => {
            let held = [
                (1, KeyModifiers::CONTROL),
                (2, KeyModifiers::ALT),
                (4, KeyModifiers::SHIFT),
            ];
            let mods = held
                .iter()
                .filter(|(bit, _)| bits & bit != 0)
                .fold(KeyModifiers::NONE, |all, (_, m)| all | *m);
            KeyEvent::new(KeyCode::Char(c), mods)
        }
    }
}

/// Decode `key` exactly as the local host does (None = the host drops it).
pub fn decode(key: Key) -> Option<DomainInputEvent> {
    convert_crossterm_event(&Event::Key(key_event(key)), &MouseConfig::default())
}

/// Press `key` through the local front door and return the action it produced.
pub fn press(state: &mut TuiState, key: Key) -> InputAction {
    match decode(key) {
        Some(event) => handle_input(state, &event),
        None => InputAction::Ignored,
    }
}

/// Press a sequence of keys (`Ctrl+X` then `h`, `z` then `a`, ...).
pub fn press_all(state: &mut TuiState, keys: &[Key]) -> Vec<InputAction> {
    keys.iter().map(|key| press(state, *key)).collect()
}

/// A fresh 80x24 local state (default Focus density, Input focus).
pub fn fresh() -> TuiState {
    TuiState::new(80, 24)
}

/// True when the action is one a "no command fires" sweep tolerates.
pub fn is_inert(action: &InputAction) -> bool {
    matches!(action, InputAction::Consumed | InputAction::Ignored)
}

/// True when `key` reaches its command in `context` through the Global parent
/// (`Ctrl+X` leader, `Ctrl+Q` quit): a sweep asserts that effect instead of
/// skipping the key.
pub fn global_owned(context: rustain::adapters::tui::keymap::Context, key: Key) -> bool {
    use rustain::adapters::tui::keymap::{Context, Host, Resolved, resolve};
    matches!(resolve(context, Host::Local, &[key]),
        Resolved::Hit(row) if row.context == Context::Global)
}

/// The effect of a Global row on `state`: `Ctrl+X` opens the leader, `Ctrl+Q`
/// quits.
pub fn assert_global_effect(state: &mut TuiState, key: Key) {
    let action = press(state, key);
    match key {
        Key::Ctrl('x') => assert!(state.pending_leader.is_some(), "Ctrl+X opens the leader"),
        Key::Ctrl('q') => assert_eq!(action, InputAction::Quit, "Ctrl+Q quits"),
        other => panic!("{other:?} is not a Global key"),
    }
}

/// A retained fan-out wave of `.0` spokes, for the wave witnesses.
#[derive(Debug)]
pub struct StubWave(pub usize);

impl rustain::domain::ports::wave_handle::WaveHandle for StubWave {
    fn snapshot(&self) -> rustain::domain::ports::wave_handle::WaveSnapshot {
        use rustain::domain::models::orchestration::{ForkJoinOutcome, SynthesisView};
        rustain::domain::ports::wave_handle::WaveSnapshot {
            spoke_count: self.0,
            completed: 0,
            honest_empty: Some(self.0 == 0),
            cancelled: false,
            outcome: ForkJoinOutcome {
                spokes: Vec::new(),
                synthesis: SynthesisView::default(),
            },
            resolve_count: 0,
            slots: Vec::new(),
            rerun_counts: Vec::new(),
        }
    }
    fn cancel(&self) {}
    fn drill(&self, _slot: usize) -> Option<rustain::domain::ports::wave_handle::DrillBody> {
        None
    }
    fn drill_id(&self, _slot: usize) -> Option<rustain::domain::models::orchestration::DrillId> {
        None
    }
    fn slots(&self) -> Vec<rustain::domain::models::AgentId> {
        Vec::new()
    }
    fn rerun_count_for_slot(&self, _slot: usize) -> u8 {
        0
    }
    fn resolve_count(&self) -> usize {
        0
    }
}
