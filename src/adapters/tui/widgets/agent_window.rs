//! The paint of the agent window (Story 21.7 A7): the shared text-window painter
//! ([`artifact_window::paint`]) with the agent's rail in front: `p` Pause or Resume and `x` Kill
//! are painted only where they act now (a live agent in a state that allows them), and a key
//! whose hint was not painted whole is inert (the state records which hints the paint made
//! live).

use ratatui::prelude::*;

use super::artifact_window::{Chrome, paint};
use crate::adapters::tui::agent_window::{can_kill, pause_label};
use crate::adapters::tui::keymap::Context as KeyContext;
use crate::adapters::tui::keymap::render::rail;
use crate::adapters::tui::keymap::table_mission::MissionCmd;
use crate::adapters::tui::state::TuiState;

pub fn render(frame: &mut Frame<'_>, rect: Rect, state: &TuiState, title: &str, show_caret: bool) {
    let Some(window) = state.agent_window.as_ref() else {
        return;
    };
    let mut lead = Vec::with_capacity(2);
    if let Some(label) = pause_label(state) {
        lead.push((
            format!(
                "{} {label}",
                rail(KeyContext::AgentWindow, MissionCmd::AgentPause)
            ),
            Some("pause"),
        ));
    }
    if can_kill(state) {
        lead.push((
            format!(
                "{} Kill",
                rail(KeyContext::AgentWindow, MissionCmd::AgentKill)
            ),
            Some("kill"),
        ));
    }
    paint(
        frame,
        rect,
        &state.theme,
        &window.text,
        title,
        show_caret,
        Chrome {
            context: KeyContext::AgentWindow,
            copy: MissionCmd::CopyTranscript,
            lead,
        },
    );
}
