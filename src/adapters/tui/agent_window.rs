//! The agent window (Story 21.7 A7, AC12): one agent's facts and its spooled transcript in a
//! frame of its own, opened by `Enter` on an Agents row. One window at a time (global like
//! S-THREAD and the artifact window): opening another agent retargets it.
//!
//! It is the artifact window's text window with an agent as its subject: the same body
//! ([`ArtifactWindowState`]: facts, then the transcript through `markdown_body::layout`,
//! Rendered by default, `v` Source, `/` line filter, RU-25 scrolling), the same key executor
//! and painter. What is the agent's own: `p` pauses a running agent or resumes a suspended one,
//! `x` asks to kill it (a decision, never a direct op), and `c` copies the transcript's
//! **original** text, refused above the 8 MiB bound before any string is built.
//!
//! The window follows its agent: every registry refresh ([`refresh`]) re-reads the facts and
//! the cached tail; when the agent is no longer in the roster the window keeps its last facts
//! under `This agent is no longer running.` and `[Esc] Back` (the stale copy of RU-9). There are
//! no tool-call rows and no `[Enter] Inspect`: a subagent's tool calls are recorded nowhere the
//! TUI can read (PC-2, DF-21-7-AGENT-TOOL-CALLS-UNPROJECTED), and no `m` Change model / `t`
//! Update tools: no per-agent model or tool capability exists (RU-21-7-11).

use crate::adapters::tui::app::InputAction;
use crate::adapters::tui::artifact_window::{
    self as text, ArtifactBody, ArtifactWindowState, Slot,
};
use crate::adapters::tui::keymap::table_mission::MissionCmd;
use crate::adapters::tui::keymap::{CommandId, Context, Host, Key};
use crate::adapters::tui::presentation::{Surface, reducer};
use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::widgets::agent_panel;
use crate::domain::models::AgentId;
use crate::domain::models::subagent_view::AgentRowView;

/// What the window says when the agent has written nothing.
const NO_OUTPUT: &str = "no output yet";
/// The line a vanished agent's window opens with.
pub const GONE: &str = "This agent is no longer running.";
/// The spool tail the registry cache keeps per agent (bytes): a transcript this long is a tail.
pub const TAIL_BYTES: usize = 8192;

pub struct AgentWindowState {
    pub agent_id: AgentId,
    /// The facts and the transcript: the artifact window's text window.
    pub text: ArtifactWindowState,
    /// The agent as the roster last showed it (what the stale copy keeps saying).
    row: Option<AgentRowView>,
    /// The agent has left the roster.
    pub gone: bool,
    /// The transcript the window last showed (kept when the agent leaves the roster).
    tail: String,
}

impl std::fmt::Debug for AgentWindowState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentWindowState")
            .field("agent_id", &self.agent_id)
            .field("gone", &self.gone)
            .finish_non_exhaustive()
    }
}

/// The header (facts) and the body of the window for the roster as it is now.
fn content(
    agent_id: &AgentId,
    row: Option<&AgentRowView>,
    gone: bool,
    entries: &[AgentRowView],
    tail: &str,
) -> (String, ArtifactBody) {
    let mut header = String::new();
    if gone {
        header.push_str(GONE);
        header.push('\n');
    }
    match row {
        Some(row) => header.push_str(&agent_panel::facts(row, entries)),
        None => {
            header.push_str("Agent: ");
            header.push_str(agent_id.as_str());
        }
    }
    if tail.len() >= TAIL_BYTES {
        header.push_str("\nTranscript: the last 8 KiB of the output (earlier output is not kept)");
    }
    (header, ArtifactBody::Source(tail.as_bytes().to_vec()))
}

impl AgentWindowState {
    /// A window on `agent_id`: its facts from the roster, its transcript from the cached tail.
    #[must_use]
    pub fn new(agent_id: AgentId, entries: &[AgentRowView], tail: &str) -> Self {
        let row = entries
            .iter()
            .find(|entry| entry.agent_id == agent_id)
            .cloned();
        let gone = row.is_none();
        let (header, body) = content(&agent_id, row.as_ref(), gone, entries, tail);
        Self {
            text: ArtifactWindowState::new(agent_id.as_str().to_owned(), header, body)
                .with_empty_text(NO_OUTPUT),
            agent_id,
            row,
            gone,
            tail: tail.to_owned(),
        }
    }

    /// Follow the roster: new facts, the cached tail, and the stale copy once the agent left.
    /// `tail` is the cache's entry for the agent: `None` for a live agent with no output yet
    /// clears the transcript, but for a vanished agent (its cache entry was dropped with it)
    /// the window keeps the last transcript it showed, so the stale copy still reads and copies.
    pub fn follow(&mut self, entries: &[AgentRowView], tail: Option<&str>) {
        match entries.iter().find(|entry| entry.agent_id == self.agent_id) {
            Some(row) => {
                self.row = Some(row.clone());
                self.gone = false;
            }
            None => self.gone = true,
        }
        match tail {
            Some(tail) => tail.clone_into(&mut self.tail),
            None if !self.gone => self.tail.clear(),
            None => {}
        }
        let (header, body) = content(
            &self.agent_id,
            self.row.as_ref(),
            self.gone,
            entries,
            &self.tail,
        );
        self.text.replace(header, body);
    }

    /// The agent as the roster last showed it.
    #[must_use]
    pub fn row(&self) -> Option<&AgentRowView> {
        self.row.as_ref()
    }

    /// The frame title, `Agent · <type>` (the type is untrusted: sanitized by the painter's
    /// title path before it reaches a cell).
    #[must_use]
    pub fn title(&self) -> String {
        title_of(self.agent_id.as_str(), self.row.as_ref())
    }
}

fn title_of(id: &str, row: Option<&AgentRowView>) -> String {
    let label = row.map_or_else(|| id.to_owned(), |row| row.subagent_type.clone());
    format!(
        "Agent · {}",
        crate::domain::services::peer_text::sanitize_display_line(&label)
    )
}

fn get(state: &TuiState) -> Option<&ArtifactWindowState> {
    state.agent_window.as_ref().map(|window| &window.text)
}

fn get_mut(state: &mut TuiState) -> Option<&mut ArtifactWindowState> {
    state.agent_window.as_mut().map(|window| &mut window.text)
}

pub(crate) const SLOT: Slot = Slot {
    surface: Surface::AgentWindow,
    context: Context::AgentWindow,
    copy: MissionCmd::CopyTranscript,
    noun: "agent transcript",
    get,
    get_mut,
};

/// Open (or retarget) the agent window on `agent_id` from the cached roster and tail.
pub fn open(state: &mut TuiState, agent_id: AgentId) {
    let tail = state
        .agent_panel_state
        .spool_tail_cache
        .get(&agent_id)
        .map_or("", String::as_str);
    let mut window = AgentWindowState::new(
        agent_id.clone(),
        &state.agent_panel_state.cached_entries,
        tail,
    );
    window
        .text
        .set_lossy(state.agent_panel_state.lossy_tails.contains(&agent_id));
    let title = window.title();
    reducer::open_agent_window(state, window, title);
}

/// The registry cache changed: the open window follows its agent (a retitled agent retitles
/// the frame). A vanished agent's cache entry is gone, so the window keeps the transcript it
/// last showed ([`AgentWindowState::follow`]).
pub fn refresh(state: &mut TuiState) {
    let Some(window) = state.agent_window.as_mut() else {
        return;
    };
    let panel = &state.agent_panel_state;
    let tail = panel
        .spool_tail_cache
        .get(&window.agent_id)
        .map(String::as_str);
    window.follow(&panel.cached_entries, tail);
    if tail.is_some() {
        window
            .text
            .set_lossy(panel.lossy_tails.contains(&window.agent_id));
    }
    let title = window.title();
    if let Some(frame) = state
        .modal_stack
        .all_frames_mut()
        .find(|frame| frame.surface == Surface::AgentWindow)
    {
        frame.tool_title = Some(title);
    }
    state.needs_redraw = true;
}

/// The agent window is the top frame.
#[must_use]
pub fn is_top(state: &TuiState) -> bool {
    text::is_top_in(state, &SLOT)
}

/// The filter field of the window is open (a text context of the frame).
#[must_use]
pub fn search_editing(state: &TuiState) -> bool {
    text::search_editing_in(state, &SLOT)
}

/// What `p` would do now: the pause or resume label of a live agent, `None` where the rail
/// paints no hint and the key is inert.
#[must_use]
pub fn pause_label(state: &TuiState) -> Option<&'static str> {
    let row = state.agent_window.as_ref().filter(|w| !w.gone)?.row()?;
    agent_panel::pause_label(row.current_status)
}

/// Whether `x` is offered now.
#[must_use]
pub fn can_kill(state: &TuiState) -> bool {
    state
        .agent_window
        .as_ref()
        .filter(|window| !window.gone)
        .and_then(AgentWindowState::row)
        .is_some_and(agent_panel::can_kill)
}

pub(crate) fn on_key(state: &mut TuiState, key: Key, host: Host) -> InputAction {
    text::on_key_in(
        state,
        key,
        host,
        &SLOT,
        &mut |state, command| match command {
            CommandId::Mission(MissionCmd::AgentPause) => {
                let live = state
                    .agent_window
                    .as_ref()
                    .is_some_and(|window| window.text.live().pause);
                let id = state
                    .agent_window
                    .as_ref()
                    .map(|window| window.agent_id.clone());
                Some(match id {
                    Some(id) if live => InputAction::AgentPauseResume(id),
                    _ => InputAction::Consumed,
                })
            }
            CommandId::Mission(MissionCmd::AgentKill) => {
                let live = state
                    .agent_window
                    .as_ref()
                    .is_some_and(|window| window.text.live().kill);
                let id = state
                    .agent_window
                    .as_ref()
                    .map(|window| window.agent_id.clone());
                Some(match id {
                    Some(id) if live => InputAction::AgentKill(id),
                    _ => InputAction::Consumed,
                })
            }
            _ => None,
        },
    )
}

pub(crate) fn on_wheel(
    state: &mut TuiState,
    delta: crate::domain::models::view_state::ScrollDelta,
) {
    text::on_wheel_in(state, &SLOT, delta);
}

pub(crate) fn search_changed(state: &mut TuiState) {
    text::search_changed_in(state, &SLOT);
}

pub(crate) fn accept_search(state: &mut TuiState) {
    text::accept_search_in(state, &SLOT);
}

pub(crate) fn clear_search(state: &mut TuiState) {
    text::clear_search_in(state, &SLOT);
}

/// The raw press-only rule of the frame (RU-29).
pub(crate) fn ignores_raw_key(state: &TuiState, key: &crossterm::event::KeyEvent) -> bool {
    text::ignores_raw_key_in(state, &SLOT, key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::NodeState;
    use crate::domain::models::subagent_view::OwnershipKind;

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
            effective_model: "m".to_owned(),
            tools_summary: "t".to_owned(),
            tokens_in: 1,
            tokens_out: 2,
            turns: 3,
            wait_reason: None,
        }
    }

    #[test]
    fn story21_7_the_window_follows_its_agent_and_keeps_a_stale_copy_when_it_leaves() {
        let agent = entry("reviewer", NodeState::Running);
        let mut window = AgentWindowState::new(
            agent.agent_id.clone(),
            std::slice::from_ref(&agent),
            "hello",
        );
        assert!(!window.gone);
        assert_eq!(window.title(), "Agent · reviewer");
        let generation = window.text.generation;
        // Nothing changed: no re-layout.
        window.follow(std::slice::from_ref(&agent), Some("hello"));
        assert_eq!(window.text.generation, generation);
        // New output: the body changes.
        window.follow(std::slice::from_ref(&agent), Some("hello\nworld"));
        assert_ne!(window.text.generation, generation);
        // The agent leaves: the stale copy keeps the last facts under the gone line.
        window.follow(&[], None);
        assert!(window.gone);
        assert_eq!(window.title(), "Agent · reviewer");
        assert_eq!(window.row().map(|row| row.turns), Some(3));
    }

    #[test]
    fn story21_7_a_window_opened_for_an_agent_not_in_the_roster_is_a_stale_copy() {
        let id = AgentId::new();
        let window = AgentWindowState::new(id.clone(), &[], "");
        assert!(window.gone);
        assert!(window.title().starts_with("Agent · "));
    }

    #[test]
    fn story21_7_a_full_tail_says_it_is_a_tail() {
        let agent = entry("reviewer", NodeState::Running);
        let (header, _) = content(
            &agent.agent_id,
            Some(&agent),
            false,
            std::slice::from_ref(&agent),
            &"x".repeat(TAIL_BYTES),
        );
        assert!(header.contains("Transcript: the last 8 KiB"), "{header}");
        let (header, _) = content(&agent.agent_id, Some(&agent), false, &[], "short");
        assert!(!header.contains("Transcript:"));
    }

    #[test]
    fn story21_7_7c01_a_vanished_agent_keeps_the_transcript_it_last_showed() {
        let agent = entry("reviewer", NodeState::Running);
        let mut window = AgentWindowState::new(
            agent.agent_id.clone(),
            std::slice::from_ref(&agent),
            "one\ntwo",
        );
        // The production order: the agent left, its cache entry went with it (`None`).
        window.follow(&[], None);
        assert!(window.gone);
        assert_eq!(window.tail, "one\ntwo");
        assert!(
            matches!(&window.text.body, ArtifactBody::Source(bytes) if bytes == b"one\ntwo"),
            "the transcript is still the body"
        );
        // A live agent whose cache entry is missing has simply no output yet.
        let mut live =
            AgentWindowState::new(agent.agent_id.clone(), std::slice::from_ref(&agent), "old");
        live.follow(std::slice::from_ref(&agent), None);
        assert!(matches!(&live.text.body, ArtifactBody::Source(bytes) if bytes.is_empty()));
    }
}
