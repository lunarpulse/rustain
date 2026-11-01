//! Story 21.3 conformance leg K9 (agent inspector): every row of `Context::SidebarAgentInspector`
//! driven BY VALUE through `agent_inspector_keys::on_key` (the adapter the event loop calls
//! for the open inspector) from a real decoded key, with the outcome and the salient state.

use rustain::adapters::tui::agent_inspector_keys::{InspectorOutcome, on_key};
use rustain::adapters::tui::keymap::{
    CommandId, Context, Host, Key, Resolved, resolve, resolve::rows_of,
};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::models::visual::PanelType;
use rustain::domain::models::{AgentId, AgentRowView, FocusState, NodeState, OwnershipKind};

use super::support::{decode, fresh, press as local_press};

const READ_ONLY: &str = "tabs/Warning/Tab is read-only (subagent output view)x1";

fn agent() -> AgentId {
    AgentId::parse("spoke-1").expect("a valid agent id")
}

/// The Agents panel focused with `spoke-1` open (scroll 3, status `status`).
#[rustfmt::skip]
fn open(status: NodeState) -> TuiState {
    let mut state = fresh();
    state.sidebar_visible = true;
    state.sidebar_panel = Some(PanelType::Agents);
    state.focus = FocusState::Sidebar { panel: PanelType::Agents, selected: 0 };
    state.agent_panel_state.cached_entries = vec![AgentRowView {
        isolated: false,
        agent_id: agent(),
        parent_id: AgentId::root(),
        subagent_type: "coder".to_string(),
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
    }];
    state.agent_panel_state.drill_down_agent = Some(agent());
    state.agent_panel_state.inspector_scroll_offset = 3;
    state
}

#[rustfmt::skip]
fn salient(state: &TuiState) -> String {
    let panel = &state.agent_panel_state;
    let notices: Vec<String> = state.notifications.records()
        .map(|r| format!("{}/{:?}/{}x{}", r.source.label(), r.severity, r.message, r.count))
        .collect();
    format!(
        "{} open={} scroll={} kill={} notices={notices:?}",
        if matches!(state.focus, FocusState::Chat) { "chat" } else { "sidebar" },
        panel.drill_down_agent.is_some(),
        panel.inspector_scroll_offset,
        panel.pending_kill_confirm.is_some(),
    )
}

/// The salient text with `open` agent, scrolled `scroll`, kill confirmation `kill`, no notices.
fn at(open: bool, scroll: u16, kill: bool) -> String {
    format!("sidebar open={open} scroll={scroll} kill={kill} notices=[]")
}

#[rustfmt::skip]
fn name(outcome: &InspectorOutcome) -> String {
    match outcome {
        InspectorOutcome::SendOp { agent_id, op } => format!("SendOp({}, {op:?})", agent_id.as_str()),
        InspectorOutcome::ConfirmKill(id) => format!("ConfirmKill({})", id.as_str()),
        InspectorOutcome::OpenConversation(conv) => {
            format!("OpenConversation({}, {}, {:?})", conv.title, conv.messages[0].content, conv.session_id)
        }
        other => format!("{other:?}"),
    }
}

/// Press `key` through the adapter: `"{outcome} | {salient}"`.
fn press(state: &mut TuiState, key: Key, read_only: bool) -> String {
    let event = decode(key).expect("the key decodes");
    let outcome = on_key(state, &event, read_only);
    format!("{} | {}", name(&outcome), salient(state))
}

fn pressed(mut state: TuiState, key: Key) -> String {
    press(&mut state, key, false)
}

/// Regression caught: an inspector key (`p x m t c j k Esc`, or `y Enter n` without a pending kill) acts on the wrong state.
#[test]
#[rustfmt::skip]
fn k9_agent_inspector_every_row_acts_by_value() {
    let same = |outcome: &str| format!("{outcome} | {}", at(true, 3, false));
    let opened = "OpenConversation(\u{1F441} coder (read-only), (subagent has no output yet), Some(\"spoke-1\")) | chat open=true scroll=3 kill=false notices=[]";
    let witnesses = [
        (Key::Char('p'), same("SendOp(spoke-1, Pause)")),
        (Key::Char('x'), format!("Consumed | {}", at(true, 3, true))),
        (Key::Char('y'), same("PassThrough")),
        (Key::Char('n'), same("PassThrough")),
        (Key::Char('m'), same("ModelNotice")),
        (Key::Char('t'), same("ToolsNotice")),
        (Key::Char('c'), opened.to_string()),
        (Key::Char('j'), format!("Consumed | {}", at(true, 4, false))),
        (Key::Char('k'), format!("Consumed | {}", at(true, 2, false))),
        (Key::Esc, format!("Consumed | {}", at(false, 0, false))),
    ];
    let rows: Vec<_> = rows_of(Context::SidebarAgentInspector, Host::Local).collect();
    assert_eq!(rows.len(), witnesses.len(), "every inspector row has exactly one witness");
    for row in rows {
        assert!(witnesses.iter().any(|(key, _)| [*key] == *row.keys), "{:?} has a witness", row.keys);
    }
    for (key, expect) in witnesses {
        assert_eq!(pressed(open(NodeState::Running), key), expect, "{key:?}");
    }
}

/// Regression caught: `y` / `Enter` / `n` ignore the pending kill, or a kill is confirmed for another agent.
#[test]
#[rustfmt::skip]
fn k9_agent_inspector_kill_confirmation_answers_only_while_pending() {
    let pending = |id: &str| {
        let mut state = open(NodeState::Running);
        state.agent_panel_state.pending_kill_confirm = AgentId::parse(id).ok();
        state
    };
    let confirm = format!("ConfirmKill(spoke-1) | {}", at(true, 3, true));
    assert_eq!(pressed(pending("spoke-1"), Key::Char('y')), confirm);
    // A real `Enter` never confirms a cascade kill: it is no inspector row and passes through.
    assert_eq!(pressed(pending("spoke-1"), Key::Enter), format!("PassThrough | {}", at(true, 3, true)));
    assert_eq!(pressed(pending("spoke-1"), Key::Char('n')), format!("Consumed | {}", at(true, 3, false)));
    assert_eq!(pressed(pending("spoke-1"), Key::Esc), format!("Consumed | {}", at(false, 0, false)));
    for key in [Key::Char('y'), Key::Enter, Key::Char('n')] {
        assert_eq!(pressed(pending("spoke-2"), key), format!("PassThrough | {}", at(true, 3, true)), "{key:?}");
    }
}

/// Regression caught: `p` pauses a finished agent, resumes a running one, or stays silent about why it refused.
#[test]
#[rustfmt::skip]
fn k9_agent_inspector_pause_follows_the_agent_status() {
    assert_eq!(pressed(open(NodeState::Running), Key::Char('p')), format!("SendOp(spoke-1, Pause) | {}", at(true, 3, false)));
    assert_eq!(pressed(open(NodeState::Suspended), Key::Char('p')), format!("SendOp(spoke-1, Resume) | {}", at(true, 3, false)));
    for status in [NodeState::Created, NodeState::Completed, NodeState::Failed] {
        let refusal = format!("Consumed | sidebar open=true scroll=3 kill=false notices=[\"task/Info/Cannot pause/resume agent in {status:?} statex1\"]");
        assert_eq!(pressed(open(status), Key::Char('p')), refusal);
    }
    let mut unknown = open(NodeState::Running);
    unknown.agent_panel_state.cached_entries.clear();
    assert_eq!(pressed(unknown, Key::Char('p')), format!("Consumed | {}", at(true, 3, false)));
}

/// Regression caught: a read-only tab runs an owner control, warns twice, cannot close the inspector, or `j`/`k` scroll the inspector instead of moving the Agents list.
#[test]
#[rustfmt::skip]
fn k9_agent_inspector_read_only_tab_warns_once_and_still_closes() {
    let keys: Vec<Key> = rows_of(Context::SidebarAgentInspector, Host::Local)
        .map(|row| row.keys[0])
        .filter(|key| *key != Key::Esc)
        .chain([Key::Char('z'), Key::Enter])
        .collect();
    assert!(keys.len() >= 10, "every inspector row key is driven: {keys:?}");
    for key in keys {
        let mut state = open(NodeState::Running);
        let expect = format!("PassThrough | sidebar open=true scroll=3 kill=false notices=[\"{READ_ONLY}\"]");
        assert_eq!(press(&mut state, key, true), expect, "{key:?}");
    }
    let mut state = open(NodeState::Running);
    assert_eq!(press(&mut state, Key::Esc, true), format!("Consumed | {}", at(false, 0, false)));
}

/// Regression caught: after the read-only pass-through, `j` / `k` scroll the inspector instead of moving the Agents list selection.
#[test]
fn k9_agent_inspector_keys_that_pass_through_move_the_agents_list() {
    let mut state = open(NodeState::Running);
    state.sidebar_entry_count = 3;
    state.sidebar_selected = 1;
    for (key, selected) in [(Key::Char('j'), 2), (Key::Char('k'), 1)] {
        local_press(&mut state, key);
        assert_eq!(state.sidebar_selected, selected, "{key:?}");
        assert_eq!(
            state.agent_panel_state.inspector_scroll_offset, 3,
            "{key:?}"
        );
    }
}

/// Regression caught: a key the inspector has no row for acts, or the inspector claims keys while it is not open.
#[test]
#[rustfmt::skip]
fn k9_agent_inspector_rowless_keys_and_a_closed_inspector_pass_through() {
    let before = salient(&open(NodeState::Running));
    for key in Key::universe() {
        let Some(event) = decode(key) else { continue };
        let Some(decoded) = Key::from_domain(&event) else { continue };
        if let Resolved::Hit(row) = resolve(Context::SidebarAgentInspector, Host::Local, &[decoded]) {
            if matches!(row.command, CommandId::Inspector(_)) {
                continue;
            }
        }
        let mut probe = open(NodeState::Running);
        assert!(matches!(on_key(&mut probe, &event, false), InspectorOutcome::PassThrough), "{key:?}");
        assert_eq!(salient(&probe), before, "{key:?}");
    }
    let closed: [fn(&mut TuiState); 4] = [
        |s| s.agent_panel_state.drill_down_agent = None,
        |s| s.sidebar_visible = false,
        |s| s.focus = FocusState::Chat,
        |s| s.sidebar_panel = Some(PanelType::Tasks),
    ];
    for close in closed {
        let mut probe = open(NodeState::Running);
        close(&mut probe);
        let before = salient(&probe);
        for key in [Key::Char('x'), Key::Char('j'), Key::Char('k'), Key::Char('p'), Key::Char('y'), Key::Esc] {
            let event = decode(key).expect("the key decodes");
            assert!(matches!(on_key(&mut probe, &event, false), InspectorOutcome::PassThrough), "{key:?}");
            assert_eq!(salient(&probe), before, "{key:?}");
        }
    }
}
