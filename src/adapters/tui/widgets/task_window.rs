//! The paint of the task window (Story 21.7 A8): the shared text-window painter
//! ([`artifact_window::paint`]) with the task's rail in front. `p` Pause or Resume, `s` Skip,
//! `r` Retry and `e` Edit are painted only where the task's status allows them (the same rules
//! the Tasks tab's rail uses, `task_panel::{pause_label, can_skip, can_retry}`), and a key whose
//! hint was not painted whole is inert (the state records which hints the paint made live). A
//! task that is gone offers none of them.

use ratatui::prelude::*;

use super::artifact_window::{Chrome, paint};
use super::task_panel;
use crate::adapters::tui::keymap::Context as KeyContext;
use crate::adapters::tui::keymap::render::rail;
use crate::adapters::tui::keymap::table_mission::MissionCmd;
use crate::adapters::tui::state::TuiState;

pub fn render(frame: &mut Frame<'_>, rect: Rect, state: &TuiState, title: &str, show_caret: bool) {
    let Some(window) = state.task_window.as_ref() else {
        return;
    };
    let mut lead = Vec::with_capacity(4);
    if !window.gone {
        let status = window.status();
        if let Some(label) = task_panel::pause_label(status) {
            lead.push((
                format!(
                    "{} {label}",
                    rail(KeyContext::TaskWindow, MissionCmd::Pause)
                ),
                Some("pause"),
            ));
        }
        if task_panel::can_retry(status) {
            lead.push((
                format!("{} Retry", rail(KeyContext::TaskWindow, MissionCmd::Retry)),
                Some("retry"),
            ));
        }
        if task_panel::can_skip(status) {
            lead.push((
                format!("{} Skip", rail(KeyContext::TaskWindow, MissionCmd::Skip)),
                Some("skip"),
            ));
        }
        if task_panel::can_retry(status) {
            lead.push((
                format!("{} Edit", rail(KeyContext::TaskWindow, MissionCmd::Edit)),
                Some("edit"),
            ));
        }
    }
    paint(
        frame,
        rect,
        &state.theme,
        &window.text,
        title,
        show_caret,
        Chrome {
            context: KeyContext::TaskWindow,
            copy: MissionCmd::CopyResult,
            lead,
        },
    );
}
