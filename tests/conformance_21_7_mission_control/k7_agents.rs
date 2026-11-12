//! K7 / AC11-AC13 / A6 / A7 / A10 (21-7 slice 7-C): the Agents tab and the agent window. The
//! roster is the production registry's (`NodeTree::list` through `mission_bridge`, never a
//! hand-written row), the spooled transcript the production `SubagentSpool`'s; keys travel as
//! crossterm events through the local host's decoder; `p` and `x` reach the registry only
//! through the production effect functions the event loop calls (`mission_bridge::{pause_resume,
//! kill}`), observed on the agents' real command channels; every assertion reads painted cells,
//! the one frame on the stack, the decision queue or the clipboard closure. No sleeps.
use super::support::*;
use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::clipboard::ClipboardResult;
use rustain::adapters::tui::decision_queue::DecisionKind;
use rustain::adapters::tui::keymap::Key;
use rustain::adapters::tui::mission_control::McTab;
use rustain::adapters::tui::presentation::Surface;
use rustain::domain::models::palette::{PaletteAction, SurfaceTarget};
use rustain::domain::models::palette::{PaletteEntry, PaletteScope};
use rustain::domain::models::{
    AgentId, AgentMetrics, CapabilityTokenId, NodeState, Op, WaitReason,
};
use rustain::infrastructure::runtime::mission_bridge;
use rustain::infrastructure::subagent::{AgentHandle, MailboxBudget, NodeTree, SubagentSpool};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// A real registry and spool, and the command channel of every agent registered in it.
struct World {
    tree: NodeTree,
    spool: SubagentSpool,
    ops: HashMap<String, mpsc::Receiver<Op>>,
    _dir: tempfile::TempDir,
}

async fn world() -> World {
    let dir = tempfile::tempdir().expect("scratch spool dir");
    World {
        tree: NodeTree::new(),
        spool: SubagentSpool::new(dir.path().join("spool"))
            .await
            .expect("spool"),
        ops: HashMap::new(),
        _dir: dir,
    }
}

impl World {
    /// Register `name` as an owned agent in `status` (as a peer when `peer`).
    async fn spawn(&mut self, name: &str, status: NodeState, peer: bool) -> AgentId {
        let id = AgentId::new();
        let (command_tx, rx) = mpsc::channel(8);
        let (status_tx, _status_rx) = tokio::sync::watch::channel(NodeState::Created);
        let (_metrics_tx, metrics_rx) = tokio::sync::watch::channel(AgentMetrics::default());
        let handle = AgentHandle {
            agent_id: id.clone(),
            token: CapabilityTokenId::nil(),
            command_tx,
            cancel_token: tokio_util::sync::CancellationToken::new(),
            depth: 1,
            subagent_type: name.to_owned(),
            spawned_at: 1_700_000_000_000,
            status: status_tx,
            metrics: metrics_rx,
            isolated: false,
            mailbox_budget: MailboxBudget::new(),
        };
        if peer {
            self.tree.register_peer(id.clone(), handle).await
        } else {
            self.tree
                .register(id.clone(), AgentId::root(), handle)
                .await
        }
        .expect("register");
        if status != NodeState::Created {
            self.tree.set_state(&id, NodeState::Running).await;
        }
        match status {
            NodeState::Created | NodeState::Running => {}
            NodeState::Waiting => {
                self.tree.set_state(&id, NodeState::Waiting).await;
                self.tree
                    .stamp_wait_reason(&id, Some(WaitReason::AwaitingHumanInput))
                    .await
                    .expect("stamp");
            }
            other => self.tree.set_state(&id, other).await,
        }
        self.ops.insert(id.as_str().to_owned(), rx);
        id
    }

    async fn say(&self, id: &AgentId, text: &str) {
        self.spool
            .append(id.as_str(), text.as_bytes())
            .await
            .expect("append");
    }

    /// The ops the agent has received so far.
    fn received(&mut self, id: &AgentId) -> Vec<String> {
        let rx = self.ops.get_mut(id.as_str()).expect("registered agent");
        std::iter::from_fn(|| rx.try_recv().ok())
            .map(|op| format!("{op:?}"))
            .collect()
    }
}

/// `Ctrl+X s` through the local front door, the effect the loop runs for the tab, a draw.
async fn open_tab(rig: &mut Rig, world: &World) {
    assert_eq!(rig.key(Key::Ctrl('x')), InputAction::Consumed);
    assert_eq!(
        rig.key(Key::Char('s')),
        InputAction::OpenMissionControl(McTab::Agents)
    );
    mission_bridge::open_agents_tab_from(&mut rig.f.h.state, &world.tree, &world.spool).await;
    rig.paint();
}

/// The effect the event loop runs for an action the hub returned (the same functions).
async fn effect(rig: &mut Rig, world: &World, action: InputAction, read_only: bool) {
    let conversation = rig.f.h.conversation.id.clone();
    let state = &mut rig.f.h.state;
    match action {
        InputAction::OpenAgentWindow(id) => {
            mission_bridge::open_agent_window_from(state, &world.tree, &world.spool, id).await;
        }
        InputAction::AgentPauseResume(id) => {
            mission_bridge::pause_resume(state, &world.tree, read_only, &id).await;
        }
        InputAction::AgentKill(id) => mission_bridge::kill(state, &conversation, read_only, id),
        InputAction::Consumed => {}
        other => panic!("not an agent effect: {other:?}"),
    }
    rig.paint();
}

/// Select the row whose painted text contains `needle` with the real `j` key.
fn select(rig: &mut Rig, needle: &str) {
    rig.paint();
    rig.key(Key::Char('g'));
    for _ in 0..8 {
        rig.paint();
        if painted_row(rig, needle).is_some_and(|row| row.contains('▸')) {
            return;
        }
        rig.key(Key::Char('j'));
    }
    panic!("no row {needle} could be selected: {:?}", body_lines(rig));
}

fn painted_row(rig: &Rig, needle: &str) -> Option<String> {
    body_lines(rig).into_iter().find(|row| row.contains(needle))
}

fn copies(rig: &mut Rig) -> Arc<Mutex<Vec<String>>> {
    let sink = Arc::new(Mutex::new(Vec::new()));
    let seen = sink.clone();
    rig.f.h.state.clipboard_copier = Some(Arc::new(move |text| {
        seen.lock().unwrap().push(text.to_owned());
        ClipboardResult::Osc52Success
    }));
    sink
}

fn frame_text(rig: &Rig) -> Vec<String> {
    let rect = frame_rect(rig);
    let buffer = rig.f.buffer();
    (rect.y..rect.bottom())
        .map(|y| {
            (rect.x..rect.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect()
}

fn warnings(rig: &Rig) -> Vec<String> {
    rig.f
        .h
        .state
        .notifications
        .records()
        .filter(|record| {
            matches!(
                record.severity,
                rustain::adapters::tui::notifications::NoticeSeverity::Warning
            )
        })
        .map(|record| record.message.clone())
        .collect()
}

const READ_ONLY: &str = "This tab shows subagent output and is read-only. Switch to a conversation tab to make changes.";

async fn rig(width: u16, height: u16) -> Rig {
    Rig::plain(Workspace::new().await, width, height).await
}

#[tokio::test]
async fn the_agents_tab_shows_ownership_status_and_the_typed_reason_at_80_columns() {
    let mut world = world().await;
    world.spawn("reviewer", NodeState::Running, false).await;
    world.spawn("remote-peer", NodeState::Running, true).await;
    let asking = world.spawn("mcp-task", NodeState::Waiting, false).await;
    world.spawn("queued-one", NodeState::Created, false).await;
    world.say(&asking, "waiting for the operator\n").await;
    let mut rig80 = rig(80, 24).await;
    open_tab(&mut rig80, &world).await;
    assert_eq!(top_surface(&rig80), Some(Surface::MissionControl));
    assert_eq!(
        top_title(&rig80).as_deref(),
        Some("Mission Control · Agents")
    );
    let reviewer = painted_row(&rig80, "reviewer").expect("the owned row");
    assert!(
        reviewer.contains('♦') && reviewer.contains("running"),
        "{reviewer}"
    );
    let peer = painted_row(&rig80, "remote-peer").expect("the peer row");
    assert!(peer.contains('◇'), "peers are ◇: {peer}");
    // DF-19-18B-NARROW-WAITING-VISIBILITY: the typed reason survives 80 columns.
    let waiting = painted_row(&rig80, "mcp-task").expect("the waiting row");
    assert!(waiting.contains("awaiting your answer"), "{waiting}");
    assert!(
        !waiting.contains("mcp-task waiting"),
        "typed, not the static word: {waiting}"
    );
    let queued = painted_row(&rig80, "queued-one").expect("the queued row");
    assert!(queued.contains("queued"), "{queued}");
    // The same typed reason survives 60 columns when the type name is long (the compact form).
    let mut long = self::world().await;
    long.spawn(
        "an-extremely-long-subagent-type-name-that-fills-the-whole-row",
        NodeState::Waiting,
        false,
    )
    .await;
    let mut rig60 = rig(60, 16).await;
    open_tab(&mut rig60, &long).await;
    let row = painted_row(&rig60, "awaiting your answer")
        .unwrap_or_else(|| panic!("{:?}", body_lines(&rig60)));
    assert!(row.contains("awaiting your answer"), "{row}");
}

#[tokio::test]
async fn the_agents_empty_state_says_so() {
    let world = world().await;
    let mut rig = rig(80, 24).await;
    open_tab(&mut rig, &world).await;
    let painted = body_lines(&rig).join("\n");
    assert!(painted.contains("No agents active."), "{painted}");
    assert!(painted.contains("delegate or use plan mode"), "{painted}");
    // Nothing to act on: the rail offers neither Open, Pause nor Kill.
    let rail = rail_line(&rig);
    assert!(!rail.contains("Open") && !rail.contains("Kill"), "{rail}");
}

#[tokio::test]
async fn the_list_follows_registry_refreshes_with_id_stable_selection() {
    let mut world = world().await;
    world.spawn("alpha-agent", NodeState::Running, false).await;
    let b = world.spawn("beta-agent", NodeState::Running, false).await;
    world.spawn("gamma-agent", NodeState::Running, false).await;
    let mut rig = rig(80, 24).await;
    open_tab(&mut rig, &world).await;
    select(&mut rig, "beta-agent");
    assert_eq!(
        rig.f.h.state.mission_control.selected_id(),
        Some(b.as_str())
    );
    // A refresh with a new row (a spawn): still beta.
    world
        .spawn("aardvark-agent", NodeState::Running, false)
        .await;
    mission_bridge::refresh_roster_from(&mut rig.f.h.state, &world.tree).await;
    rig.paint();
    assert_eq!(
        rig.f.h.state.mission_control.selected_id(),
        Some(b.as_str()),
        "the selection is the agent, never an index"
    );
    assert!(painted_row(&rig, "aardvark-agent").is_some());
    // Beta leaves the registry: the placeholder takes its place and nothing is silently
    // rebound to a neighbour.
    world.tree.deregister(&b).await;
    mission_bridge::refresh_roster_from(&mut rig.f.h.state, &world.tree).await;
    rig.paint();
    let gone = painted_row(&rig, "no longer available").unwrap_or_else(|| {
        panic!(
            "a removed selection shows the placeholder: {:?}",
            body_lines(&rig)
        )
    });
    assert!(gone.contains("beta-agent"), "{gone}");
    assert!(
        painted_row(&rig, "gamma-agent").is_some_and(|row| !row.contains('▸')),
        "the next row is never silently selected"
    );
    // Enter on the placeholder acts on nothing.
    assert_eq!(rig.key(Key::Enter), InputAction::Consumed);
    assert_eq!(top_surface(&rig), Some(Surface::MissionControl));
}

#[tokio::test]
async fn p_sends_exactly_one_pause_or_resume_op_through_the_registry() {
    let mut world = world().await;
    let running = world.spawn("runner", NodeState::Running, false).await;
    let sleeper = world.spawn("sleeper", NodeState::Suspended, false).await;
    let finished = world.spawn("finished", NodeState::Completed, false).await;
    let mut rig = rig(80, 24).await;
    open_tab(&mut rig, &world).await;
    // A running agent: `p` asks, the effect sends exactly one Pause.
    select(&mut rig, "runner");
    assert!(rail_line(&rig).contains("Pause"), "{}", rail_line(&rig));
    let action = rig.key(Key::Char('p'));
    assert_eq!(action, InputAction::AgentPauseResume(running.clone()));
    assert!(
        world.received(&running).is_empty(),
        "the key itself sends nothing"
    );
    effect(&mut rig, &world, action, false).await;
    assert_eq!(world.received(&running), vec!["Pause".to_owned()]);
    // A suspended agent resumes.
    select(&mut rig, "sleeper");
    assert!(rail_line(&rig).contains("Resume"), "{}", rail_line(&rig));
    let action = rig.key(Key::Char('p'));
    assert_eq!(action, InputAction::AgentPauseResume(sleeper.clone()));
    effect(&mut rig, &world, action, false).await;
    assert_eq!(world.received(&sleeper), vec!["Resume".to_owned()]);
    // A finished agent: no `p` hint is painted and the key is inert.
    select(&mut rig, "finished");
    assert!(!rail_line(&rig).contains("Pause") && !rail_line(&rig).contains("Resume"));
    assert_eq!(rig.key(Key::Char('p')), InputAction::Consumed);
    assert!(world.received(&finished).is_empty());
    // Even forced through the effect (a race: the state changed after the key), a state that
    // allows neither is told, not sent.
    effect(
        &mut rig,
        &world,
        InputAction::AgentPauseResume(finished.clone()),
        false,
    )
    .await;
    assert!(world.received(&finished).is_empty());
    assert!(
        rig.f
            .h
            .state
            .notifications
            .records()
            .any(|record| record.message.contains("Cannot pause/resume agent in"))
    );
}

// Mutation M5's killer: `x` raises the 21-6 `AgentKill` decision for exactly that agent in the
// asking conversation and sends NOTHING (the confirmation decides; the op is the event loop's,
// after it); a read-only tab refuses with the existing warning and raises nothing.
#[tokio::test]
async fn agent_kill_raises_the_decision_and_sends_nothing() {
    let mut world = world().await;
    let target = world.spawn("victim-agent", NodeState::Running, false).await;
    let bystander = world.spawn("bystander", NodeState::Running, false).await;
    let mut rig = rig(80, 24).await;
    open_tab(&mut rig, &world).await;
    select(&mut rig, "victim-agent");
    assert!(rail_line(&rig).contains("Kill"), "{}", rail_line(&rig));
    let action = rig.key(Key::Char('x'));
    assert_eq!(action, InputAction::AgentKill(target.clone()));
    assert!(
        !rig.f.h.state.decisions.holds(DecisionKind::AgentKill),
        "the key raises nothing"
    );
    effect(&mut rig, &world, action, false).await;
    let queued: Vec<_> = rig.f.h.state.decisions.items().collect();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].kind, DecisionKind::AgentKill);
    assert_eq!(queued[0].source, target.as_str());
    assert_eq!(
        queued[0].conversation_id.as_deref(),
        Some(rig.f.h.conversation.id.as_str())
    );
    assert!(
        world.received(&target).is_empty(),
        "no op before the decision"
    );
    assert!(world.received(&bystander).is_empty());
    assert_eq!(
        world.tree.list().await.len(),
        2,
        "nothing was killed: the confirmation has not been answered"
    );
    assert!(
        world
            .tree
            .list()
            .await
            .into_iter()
            .all(|entry| entry.to_view().current_status == NodeState::Running),
        "no agent left Running before the confirmation is answered"
    );
    // The confirmation is a decision frame naming the agent over the hub.
    assert!(is_open(&rig, Surface::Decision));
    assert!(
        lines(&rig)
            .join("\n")
            .contains("Kill victim-agent and its subtree?")
    );
}

#[tokio::test]
async fn read_only_tab_agent_actions_refuse_and_send_nothing() {
    let mut world = world().await;
    let running = world.spawn("runner", NodeState::Running, false).await;
    let mut rig = rig(80, 24).await;
    open_tab(&mut rig, &world).await;
    select(&mut rig, "runner");
    for key in [Key::Char('p'), Key::Char('x')] {
        let action = rig.key(key);
        effect(&mut rig, &world, action, true).await;
    }
    assert!(
        world.received(&running).is_empty(),
        "a read-only tab sends no op"
    );
    assert!(
        !rig.f.h.state.decisions.holds(DecisionKind::AgentKill),
        "and raises no decision"
    );
    assert_eq!(
        warnings(&rig),
        vec![READ_ONLY.to_owned()],
        "the existing warning, once (the repeat is coalesced)"
    );
    // Control: the same key on a writable tab acts.
    let action = rig.key(Key::Char('p'));
    effect(&mut rig, &world, action, false).await;
    assert_eq!(world.received(&running), vec!["Pause".to_owned()]);
}

#[tokio::test]
async fn enter_opens_the_agent_window_with_facts_and_the_transcript_and_esc_returns_to_the_hub() {
    let mut world = world().await;
    let coder = world.spawn("coder-agent", NodeState::Running, false).await;
    world.spawn("other-agent", NodeState::Running, false).await;
    world
        .say(
            &coder,
            "# Plan\n\nbuilding the project\nfinished step one\n",
        )
        .await;
    let mut rig = rig(80, 24).await;
    open_tab(&mut rig, &world).await;
    select(&mut rig, "coder-agent");
    let composer = composer_rows(&rig);
    let status = status_row(&rig);
    let action = rig.key(Key::Enter);
    assert_eq!(action, InputAction::OpenAgentWindow(coder.clone()));
    effect(&mut rig, &world, action, false).await;
    assert_eq!(top_surface(&rig), Some(Surface::AgentWindow));
    assert_eq!(top_title(&rig).as_deref(), Some("Agent · coder-agent"));
    let text = frame_text(&rig).join("\n");
    for expected in [
        "Agent: coder-agent",
        "Status: running",
        "Ownership: ♦",
        "Tokens:",
        "building the project",
        "Plan",
    ] {
        assert!(text.contains(expected), "{expected}: {text}");
    }
    // PC-2 / RU-21-7-11: no tool-call rows, no Inspect, no model/tools stubs, anywhere.
    for banned in ["Inspect", "Change model", "Update tools", "[m]", "[t]"] {
        assert!(!text.contains(banned), "{banned}: {text}");
    }
    let rail = rail_line(&rig);
    for hint in ["Pause", "Kill", "Back"] {
        assert!(rail.contains(hint), "{hint}: {rail}");
    }
    // The composer and the status row are untouched by the window.
    assert_eq!(composer_rows(&rig), composer);
    assert_eq!(status_row(&rig), status);
    // Esc returns to the hub on the Agents tab with its selection.
    assert_eq!(rig.key(Key::Esc), InputAction::Consumed);
    rig.paint();
    assert_eq!(top_surface(&rig), Some(Surface::MissionControl));
    assert_eq!(
        rig.f.h.state.mission_control.selected_id(),
        Some(coder.as_str())
    );
    assert!(
        rig.f.h.state.agent_window.is_none(),
        "the payload leaves with the frame"
    );
}

// Mutation M6's killer: the transcript passes the display sanitizer at the line builder (no
// escape, control or bidi override is ever a painted cell, Rendered or Source), while `c`
// copies the ORIGINAL spooled text.
#[tokio::test]
async fn transcript_controls_never_reach_cells_and_copy_returns_original() {
    let original = "plain \u{1b}[31mred\u{1b}[0m \u{202e}rtl\u{2066}iso\u{7} end\n";
    let mut world = world().await;
    let id = world.spawn("noisy-agent", NodeState::Running, false).await;
    world.say(&id, original).await;
    let mut rig = rig(130, 40).await;
    open_tab(&mut rig, &world).await;
    select(&mut rig, "noisy-agent");
    let action = rig.key(Key::Enter);
    effect(&mut rig, &world, action, false).await;
    let sink = copies(&mut rig);
    for mode_key in [None, Some(Key::Char('v'))] {
        if let Some(key) = mode_key {
            rig.key(key);
            rig.paint();
        }
        let cells: Vec<char> = rig
            .f
            .buffer()
            .content
            .iter()
            .flat_map(|cell| cell.symbol().chars())
            .collect();
        assert!(
            !cells.iter().any(|c| {
                c.is_control()
                    || ('\u{202a}'..='\u{202e}').contains(c)
                    || ('\u{2066}'..='\u{2069}').contains(c)
            }),
            "no escape, control or bidi override is a painted cell"
        );
        assert!(
            frame_text(&rig).join("\n").contains("red"),
            "{:?}",
            frame_text(&rig)
        );
    }
    assert!(sink.lock().unwrap().is_empty());
    rig.key(Key::Char('c'));
    assert_eq!(
        &*sink.lock().unwrap(),
        &[original.to_owned()],
        "copy returns the spooled text, not the display"
    );
}

#[tokio::test]
async fn the_window_follows_its_agent_and_a_removed_agent_is_a_stale_copy() {
    let mut world = world().await;
    let id = world.spawn("fading-agent", NodeState::Running, false).await;
    world.say(&id, "first line\n").await;
    let mut rig = rig(80, 24).await;
    open_tab(&mut rig, &world).await;
    select(&mut rig, "fading-agent");
    let action = rig.key(Key::Enter);
    effect(&mut rig, &world, action, false).await;
    assert!(frame_text(&rig).join("\n").contains("first line"));
    // New output reaches the open window through the cache the events refresh.
    world.say(&id, "second line\n").await;
    mission_bridge::refresh_spool_tail_from(&mut rig.f.h.state, &world.spool, id.as_str()).await;
    rig.paint();
    assert!(frame_text(&rig).join("\n").contains("second line"));
    // The agent leaves the registry: the window keeps its last facts under the gone line, the
    // action hints are gone and `Esc` is still the way out.
    world.tree.deregister(&id).await;
    mission_bridge::refresh_roster_from(&mut rig.f.h.state, &world.tree).await;
    rig.paint();
    let text = frame_text(&rig).join("\n");
    assert!(text.contains("This agent is no longer running."), "{text}");
    assert!(text.contains("Agent: fading-agent"), "{text}");
    let rail = rail_line(&rig);
    assert!(!rail.contains("Pause") && !rail.contains("Kill"), "{rail}");
    assert!(rail.contains("Back"), "{rail}");
    assert_eq!(rig.key(Key::Char('p')), InputAction::Consumed);
    assert_eq!(rig.key(Key::Char('x')), InputAction::Consumed);
    assert_eq!(top_surface(&rig), Some(Surface::AgentWindow));
    rig.key(Key::Esc);
    assert_eq!(top_surface(&rig), Some(Surface::MissionControl));
}

#[tokio::test]
async fn the_window_p_and_x_ask_for_the_same_effects_as_the_tab() {
    let mut world = world().await;
    let id = world.spawn("worker-agent", NodeState::Running, false).await;
    let mut rig = rig(130, 30).await;
    open_tab(&mut rig, &world).await;
    select(&mut rig, "worker-agent");
    let action = rig.key(Key::Enter);
    effect(&mut rig, &world, action, false).await;
    assert_eq!(top_surface(&rig), Some(Surface::AgentWindow));
    let action = rig.key(Key::Char('p'));
    assert_eq!(action, InputAction::AgentPauseResume(id.clone()));
    effect(&mut rig, &world, action, false).await;
    assert_eq!(world.received(&id), vec!["Pause".to_owned()]);
    let action = rig.key(Key::Char('x'));
    assert_eq!(action, InputAction::AgentKill(id.clone()));
    effect(&mut rig, &world, action, false).await;
    assert!(rig.f.h.state.decisions.holds(DecisionKind::AgentKill));
    assert!(
        world.received(&id).is_empty(),
        "the kill is a decision, never a direct op"
    );
}

#[tokio::test]
async fn the_slash_filter_narrows_the_rows_and_esc_clears_before_it_closes() {
    let mut world = world().await;
    world
        .spawn("reviewer-agent", NodeState::Running, false)
        .await;
    world.spawn("coder-agent", NodeState::Running, false).await;
    world.spawn("tester-agent", NodeState::Running, false).await;
    let mut rig = rig(80, 24).await;
    open_tab(&mut rig, &world).await;
    assert!(rail_line(&rig).contains("Search"), "{}", rail_line(&rig));
    rig.key(Key::Char('/'));
    rig.paint();
    for c in "coder".chars() {
        rig.key(Key::Char(c));
    }
    assert_eq!(rig.key(Key::Enter), InputAction::Consumed);
    rig.paint();
    assert!(painted_row(&rig, "coder-agent").is_some());
    assert!(
        painted_row(&rig, "reviewer-agent").is_none(),
        "{:?}",
        body_lines(&rig)
    );
    assert!(painted_row(&rig, "tester-agent").is_none());
    assert!(
        lines(&rig).join("\n").contains("/ coder"),
        "the kept query is echoed"
    );
    // Esc clears the kept filter first; the hub stays; the next Esc closes it.
    rig.key(Key::Esc);
    rig.paint();
    assert_eq!(top_surface(&rig), Some(Surface::MissionControl));
    assert!(
        painted_row(&rig, "reviewer-agent").is_some(),
        "the filter is cleared"
    );
    rig.key(Key::Esc);
    assert_eq!(top_surface(&rig), None);
}

#[tokio::test]
async fn the_palette_opens_agents_and_adapters_at_every_width() {
    for (width, height) in [(60, 16), (80, 24), (130, 30)] {
        let mut rig = rig(width, height).await;
        let action = choose(&mut rig, PaletteAction::OpenSurface(SurfaceTarget::Agents));
        assert_eq!(action, InputAction::OpenMissionControl(McTab::Agents));
        assert_eq!(
            top_title(&rig).as_deref(),
            Some("Mission Control · Agents"),
            "{width}x{height}"
        );
        let action = choose(
            &mut rig,
            PaletteAction::OpenSurface(SurfaceTarget::Adapters),
        );
        assert_eq!(action, InputAction::OpenAdapters, "{width}x{height}");
    }
    let registry = rustain::adapters::command_registry::CommandRegistry::new();
    let mut palette = rustain::adapters::palette_registry::PaletteRegistry::new();
    palette.populate_from_command_registry(&registry);
    let entries = palette.all_entries();
    for (name, shortcut) in [("agents", "Ctrl+X, S"), ("adapters", "Ctrl+X, A")] {
        let entry = entries
            .iter()
            .find(|entry| entry.name == name)
            .unwrap_or_else(|| panic!("palette entry {name}"));
        assert_eq!(entry.shortcut.as_deref(), Some(shortcut), "{name}");
    }
}

// AC13 / RU-21 / RU-21-7-11: the inspector's stubs and its conversation-tab opener are gone from
// the registry, and bare `c` is Copy in EVERY context that binds it (no exception remains).
#[test]
fn no_stub_rows_remain_and_bare_c_is_copy_in_every_context() {
    use rustain::adapters::tui::keymap::{CommandId, Context, Key as K, table};
    let rows = table::all_rows();
    for row in &rows {
        assert!(
            !row.label.contains("Change model") && !row.label.contains("Update tools"),
            "{:?}",
            row.label
        );
        if row.keys == [K::Char('c')] {
            let text_field = matches!(
                row.context,
                Context::TextEntry | Context::Input | Context::SearchTyping
            );
            assert!(
                text_field || row.copy,
                "{:?} binds bare `c` as a non-copy row",
                row.context
            );
        }
    }
    // The agent window's `c` is the Copy row and its other letters are the registry's.
    let agent: Vec<_> = rows
        .iter()
        .filter(|row| row.context == Context::AgentWindow)
        .collect();
    assert!(
        agent
            .iter()
            .any(|row| matches!(row.command, CommandId::Mission(_)) && row.copy)
    );
    let letters: Vec<_> = agent
        .iter()
        .filter_map(|row| match row.keys {
            [K::Char(c)] => Some(*c),
            _ => None,
        })
        .collect();
    for expected in ['p', 'x', 'c', '/'] {
        assert!(letters.contains(&expected), "{expected}: {letters:?}");
    }
    assert!(
        !letters.contains(&'m') && !letters.contains(&'t'),
        "{letters:?}"
    );
}

/// Select `action` in a real palette frame (the palette's own Enter path).
fn choose(rig: &mut Rig, action: PaletteAction) -> InputAction {
    rig.key(Key::Ctrl('p'));
    rig.f
        .h
        .state
        .command_palette
        .filtered_entries
        .push(PaletteEntry {
            name: "selected fixture".into(),
            description: String::new(),
            shortcut: None,
            scope: PaletteScope::All,
            action,
        });
    rig.key(Key::Enter)
}

// 7C-02: an untrusted scalar (the agent's type label) is one safe line at the builder: a
// newline or CR in it can never become a second fact row, and no escape, OSC, bidi or
// zero-width scalar reaches a painted cell.
#[tokio::test]
async fn an_agent_type_label_is_one_safe_fact_line_and_never_a_fabricated_row() {
    let mut world = world().await;
    world
        .spawn(
            "evil\nTokens: 999 in / 0 out\r\u{1b}]0;pwned\u{7}\u{202e}x\u{200b}y",
            NodeState::Running,
            false,
        )
        .await;
    let mut rig = rig(130, 40).await;
    open_tab(&mut rig, &world).await;
    let action = rig.key(Key::Char('j'));
    assert_eq!(action, InputAction::Consumed);
    rig.paint();
    let action = rig.key(Key::Enter);
    effect(&mut rig, &world, action, false).await;
    let text = frame_text(&rig);
    let joined = text.join("\n");
    assert!(
        joined.contains("Agent: evil Tokens: 999 in / 0 out"),
        "{joined}"
    );
    assert!(
        !text.iter().any(|row| row
            .trim_start_matches('│')
            .trim_start()
            .starts_with("Tokens: 999")),
        "no fabricated fact row: {joined}"
    );
    let cells: Vec<char> = rig
        .f
        .buffer()
        .content
        .iter()
        .flat_map(|cell| cell.symbol().chars())
        .collect();
    assert!(
        !cells.iter().any(|c| c.is_control() || *c == '\u{202e}'),
        "no control or bidi scalar is a painted cell"
    );
}

// 7C-01: the production path (a registry removal, `apply_roster`) drops the agent's cache
// entry before the window follows; the stale copy still shows and copies the last transcript.
#[tokio::test]
async fn a_removed_agent_keeps_its_last_transcript_and_still_copies_it() {
    let mut world = world().await;
    let id = world.spawn("fading-agent", NodeState::Running, false).await;
    world.say(&id, "first line\nsecond line\n").await;
    let mut rig = rig(80, 24).await;
    open_tab(&mut rig, &world).await;
    select(&mut rig, "fading-agent");
    let action = rig.key(Key::Enter);
    effect(&mut rig, &world, action, false).await;
    assert!(frame_text(&rig).join("\n").contains("second line"));
    world.tree.deregister(&id).await;
    mission_bridge::refresh_roster_from(&mut rig.f.h.state, &world.tree).await;
    assert!(
        rig.f.h.state.agent_panel_state.spool_tail_cache.is_empty(),
        "the cache entry left with the agent"
    );
    rig.paint();
    let text = frame_text(&rig).join("\n");
    assert!(text.contains("This agent is no longer running."), "{text}");
    assert!(text.contains("second line"), "the transcript stays: {text}");
    assert!(!text.contains("no output yet"), "{text}");
    let sink = copies(&mut rig);
    rig.key(Key::Char('c'));
    assert_eq!(
        &*sink.lock().unwrap(),
        &["first line\nsecond line\n".to_owned()],
        "the stale copy copies the last transcript"
    );
}

// 7C-06: a valid multibyte transcript longer than the 8 KiB tail is cut at a character
// boundary: Copy is a suffix of the original bytes, never a lossy conversion.
#[tokio::test]
async fn a_multibyte_tail_cut_by_the_ring_copies_original_bytes_at_a_character_boundary() {
    let original = "€".repeat(4000);
    let mut world = world().await;
    let id = world
        .spawn("unicode-agent", NodeState::Running, false)
        .await;
    world.say(&id, &original).await;
    let mut rig = rig(130, 40).await;
    open_tab(&mut rig, &world).await;
    select(&mut rig, "unicode-agent");
    let action = rig.key(Key::Enter);
    effect(&mut rig, &world, action, false).await;
    let sink = copies(&mut rig);
    rig.key(Key::Char('c'));
    let copied = sink.lock().unwrap().clone();
    assert_eq!(copied.len(), 1, "{:?}", warnings(&rig));
    assert!(!copied[0].contains('\u{fffd}'), "no replacement character");
    assert!(
        original.ends_with(copied[0].as_str()),
        "a suffix of the original"
    );
    assert!(copied[0].len() >= 8192 - 6 && copied[0].len() <= 8192);
}

// 7C-03: `c` on a list tab copies the selected row's text (or its detail text in the detail
// pane), all or nothing; no selection has no hint and copies nothing.
#[tokio::test]
async fn hub_copy_takes_the_selected_row_or_its_detail() {
    let mut world = world().await;
    world
        .spawn("reviewer-agent", NodeState::Running, false)
        .await;
    let mut rig = rig(130, 30).await;
    let sink = copies(&mut rig);
    open_tab(&mut rig, &world).await;
    select(&mut rig, "reviewer-agent");
    assert!(rail_line(&rig).contains("[c] Copy"), "{}", rail_line(&rig));
    let want_row = rig
        .f
        .h
        .state
        .mission_control
        .selected_row()
        .map(|row| row.text.clone())
        .expect("a selected row");
    rig.key(Key::Char('c'));
    assert_eq!(&*sink.lock().unwrap(), &[want_row]);
    // Detail focus (a wide frame): the detail text.
    rig.key(Key::Tab);
    rig.paint();
    let want_detail = rig
        .f
        .h
        .state
        .mission_control
        .selected_row()
        .map(|row| row.detail.clone())
        .expect("a selected row");
    assert!(
        want_detail.starts_with("Agent: reviewer-agent"),
        "{want_detail}"
    );
    rig.key(Key::Char('c'));
    assert_eq!(sink.lock().unwrap().last(), Some(&want_detail));
    assert_eq!(sink.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn hub_copy_refuses_above_the_bound_and_without_a_selection_copies_nothing() {
    use rustain::adapters::tui::mission_control::McRow;
    use rustain::adapters::tui::presentation::reducer;
    let mut rig = rig(130, 30).await;
    let sink = copies(&mut rig);
    rig.f.h.state.mission_control.register_probe(vec![McRow {
        id: "big".to_owned(),
        text: "big row".to_owned(),
        detail: "x".repeat(8 * 1024 * 1024 + 1),
        compact: None,
        keep: Vec::new(),
    }]);
    let _ = reducer::open_mission_control(&mut rig.f.h.state, McTab::Probe);
    rig.paint();
    rig.key(Key::Char('j'));
    rig.paint();
    assert!(rail_line(&rig).contains("[c] Copy"), "{}", rail_line(&rig));
    rig.key(Key::Tab);
    rig.paint();
    rig.key(Key::Char('c'));
    assert!(
        sink.lock().unwrap().is_empty(),
        "above the bound: nothing is copied"
    );
    let notices: Vec<String> = rig
        .f
        .h
        .state
        .notifications
        .records()
        .map(|record| record.message.clone())
        .collect();
    assert!(notices.iter().any(|m| m.contains("8 MiB")), "{notices:?}");
    // The row text itself (master focus) is within the bound and copies whole.
    rig.key(Key::Tab);
    rig.paint();
    rig.key(Key::Char('c'));
    assert_eq!(&*sink.lock().unwrap(), &["big row".to_owned()]);
    // An empty list: no hint, the key is inert.
    rig.f.h.state.mission_control.register_probe(Vec::new());
    rig.paint();
    assert!(!rail_line(&rig).contains("[c] Copy"), "{}", rail_line(&rig));
    rig.key(Key::Char('c'));
    assert_eq!(sink.lock().unwrap().len(), 1);
}
