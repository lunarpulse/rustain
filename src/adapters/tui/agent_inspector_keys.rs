//! Key dispatch of the Agents-panel inspector (Story 21.3 AC21): the decisions of the
//! event loop's former hand-written `match`, resolved through the keymap registry
//! (`Context::SidebarAgentInspector`). Pure state changes happen here; what needs the
//! registry, the event bus or the tab manager comes back as an [`InspectorOutcome`].

use crate::adapters::tui::keymap::table_inspector::InspectorCmd;
use crate::adapters::tui::keymap::{self, CommandId, Context, Host, Key, Resolved};
use crate::adapters::tui::notifications::{self, NoticeSource};
use crate::adapters::tui::presentation::reducer;
use crate::adapters::tui::state::TuiState;
use crate::domain::events::DomainInputEvent;
use crate::domain::models::conversation::{ChatMessage, generate_message_id};
use crate::domain::models::session_meta::now_unix;
use crate::domain::models::visual::PanelType;
use crate::domain::models::{
    AgentId, Authorship, ChannelKind, Conversation, FocusState, MessageRole, NodeState, Op,
    SendAuthorization, generate_conversation_id,
};

/// What the event loop does after the inspector saw a key.
#[derive(Debug)]
pub enum InspectorOutcome {
    /// Not the inspector's key (or its guard failed): `handle_operator_input` goes on.
    PassThrough,
    /// Handled here: the loop reads the next event.
    Consumed,
    /// `p` on a running or suspended agent: refresh the cache, then send `op`.
    SendOp { agent_id: AgentId, op: Op },
    /// `y` with the kill confirmation shown: cascade-kill the agent.
    ConfirmKill(AgentId),
    /// `m`: post the "use the model selector" notice.
    ModelNotice,
    /// `t`: post the "not available yet" notice.
    ToolsNotice,
    /// `c`: open this read-only conversation in a new tab.
    OpenConversation(Box<Conversation>),
}

/// Close the inspector, back to the Agents list.
pub(crate) fn close(state: &mut TuiState) {
    let panel = &mut state.agent_panel_state;
    panel.drill_down_agent = None;
    panel.inspector_scroll_offset = 0;
    panel.pending_kill_confirm = None;
    state.needs_redraw = true;
}

/// Scroll the inspector one line.
pub(crate) fn scroll(state: &mut TuiState, down: bool) {
    let offset = &mut state.agent_panel_state.inspector_scroll_offset;
    *offset = if down {
        offset.saturating_add(1)
    } else {
        offset.saturating_sub(1)
    };
    state.needs_redraw = true;
}

fn command(event: &DomainInputEvent) -> Option<InspectorCmd> {
    let keys = [Key::from_domain(event)?];
    match keymap::resolve(Context::SidebarAgentInspector, Host::Local, &keys) {
        Resolved::Hit(row) => match row.command {
            CommandId::Inspector(cmd) => Some(cmd),
            _ => None,
        },
        _ => None,
    }
}

/// The read-only conversation of the agent's spooled output (`c`).
fn read_only_conversation(state: &TuiState, agent_id: &AgentId) -> Conversation {
    let panel = &state.agent_panel_state;
    let title = match panel
        .cached_entries
        .iter()
        .find(|e| e.agent_id == *agent_id)
    {
        Some(entry) => format!("\u{1F441} {} (read-only)", entry.subagent_type),
        None => "\u{1F441} Agent (read-only)".to_string(),
    };
    let body = panel
        .spool_tail_cache
        .get(agent_id)
        .filter(|tail| !tail.is_empty())
        .cloned()
        .unwrap_or_else(|| "(subagent has no output yet)".to_string());
    let now = now_unix();
    Conversation {
        id: generate_conversation_id(),
        title,
        messages: vec![ChatMessage {
            id: generate_message_id(),
            role: MessageRole::Assistant,
            content: body,
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: now,
            token_count: None,
            stop_reason: None,
            synthetic: true,
            images: vec![],
            origin: ChannelKind::Terminal,
            authorship: Authorship::HumanWritten,
            send_authorization: SendAuthorization::Unknown,
            retracted_at_ms: None,
            history_source: Default::default(),
        }],
        created_at: now,
        updated_at: now,
        session_id: Some(agent_id.as_str().to_string()),
        ..Default::default()
    }
}

/// One user event while the Agents panel is focused with an agent open. `Esc` closes
/// the inspector first, even on a read-only tab; any other event on a read-only tab
/// posts the read-only warning and goes on to `handle_operator_input`.
pub fn on_key(
    state: &mut TuiState,
    event: &DomainInputEvent,
    read_only_tab: bool,
) -> InspectorOutcome {
    if !(state.sidebar_visible
        && matches!(state.focus, FocusState::Sidebar { .. })
        && state.sidebar_panel == Some(PanelType::Agents))
    {
        return InspectorOutcome::PassThrough;
    }
    let Some(agent_id) = state.agent_panel_state.drill_down_agent.clone() else {
        return InspectorOutcome::PassThrough;
    };
    let cmd = command(event);
    if read_only_tab && cmd != Some(InspectorCmd::Close) {
        notifications::warning(
            state,
            NoticeSource::Tabs,
            "Tab is read-only (subagent output view)",
        );
        state.needs_redraw = true;
        return InspectorOutcome::PassThrough;
    }
    // With the kill confirmation shown, `Esc` and `Ctrl+C` never answer it (A8): one cue
    // names the keys that do and the confirmation stays open.
    let kill_pending = state.agent_panel_state.pending_kill_confirm.as_ref() == Some(&agent_id);
    if kill_pending && matches!(Key::from_domain(event), Some(Key::Esc | Key::Ctrl('c'))) {
        let answers = [InspectorCmd::ConfirmKill, InspectorCmd::CancelKill]
            .map(|cmd| (Context::SidebarAgentInspector, CommandId::Inspector(cmd)));
        notifications::info(state, NoticeSource::Approval, keymap::render::cue(&answers));
        state.needs_redraw = true;
        return InspectorOutcome::Consumed;
    }
    let Some(cmd) = cmd else {
        return InspectorOutcome::PassThrough;
    };
    match cmd {
        InspectorCmd::Close => close(state),
        InspectorCmd::ScrollDown => scroll(state, true),
        InspectorCmd::ScrollUp => scroll(state, false),
        InspectorCmd::Kill => {
            state.agent_panel_state.pending_kill_confirm = Some(agent_id);
            state.needs_redraw = true;
        }
        InspectorCmd::ConfirmKill if kill_pending => {
            return InspectorOutcome::ConfirmKill(agent_id);
        }
        InspectorCmd::CancelKill if kill_pending => {
            state.agent_panel_state.pending_kill_confirm = None;
            state.needs_redraw = true;
        }
        InspectorCmd::ConfirmKill | InspectorCmd::CancelKill => {
            return InspectorOutcome::PassThrough;
        }
        InspectorCmd::Model => {
            state.needs_redraw = true;
            return InspectorOutcome::ModelNotice;
        }
        InspectorCmd::Tools => {
            state.needs_redraw = true;
            return InspectorOutcome::ToolsNotice;
        }
        InspectorCmd::Conversation => {
            let conversation = read_only_conversation(state, &agent_id);
            reducer::set_base(state, FocusState::Chat);
            state.needs_redraw = true;
            return InspectorOutcome::OpenConversation(Box::new(conversation));
        }
        InspectorCmd::Pause => {
            let status = state
                .agent_panel_state
                .cached_entries
                .iter()
                .find(|e| e.agent_id == agent_id)
                .map(|e| e.current_status);
            match status {
                None => {}
                Some(NodeState::Running) => {
                    return InspectorOutcome::SendOp {
                        agent_id,
                        op: Op::Pause,
                    };
                }
                Some(NodeState::Suspended) => {
                    return InspectorOutcome::SendOp {
                        agent_id,
                        op: Op::Resume,
                    };
                }
                Some(other) => {
                    notifications::info(
                        state,
                        NoticeSource::Task,
                        format!("Cannot pause/resume agent in {other:?} state"),
                    );
                    state.needs_redraw = true;
                }
            }
        }
    }
    InspectorOutcome::Consumed
}
