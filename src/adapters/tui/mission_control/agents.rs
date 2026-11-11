//! The Agents tab's rows (Story 21.7 A6, AC11): the subagent roster the registry last
//! reported, as a list tab.
//!
//! No second renderer and no poll: a row is `agent_panel::row_text` of a cached
//! `AgentRowView`, the wide detail preview is `agent_panel::detail_text`, and the tab follows
//! the registry through the one refresh the event loop already runs on every subagent
//! `CapabilityEvent` ([`sync`]). A row's id is the agent id, so a selection survives a refresh
//! that reorders the roster and a vanished agent leaves the muted placeholder behind (A3).

use std::collections::HashMap;

use super::{McRow, McTab};
use crate::adapters::tui::app::InputAction;
use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::widgets::agent_panel;
use crate::domain::models::AgentId;
use crate::domain::models::subagent_view::AgentRowView;

/// What the tab shows: the rows and the words for an empty roster.
pub struct AgentsList {
    pub rows: Vec<McRow>,
    pub empty: Option<String>,
}

/// The zero state (the old panel's words).
pub const EMPTY: &str = "No agents active. Ask the assistant to delegate or use plan mode.";

/// Build the tab from the roster and the cached tails of their output.
#[must_use]
pub fn list(entries: &[AgentRowView], tails: &HashMap<AgentId, String>) -> AgentsList {
    let rows = entries
        .iter()
        .map(|entry| {
            let tail = tails.get(&entry.agent_id).map_or("", String::as_str);
            let status = agent_panel::status_word(entry);
            McRow::new(
                entry.agent_id.as_str(),
                &agent_panel::row_text(entry, tail),
                &agent_panel::detail_text(entry, entries, tail),
            )
            .with_compact(agent_panel::compact_text(entry), vec![status.to_owned()])
        })
        .collect::<Vec<_>>();
    AgentsList {
        empty: rows.is_empty().then(|| EMPTY.to_owned()),
        rows,
    }
}

/// The agent a row id names.
#[must_use]
pub fn agent_id(id: &str) -> AgentId {
    AgentId::from_validated(id.to_owned())
}

/// `Enter` on an Agents row.
#[must_use]
pub fn open_action(id: &str) -> InputAction {
    InputAction::OpenAgentWindow(agent_id(id))
}

/// The roster entry of the selected row, if it is still there.
#[must_use]
pub fn selected(state: &TuiState) -> Option<&AgentRowView> {
    let id = state.mission_control.selected_id()?;
    state
        .agent_panel_state
        .cached_entries
        .iter()
        .find(|entry| entry.agent_id.as_str() == id)
}

/// The registry changed (or the tab opened): rebuild the tab's rows and refresh an open agent
/// window from the same cache.
pub fn sync(state: &mut TuiState) {
    let panel = &state.agent_panel_state;
    let list = list(&panel.cached_entries, &panel.spool_tail_cache);
    state
        .mission_control
        .set_list(McTab::Agents, list.rows, list.empty, None);
    crate::adapters::tui::agent_window::refresh(state);
    state.needs_redraw = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::subagent_view::OwnershipKind;
    use crate::domain::models::{NodeState, WaitReason};

    fn entry(name: &str, status: NodeState) -> AgentRowView {
        AgentRowView {
            isolated: false,
            agent_id: AgentId::new(),
            parent_id: AgentId::root(),
            subagent_type: name.to_owned(),
            spawned_at: 0,
            depth: 1,
            current_status: status,
            ownership: OwnershipKind::Owned,
            effective_model: String::new(),
            tools_summary: String::new(),
            tokens_in: 0,
            tokens_out: 0,
            turns: 0,
            wait_reason: None,
        }
    }

    #[test]
    fn story21_7_an_empty_roster_says_so_and_a_row_is_keyed_by_the_agent_id() {
        let empty = list(&[], &HashMap::new());
        assert!(empty.rows.is_empty());
        assert_eq!(empty.empty.as_deref(), Some(EMPTY));
        let mut waiting = entry("mcp-task", NodeState::Waiting);
        waiting.wait_reason = Some(WaitReason::AwaitingHumanInput);
        let tails = HashMap::from([(waiting.agent_id.clone(), "asking\n".to_owned())]);
        let built = list(std::slice::from_ref(&waiting), &tails);
        assert!(built.empty.is_none());
        assert_eq!(built.rows[0].id, waiting.agent_id.as_str());
        assert!(built.rows[0].text.contains("awaiting your answer  asking"));
        assert_eq!(built.rows[0].keep, vec!["awaiting your answer".to_owned()]);
        assert!(built.rows[0].detail.contains("Recent output:\n  asking"));
    }

    #[test]
    fn story21_7_sync_follows_the_registry_cache_and_keeps_the_selected_id() {
        let mut state = TuiState::new(80, 24);
        let (a, b) = (
            entry("alpha", NodeState::Running),
            entry("beta", NodeState::Created),
        );
        state.agent_panel_state.cached_entries = vec![a.clone(), b.clone()];
        sync(&mut state);
        state.mission_control.switch_to(McTab::Agents);
        assert_eq!(state.mission_control.rows().len(), 2);
        // Select beta, then refresh with the roster reversed: still beta.
        state
            .mission_control
            .move_selection(crate::adapters::tui::presentation::viewer::ViewerCmd::LineDown);
        state
            .mission_control
            .move_selection(crate::adapters::tui::presentation::viewer::ViewerCmd::LineDown);
        assert_eq!(
            state.mission_control.selected_id(),
            Some(b.agent_id.as_str())
        );
        state.agent_panel_state.cached_entries = vec![b.clone(), a];
        sync(&mut state);
        assert_eq!(
            state.mission_control.selected_id(),
            Some(b.agent_id.as_str())
        );
        assert_eq!(
            selected(&state).map(|e| e.subagent_type.as_str()),
            Some("beta")
        );
    }
}
