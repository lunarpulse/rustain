//! The one key executor of the top Mission Control frame (both hosts). A key resolves in
//! `Context::MissionControl` (its own rows, then the RU-25 rows of `InspectorBody`); a key the
//! registry has no row for, and a row that cannot act now, is consumed: never the composer,
//! never the Stage, never a card beneath. The filter field of Messages is a text context of its
//! own (`Context::InspectorSearch`, routed before this executor).

use super::{McPane, McTab};
use crate::adapters::tui::app::InputAction;
use crate::adapters::tui::keymap::table_mission::MissionCmd;
use crate::adapters::tui::keymap::table_overlay::OverlayCmd;
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
        CommandId::Mission(MissionCmd::Back) => back(state),
        CommandId::Mission(MissionCmd::PrevTab) => return step_tab(state, false, host),
        CommandId::Mission(MissionCmd::NextTab) => return step_tab(state, true, host),
        CommandId::Mission(MissionCmd::Focus) => toggle_pane(state),
        // The filter and the export act only where the rail painted their hints whole.
        CommandId::Overlay(OverlayCmd::InspectorSearch) => {
            let mission = &mut state.mission_control;
            if mission.tab == McTab::Messages
                && mission.fit().search
                && let Some(page) = mission.messages.as_mut()
            {
                page.search.open();
            }
        }
        CommandId::Mission(MissionCmd::Export) => {
            let mission = &state.mission_control;
            if mission.tab == McTab::Messages
                && mission.fit().export
                && mission
                    .messages
                    .as_ref()
                    .is_some_and(|page| page.report.is_some())
            {
                return InputAction::ExportTransparency;
            }
        }
        CommandId::Mission(MissionCmd::Open) => {
            let mission = &state.mission_control;
            if mission.tab == McTab::Artifacts
                && mission.fit().open
                && let Some(id) = mission.selected_id()
            {
                return InputAction::OpenArtifactWindow(id.to_owned());
            }
        }
        CommandId::Viewer(command) if !command.horizontal() => scroll(state, command),
        _ => {}
    }
    InputAction::Consumed
}

/// `Esc` / `Ctrl+C`: a kept Messages filter clears first, then the frame backs.
fn back(state: &mut TuiState) {
    let kept = state.mission_control.tab == McTab::Messages
        && state
            .mission_control
            .messages
            .as_ref()
            .is_some_and(|page| !page.search.query.is_empty());
    if kept {
        super::clear_search(state);
    } else {
        reducer::back(state);
    }
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

/// `Left`/`Right`: the previous or next delivered tab, wrapping; inert with one tab. The new
/// tab's source is read by the host (`InputAction::OpenMissionControl`).
fn step_tab(state: &mut TuiState, forward: bool, host: Host) -> InputAction {
    let delivered = state.mission_control.delivered(host);
    let tabs = delivered.as_slice();
    if tabs.len() < 2 {
        return InputAction::Consumed;
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
    InputAction::OpenMissionControl(tabs[next])
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
        McTab::Messages => {
            if let Some(page) = state.mission_control.messages.as_mut() {
                let (rows, viewport) = page.fit();
                // A pending jump-to-end (`follow`, set by `G`) is resolved against the live
                // fit, so relative moves start from the real bottom.
                if page.viewer.follow {
                    page.viewer.top = usize::MAX;
                }
                viewer::apply(&mut page.viewer, command, rows, viewport);
            }
        }
        McTab::Artifacts | McTab::Probe => {
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
/// The open filter field is a text context with readline chords of its own: the guard stands
/// aside there (like every text field of the thread window).
pub(crate) fn ignores_raw_key(state: &TuiState, key: &crossterm::event::KeyEvent) -> bool {
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
    if !super::is_top(state) {
        return false;
    }
    if super::search_editing(state) {
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
