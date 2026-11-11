//! The hub's agent and Adapters sources and effects (Story 21.7 7-C): the subagent roster the
//! Agents tab and the agent window follow, the one pause/resume op, the kill confirmation and
//! the Adapters snapshot. The event loop only calls these; every rule lives here so a test
//! drives the same function the loop does.
//!
//! The roster is event-driven, never polled: `refresh_roster` runs on the subagent
//! `CapabilityEvent`s the loop already handles (and when a surface opens), and every refresh
//! rebuilds the tab's rows and refreshes an open agent window from the same cache.

use std::sync::Arc;

use crate::adapters::tui::decision_queue;
use crate::adapters::tui::mission_control::agents;
use crate::adapters::tui::notifications::{self, NoticeSource};
use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::widgets::agent_panel;
use crate::domain::models::subagent_view::AgentRowView;
use crate::domain::models::{AgentId, NodeState, Op};
use crate::infrastructure::runtime::agent_core::AgentCore;
use crate::infrastructure::subagent::{NodeTree, SubagentSpool};

/// The warning a mutation on a read-only (subagent output) tab gets.
pub const READ_ONLY: &str = "Tab is read-only (subagent output view)";

/// Bytes of an agent's spool the cache keeps (a transcript is its tail).
const TAIL_READ_BYTES: usize = crate::adapters::tui::agent_window::TAIL_BYTES;

pub fn get_subagent_provider(
    agent_core: &AgentCore,
) -> Option<Arc<crate::adapters::subagent::SubagentProvider>> {
    let tools_arc = agent_core.tools.load_full();
    let any_ref: &dyn std::any::Any = (*tools_arc).as_any();
    let composite = any_ref
        .downcast_ref::<crate::adapters::composite_toolset_adapter::CompositeToolsetAdapter>()?;
    composite.subagent_provider().cloned()
}

/// Replace the roster the surfaces read: kill confirmations whose agent left are moot, a tail
/// of an agent that left is dropped, and the tab and the open window follow.
pub fn apply_roster(state: &mut TuiState, entries: Vec<AgentRowView>) {
    state.agent_panel_state.cached_entries = entries;
    retain_roster_tails(&mut state.agent_panel_state);
    decision_queue::withdraw_gone_agent_kills(state);
    agents::sync(state);
}

/// Read the roster from the registry and apply it.
pub async fn refresh_roster_from(state: &mut TuiState, registry: &NodeTree) {
    let entries = registry
        .list()
        .await
        .into_iter()
        .map(|entry| entry.to_view())
        .collect();
    apply_roster(state, entries);
}

pub async fn refresh_roster(state: &mut TuiState, agent_core: &AgentCore) {
    match get_subagent_provider(agent_core) {
        Some(provider) => refresh_roster_from(state, provider.registry()).await,
        None => apply_roster(state, Vec::new()),
    }
}

/// The tail of an agent's spool as text: the last [`TAIL_READ_BYTES`] bytes, cut at a character
/// boundary (up to three orphaned continuation bytes of a split scalar are dropped, never
/// replaced by U+FFFD), and `true` when what remains is still not valid UTF-8 (the text is then
/// a lossy rendering for display only; its window refuses to copy it as the original).
#[must_use]
pub fn tail_text(bytes: &[u8]) -> (String, bool) {
    let start = bytes.len().saturating_sub(TAIL_READ_BYTES);
    let mut slice = &bytes[start..];
    let orphans = slice
        .iter()
        .take(3)
        .take_while(|byte| **byte & 0xC0 == 0x80)
        .count();
    slice = &slice[orphans..];
    match std::str::from_utf8(slice) {
        Ok(text) => (text.to_owned(), false),
        Err(_) => (String::from_utf8_lossy(slice).into_owned(), true),
    }
}

/// Re-read one agent's spooled tail into the cache (bounded: the last [`TAIL_READ_BYTES`]).
pub async fn refresh_spool_tail_from(state: &mut TuiState, spool: &SubagentSpool, task_id: &str) {
    let (tail, lossy) = tail_text(&spool.tail(task_id).await);
    let agent_id = AgentId::from_validated(task_id.to_owned());
    let panel = &mut state.agent_panel_state;
    if lossy {
        panel.lossy_tails.insert(agent_id.clone());
    } else {
        panel.lossy_tails.remove(&agent_id);
    }
    panel.spool_tail_cache.insert(agent_id, tail);
    retain_roster_tails(panel);
    agents::sync(state);
}

/// Drop the cached tails (and lossy marks) of agents that left the roster.
fn retain_roster_tails(panel: &mut crate::adapters::tui::state::AgentPanelState) {
    let roster = &panel.cached_entries;
    panel
        .spool_tail_cache
        .retain(|id, _| roster.iter().any(|entry| &entry.agent_id == id));
    panel
        .lossy_tails
        .retain(|id| roster.iter().any(|entry| &entry.agent_id == id));
}

pub async fn refresh_spool_tail(state: &mut TuiState, agent_core: &AgentCore, task_id: &str) {
    if let Some(provider) = get_subagent_provider(agent_core) {
        refresh_spool_tail_from(state, provider.spool(), task_id).await;
    }
}

/// The Agents tab opened (a chord or `Left`/`Right`): the registry's roster and every listed
/// agent's tail, once.
pub async fn open_agents_tab_from(
    state: &mut TuiState,
    registry: &NodeTree,
    spool: &SubagentSpool,
) {
    refresh_roster_from(state, registry).await;
    let ids: Vec<String> = state
        .agent_panel_state
        .cached_entries
        .iter()
        .map(|entry| entry.agent_id.as_str().to_owned())
        .collect();
    for id in ids {
        refresh_spool_tail_from(state, spool, &id).await;
    }
}

pub async fn open_agents_tab(state: &mut TuiState, agent_core: &AgentCore) {
    match get_subagent_provider(agent_core) {
        Some(provider) => {
            open_agents_tab_from(state, provider.registry(), provider.spool()).await;
        }
        None => apply_roster(state, Vec::new()),
    }
}

/// `Enter` on an Agents row: the agent's window, opened on a fresh roster and tail.
pub async fn open_agent_window_from(
    state: &mut TuiState,
    registry: &NodeTree,
    spool: &SubagentSpool,
    agent_id: AgentId,
) {
    refresh_roster_from(state, registry).await;
    refresh_spool_tail_from(state, spool, agent_id.as_str()).await;
    crate::adapters::tui::agent_window::open(state, agent_id);
}

pub async fn open_agent_window(state: &mut TuiState, agent_core: &AgentCore, agent_id: AgentId) {
    match get_subagent_provider(agent_core) {
        Some(provider) => {
            open_agent_window_from(state, provider.registry(), provider.spool(), agent_id).await;
        }
        None => {
            apply_roster(state, Vec::new());
            crate::adapters::tui::agent_window::open(state, agent_id);
        }
    }
}

fn refuse_read_only(state: &mut TuiState) {
    notifications::warning(state, NoticeSource::Tabs, READ_ONLY);
    state.needs_redraw = true;
}

/// `p`: pause a running agent, resume a suspended one: exactly one `Op` through the registry,
/// decided on the roster as it is NOW (not as it was painted). A read-only tab refuses with
/// the existing warning and sends nothing; any other state is told, not sent.
pub async fn pause_resume(
    state: &mut TuiState,
    registry: &NodeTree,
    read_only: bool,
    agent_id: &AgentId,
) {
    if read_only {
        refuse_read_only(state);
        return;
    }
    refresh_roster_from(state, registry).await;
    let status = state
        .agent_panel_state
        .cached_entries
        .iter()
        .find(|entry| &entry.agent_id == agent_id)
        .map(|entry| entry.current_status);
    let op = match status {
        None => return,
        Some(NodeState::Running) => Op::Pause,
        Some(NodeState::Suspended) => Op::Resume,
        Some(other) => {
            notifications::info(
                state,
                NoticeSource::Task,
                format!("Cannot pause/resume agent in {other:?} state"),
            );
            state.needs_redraw = true;
            return;
        }
    };
    let _ = registry.send_op(agent_id, op).await;
    refresh_roster_from(state, registry).await;
    state.needs_redraw = true;
}

pub async fn pause_resume_agent(
    state: &mut TuiState,
    agent_core: &AgentCore,
    read_only: bool,
    agent_id: &AgentId,
) {
    match get_subagent_provider(agent_core) {
        Some(provider) => pause_resume(state, provider.registry(), read_only, agent_id).await,
        None if read_only => refuse_read_only(state),
        None => {}
    }
}

/// `x`: raise the kill confirmation of `agent_id` (a decision frame: nothing is sent until the
/// operator confirms). A read-only tab refuses with the existing warning and raises nothing.
pub fn kill(state: &mut TuiState, conversation_id: &str, read_only: bool, agent_id: AgentId) {
    if read_only {
        refuse_read_only(state);
        return;
    }
    let Some(row) = state
        .agent_panel_state
        .cached_entries
        .iter()
        .find(|entry| entry.agent_id == agent_id)
    else {
        return;
    };
    // Nothing to kill once the agent finished.
    if !agent_panel::can_kill(row) {
        return;
    }
    let label = row.subagent_type.clone();
    decision_queue::raise_agent_kill(state, conversation_id, agent_id, label);
    state.needs_redraw = true;
}

/// `Ctrl+X a` / the palette: the Adapters window on a fresh health snapshot.
pub fn open_adapters(state: &mut TuiState, agent_core: &AgentCore) {
    let rows = crate::adapters::tui::widgets::adapter_status_panel::snapshot(
        agent_core,
        &state.session_overrides,
    );
    crate::adapters::tui::adapters_window::open(state, rows);
}
