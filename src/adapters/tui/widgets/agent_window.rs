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
                rail(KeyContext::AgentWindow, MissionCmd::Pause)
            ),
            Some("pause"),
        ));
    }
    if can_kill(state) {
        lead.push((
            format!("{} Kill", rail(KeyContext::AgentWindow, MissionCmd::Kill)),
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

/// Paint the exact reader with the native rail. Relations occupy only the otherwise
/// empty bottom border and are actionable only when their whole hint fits.
#[cfg(feature = "session-visit")]
pub(crate) fn render_visit(
    frame: &mut Frame<'_>,
    rect: Rect,
    state: &TuiState,
    title: &str,
    show_caret: bool,
) {
    use crate::adapters::tui::keymap::render::rail_arrows;
    use crate::adapters::tui::text_geometry::cells;

    let Some(visit) = state.session_visit.as_ref() else {
        return;
    };
    let context = KeyContext::SessionVisit;
    paint(
        frame,
        rect,
        &state.theme,
        &visit.text,
        title,
        show_caret,
        Chrome {
            context,
            copy: MissionCmd::CopySource,
            lead: Vec::new(),
        },
    );
    visit.relations_painted.set(false);
    if visit.text.search.editing || rect.height < 3 || rect.width < 3 {
        return;
    }
    let hint = match visit.relations() {
        (true, true) => format!(
            " {} Related ",
            rail_arrows(context, MissionCmd::PreviousVisit, MissionCmd::NextVisit)
        ),
        (true, false) => format!(
            " {} Previous source ",
            rail(context, MissionCmd::PreviousVisit)
        ),
        (false, true) => format!(" {} Next source ", rail(context, MissionCmd::NextVisit)),
        (false, false) => return,
    };
    let width = rect.width - 2;
    if cells(&hint) <= usize::from(width) {
        frame.render_widget(
            Line::styled(hint, Style::default().fg(state.theme.colors.fg_secondary)),
            Rect::new(rect.x + 1, rect.bottom() - 1, width, 1),
        );
        visit.relations_painted.set(true);
    }
}
