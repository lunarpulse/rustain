//! The Adapters window (Story 21.7 A21, AC14): the port and MCP server health rows the sidebar
//! panel used to paint, in a frame of their own at every width. Read-only: `j k` and the RU-25
//! scroll keys move, `Esc` goes back.
//!
//! The rows are a snapshot taken when the window opens ([`AdaptersWindowState::new`]; the event
//! loop reads every loaded adapter's `health_snapshot()` once): opening it again reads again.
//! There is no timer behind the window (no new poll, AD-2111), so a health change while it is
//! open shows at the next opening; the rail says nothing live about it.

use std::cell::Cell;

use crate::adapters::tui::app::InputAction;
use crate::adapters::tui::keymap::table_mission::MissionCmd;
use crate::adapters::tui::keymap::{self, CommandId, Context, Host, Key, Resolved};
use crate::adapters::tui::presentation::viewer::{self, Viewer, ViewerCmd};
use crate::adapters::tui::presentation::{Surface, reducer};
use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::widgets::adapter_status_panel::AdapterLine;

#[derive(Debug)]
pub struct AdaptersWindowState {
    /// The health snapshot taken at open.
    pub rows: Vec<AdapterLine>,
    pub viewer: Viewer,
    /// `(painted lines, viewport rows)` of the last paint: the keys clamp against it.
    fit: Cell<(usize, usize)>,
}

impl AdaptersWindowState {
    #[must_use]
    pub fn new(rows: Vec<AdapterLine>) -> Self {
        Self {
            rows,
            viewer: Viewer::default(),
            fit: Cell::new((0, 0)),
        }
    }

    #[must_use]
    pub fn fit(&self) -> (usize, usize) {
        self.fit.get()
    }

    pub fn set_fit(&self, lines: usize, viewport: usize) {
        self.fit.set((lines, viewport));
    }
}

/// Open (or refresh) the Adapters window with a fresh snapshot.
pub fn open(state: &mut TuiState, rows: Vec<AdapterLine>) {
    reducer::open_adapters(state, AdaptersWindowState::new(rows));
}

/// The Adapters window is the top frame.
#[must_use]
pub fn is_top(state: &TuiState) -> bool {
    state
        .modal_stack
        .top()
        .is_some_and(|frame| frame.surface == Surface::Adapters)
}

pub(crate) fn on_key(state: &mut TuiState, key: Key, host: Host) -> InputAction {
    if !is_top(state) {
        return InputAction::Consumed;
    }
    let Resolved::Hit(row) = keymap::resolve(Context::Adapters, host, &[key]) else {
        return InputAction::Consumed;
    };
    state.needs_redraw = true;
    match row.command {
        CommandId::Mission(MissionCmd::Back) => {
            reducer::back(state);
        }
        CommandId::Viewer(command) if !command.horizontal() => scroll(state, command),
        _ => {}
    }
    InputAction::Consumed
}

fn scroll(state: &mut TuiState, command: ViewerCmd) {
    if let Some(window) = state.adapters_window.as_mut() {
        let (rows, viewport) = window.fit();
        viewer::apply(&mut window.viewer, command, rows, viewport);
    }
}

/// A wheel step scrolls like the line keys (state-only).
pub(crate) fn on_wheel(
    state: &mut TuiState,
    delta: crate::domain::models::view_state::ScrollDelta,
) {
    if let Some(window) = state.adapters_window.as_mut() {
        let (rows, viewport) = window.fit();
        viewer::apply_scroll(&mut window.viewer, delta, rows, viewport);
        state.needs_redraw = true;
    }
}

/// The raw press-only rule of the frame (RU-29): only a plain Press acts, and a Ctrl letter
/// only where the registry binds it.
pub(crate) fn ignores_raw_key(state: &TuiState, key: &crossterm::event::KeyEvent) -> bool {
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
    if !is_top(state) {
        return false;
    }
    if key.kind != KeyEventKind::Press {
        return true;
    }
    match key.code {
        KeyCode::Char(c) => {
            let plain = key.modifiers.difference(KeyModifiers::SHIFT).is_empty();
            !plain && !(key.modifiers == KeyModifiers::CONTROL && "xpqcdu".contains(c))
        }
        _ => !key.modifiers.is_empty(),
    }
}
