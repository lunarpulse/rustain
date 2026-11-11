//! Frame-owned Wave list, drill and divergence effects. Retained runs outlive Back.

use crate::adapters::tui::app::{self, InputAction};
use crate::adapters::tui::keymap::{
    CommandId, Context, Host, Key, Resolved, resolve, table_chat::ChatCmd, table_panels::PanelCmd,
};
use crate::adapters::tui::presentation::{self, Surface, reducer};
use crate::adapters::tui::state::TuiState;

/// Wave panel commands only; opening borrows a frame, never the base focus.
pub fn command(state: &mut TuiState, cmd: PanelCmd) -> InputAction {
    match cmd {
        PanelCmd::WaveOpenOverlay | PanelCmd::WaveOpenFromChat => {
            reducer::open(state, Surface::WaveOverlay);
            state.needs_redraw = true;
            InputAction::OpenWaveOverlay
        }
        PanelCmd::WaveOpenDiverge | PanelCmd::WaveDiverge => {
            if state.wave_run.is_some() {
                reducer::open(state, Surface::WaveOverlay);
            }
            state.needs_redraw = true;
            InputAction::OpenDivergeView
        }
        PanelCmd::WaveBack if !presentation::is_open(state, Surface::WaveOverlay) => {
            InputAction::DismissWave
        }
        PanelCmd::WaveBack | PanelCmd::WaveClose => back_action(state, false),
        PanelCmd::WaveCloseDiverge | PanelCmd::WaveDivergeClose => {
            state.needs_redraw = true;
            InputAction::CloseDivergeView
        }
        PanelCmd::WaveNext => {
            if let Some(handle) = &state.wave_run {
                let max = handle.snapshot().spoke_count.saturating_sub(1);
                state.wave_overlay_selected =
                    state.wave_overlay_selected.saturating_add(1).min(max);
                state.needs_redraw = true;
            }
            InputAction::Consumed
        }
        PanelCmd::WavePrev => {
            state.wave_overlay_selected = state.wave_overlay_selected.saturating_sub(1);
            state.needs_redraw = true;
            InputAction::Consumed
        }
        PanelCmd::WaveDrill => InputAction::WaveDrillSpoke(state.wave_overlay_selected),
        PanelCmd::WaveRerun => InputAction::WaveRerunSpoke(state.wave_overlay_selected),
    }
}

/// The subview key context is frame payload, not open authority.
pub fn context(state: &TuiState) -> Context {
    if state.wave_chat_keys {
        Context::ChatWave
    } else {
        Context::WaveOverlay
    }
}

/// Registry-owned keys only; a ChatWave subview never leaks inherited Stage commands.
pub fn on_key(state: &mut TuiState, key: Key, host: Host) -> InputAction {
    let Resolved::Hit(row) = resolve(context(state), host, &[key]) else {
        return InputAction::Consumed;
    };
    match row.command {
        CommandId::Panel(cmd) => command(state, cmd),
        CommandId::Chat(ChatCmd::CancelOrQuit) => back_action(state, true),
        CommandId::Chat(ChatCmd::OpenPalette) => app::open_palette(state),
        _ => InputAction::Consumed,
    }
}

/// Legacy return matrix: divergence first; Chat-wave drill Esc dismisses, but
/// Ctrl+C always leaves the retained run behind at the frame boundary.
pub fn back_action(state: &TuiState, ctrl_c: bool) -> InputAction {
    if state.wave_diverge_open {
        InputAction::CloseDivergeView
    } else if !ctrl_c && state.wave_chat_keys && state.wave_drill_body.is_some() {
        InputAction::DismissWave
    } else {
        InputAction::CloseWaveOverlay
    }
}

/// Apply only Wave view effects inside the same reconciliation boundary as keys.
/// Async run/rerun completion updates global data elsewhere; it never opens a view.
pub fn apply(state: &mut TuiState, conversation_id: &str, action: &InputAction) -> bool {
    if !matches!(
        action,
        InputAction::OpenWaveOverlay
            | InputAction::CloseWaveOverlay
            | InputAction::WaveDrillSpoke(_)
            | InputAction::OpenDivergeView
            | InputAction::CloseDivergeView
            | InputAction::DismissWave
    ) {
        return false;
    }
    app::with_card_focus_reconcile(state, |state| {
        apply_inner(state, conversation_id, action);
    });
    true
}

fn apply_inner(state: &mut TuiState, conversation_id: &str, action: &InputAction) {
    // A stale Stage can release its frame but never drill or dismiss another
    // Stage's retained data. Do not disturb a higher owner covering this frame.
    if state.modal_stack.all_frames().any(|frame| {
        frame.surface == Surface::WaveOverlay
            && frame
                .conversation_id
                .as_deref()
                .is_some_and(|id| id != conversation_id)
    }) {
        reducer::close(state, Surface::WaveOverlay);
        return;
    }
    if matches!(
        action,
        InputAction::OpenWaveOverlay | InputAction::OpenDivergeView
    ) {
        reducer::open(state, Surface::WaveOverlay);
        if reducer::initialize_surface(state, Surface::WaveOverlay) {
            reducer::bind_stage(state, Surface::WaveOverlay, conversation_id);
        }
    }
    let owns_frame = state.modal_stack.top().is_some_and(|frame| {
        frame.surface == Surface::WaveOverlay
            && frame.conversation_id.as_deref() == Some(conversation_id)
    });
    if !owns_frame {
        // Compact retained-wave Esc has no frame to close. A different modal
        // owner must not receive a delayed global dismiss.
        if matches!(action, InputAction::DismissWave) && state.modal_stack.is_empty() {
            state.dismiss_wave();
        }
        return;
    }
    match action {
        InputAction::OpenWaveOverlay => {
            // Enter from the Chat-wave drill restores overlay keys, not focus.
            state.wave_chat_keys = false;
            state.wave_overlay_selected = 0;
        }
        InputAction::CloseWaveOverlay => {
            reducer::close(state, Surface::WaveOverlay);
        }
        InputAction::WaveDrillSpoke(slot) => {
            if let Some(handle) = &state.wave_run
                && let Some(body) = handle.drill(*slot)
            {
                state.wave_drill_body = Some((*slot, body.as_render_str().to_owned()));
            }
        }
        InputAction::OpenDivergeView => {
            if state.wave_run.is_some() {
                state.wave_diverge_open = true;
                state.wave_chat_keys = true;
            } else {
                reducer::close(state, Surface::WaveOverlay);
            }
        }
        InputAction::CloseDivergeView => {
            state.wave_diverge_open = false;
            if state.wave_drill_body.is_some() {
                state.wave_chat_keys = true;
            } else {
                // Direct d and list -> d both return to compact, not the list.
                reducer::close(state, Surface::WaveOverlay);
            }
        }
        InputAction::DismissWave => {
            state.dismiss_wave();
            reducer::close(state, Surface::WaveOverlay);
        }
        _ => unreachable!("recognized Wave effect"),
    }
    state.needs_redraw = true;
}
