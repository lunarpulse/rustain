//! The one key executor of the top Mission Control frame (local host). A key resolves in
//! `Context::MissionControl` (its own rows, then the RU-25 rows of `InspectorBody`); a key the
//! registry has no row for, and a row that cannot act now, is consumed: never the composer,
//! never the Stage, never a card beneath.

use super::{McPane, McTab};
use crate::adapters::tui::app::InputAction;
use crate::adapters::tui::keymap::table_mission::MissionCmd;
use crate::adapters::tui::keymap::{self, CommandId, Context, Host, Key, Resolved};
use crate::adapters::tui::presentation::reducer;
use crate::adapters::tui::presentation::viewer::{self, ViewerCmd};
use crate::adapters::tui::state::TuiState;

pub(crate) fn on_key(state: &mut TuiState, key: Key, host: Host) -> InputAction {
    if !super::is_top(state) {
        return InputAction::Consumed;
    }
    let Resolved::Hit(row) = keymap::resolve(Context::MissionControl, host, &[key]) else {
        return InputAction::Consumed;
    };
    state.needs_redraw = true;
    match row.command {
        CommandId::Mission(MissionCmd::Back) => {
            reducer::back(state);
        }
        CommandId::Mission(MissionCmd::PrevTab) => step_tab(state, false),
        CommandId::Mission(MissionCmd::NextTab) => step_tab(state, true),
        CommandId::Mission(MissionCmd::Focus) => toggle_pane(state),
        CommandId::Viewer(command) if !command.horizontal() => scroll(state, command),
        _ => {}
    }
    InputAction::Consumed
}

/// A wheel step scrolls the body like the line keys (state-only, never a close).
pub(crate) fn on_wheel(
    state: &mut TuiState,
    delta: crate::domain::models::view_state::ScrollDelta,
) {
    use crate::domain::models::view_state::ScrollDelta::{WheelDown, WheelUp};
    let (up, lines) = match delta {
        WheelUp(lines) => (true, lines),
        WheelDown(lines) => (false, lines),
        _ => return,
    };
    for _ in 0..lines {
        scroll(
            state,
            if up {
                ViewerCmd::LineUp
            } else {
                ViewerCmd::LineDown
            },
        );
    }
    state.needs_redraw = true;
}

/// `Left`/`Right`: the previous or next delivered tab, wrapping; inert with one tab.
fn step_tab(state: &mut TuiState, forward: bool) {
    let delivered = state.mission_control.delivered();
    let tabs = delivered.as_slice();
    if tabs.len() < 2 {
        return;
    }
    let at = tabs
        .iter()
        .position(|tab| *tab == state.mission_control.tab)
        .unwrap_or(0);
    let next = if forward {
        (at + 1) % tabs.len()
    } else {
        (at + tabs.len() - 1) % tabs.len()
    };
    reducer::set_mission_tab(state, tabs[next]);
}

/// `Tab`: master ⇄ detail, only where the painted frame showed both.
fn toggle_pane(state: &mut TuiState) {
    let mission = &mut state.mission_control;
    if !mission.tab.is_list() || !mission.fit().wide {
        return;
    }
    mission.pane = match mission.pane {
        McPane::Master => McPane::Detail,
        McPane::Detail => McPane::Master,
    };
}

/// The RU-25 rows by value: a body tab scrolls, a list tab moves its selection (master) or
/// scrolls its detail preview.
fn scroll(state: &mut TuiState, command: ViewerCmd) {
    match state.mission_control.tab {
        McTab::Room => room_scroll(state, command),
        McTab::Probe => {
            let mission = &mut state.mission_control;
            match (mission.pane, mission.fit().wide) {
                (McPane::Detail, true) => {
                    let fit = mission.fit();
                    viewer::apply(
                        &mut mission.detail,
                        command,
                        fit.detail_rows,
                        fit.detail_viewport,
                    );
                }
                _ => mission.move_selection(command),
            }
        }
    }
}

/// The Room body scrolls by node rows; the cursor follows the first painted row so the next
/// paint keeps the position and a refold rebases it by identity.
fn room_scroll(state: &mut TuiState, command: ViewerCmd) {
    let room = &mut state.room_panel;
    let mut position = viewer::Viewer {
        top: room.scroll_offset,
        ..Default::default()
    };
    viewer::apply(
        &mut position,
        command,
        room.visible_len(),
        room.viewport_rows,
    );
    room.scroll_offset = position.top;
    let mut cursor = position.top;
    room.synchronize_selection(&mut cursor);
    state.mission_control.room_cursor = cursor;
}

/// The raw press-only rule of the frame (RU-29): conversion discards kind and modifiers, so the
/// host's raw pre-filter asks this first. Only a plain Press acts: a Release, a Repeat and an
/// Alt variant never do, and a Ctrl letter acts only where the registry binds it (`Ctrl+X`,
/// `Ctrl+P`, `Ctrl+Q`, `Ctrl+C`, `Ctrl+D`, `Ctrl+U`); shifted printables (`G`) resolve normally.
pub(crate) fn ignores_raw_key(state: &TuiState, key: &crossterm::event::KeyEvent) -> bool {
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
    if !super::is_top(state) {
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
