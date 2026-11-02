//! Story 21.3 conformance leg K4 (panels): the task drill-down, the wave/reorder Chat
//! sub-states, the Tasks / Agents / A2A log / Room / Artifacts sidebar panels and the
//! wave overlay, each witnessed BY VALUE through the real local front door
//! (`support::press` -> `app::handle_input`) by the shared `witness::run` harness.

use std::sync::Arc;

use rustain::adapters::tui::app::{InputAction, handle_input};
use rustain::adapters::tui::keymap::{Context, Key};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::events::DomainInputEvent;
use rustain::domain::models::visual::{OverlayType, PanelType};
use rustain::domain::models::{
    AgentId, ArtifactId, ArtifactKind, CapabilityTokenId, ContentHash, Direction, EvidenceArtifact,
    FocusState, HostBinding, OrchestrationRoom, OrchestrationRoomId, PermissionMode, ProvenanceTag,
    ReviewStatus, RoomEvent,
};
use rustain::domain::services::patch_review::MergeBackPolicy;
use rustain::domain::services::transparency::{TransparencyKind, TransparencyRow};

use super::support::{StubWave, fresh, press};
use super::witness::{Case, W, local, run, w, wp};

// ── fixtures ──────────────────────────────────────────────────────────────

/// Chat inside the drill-down of task 3, result body scrolled to row 5.
fn drill_state() -> TuiState {
    let mut state = fresh();
    state.focus = FocusState::Chat;
    state.task_panel_state.drill_down_task = Some(3);
    state.task_panel_state.detail_scroll_offset = 5;
    state
}

/// Chat while task 2 is being moved (reorder mode on, original order kept).
fn reorder_state() -> TuiState {
    let mut state = fresh();
    state.focus = FocusState::Chat;
    state.task_panel_state.reorder_mode_for = Some(2);
    state.task_panel_state.reorder_original_order = Some(vec![1, 2, 3]);
    state
}

/// The Tasks sidebar panel focused: three tasks, the second selected.
fn tasks_state() -> TuiState {
    let mut state = fresh();
    state.focus = FocusState::Sidebar {
        panel: PanelType::Tasks,
        selected: 1,
    };
    state.sidebar_selected = 1;
    state.task_panel_state.selected_index = 1;
    state.task_panel_state.task_count = 3;
    state
}

/// A sidebar panel focused with nothing loaded.
fn panel_state(panel: PanelType) -> TuiState {
    let mut state = fresh();
    state.focus = FocusState::Sidebar { panel, selected: 0 };
    state
}

/// The Tasks panel focused while its second task is being moved.
fn reorder_tasks_state() -> TuiState {
    let mut state = tasks_state();
    state.task_panel_state.reorder_mode_for = Some(2);
    state
}

fn tasks_salient(state: &TuiState) -> String {
    let t = &state.task_panel_state;
    let (idx, drill, exp) = (t.selected_index, t.drill_down_task, t.expanded_detail);
    let (scroll, reorder) = (t.detail_scroll_offset, t.reorder_mode_for);
    format!(
        "{:?} idx={idx} drill={drill:?} exp={exp} scroll={scroll} reorder={reorder:?}",
        state.focus
    )
}

fn panel_salient(state: &TuiState) -> String {
    format!("{:?} sel={}", state.focus, state.sidebar_selected)
}

// ── ChatDrill ─────────────────────────────────────────────────────────────

/// Regression caught: a drill-down verb (`r s e p c`, scroll, `Enter`, `Esc`) acts on the wrong task or state.
#[test]
#[rustfmt::skip]
fn chat_drill_rows_act_on_the_drilled_task() {
    const ROWS: &[W] = &[
        w(&[Key::Char('c')], "CopyTaskResult { plan_id: None, task_number: 3 } | Chat idx=0 drill=Some(3) exp=false scroll=5 reorder=None"),
        w(&[Key::Char('r')], "TaskRetry(3) | Chat idx=0 drill=Some(3) exp=false scroll=5 reorder=None"),
        w(&[Key::Char('s')], "TaskSkip(3) | Chat idx=0 drill=Some(3) exp=false scroll=5 reorder=None"),
        w(&[Key::Char('e')], "TaskEdit(3) | Chat idx=0 drill=Some(3) exp=false scroll=5 reorder=None"),
        w(&[Key::Char('p')], "TaskPause(3) | Chat idx=0 drill=Some(3) exp=false scroll=5 reorder=None"),
        w(&[Key::Char('j')], "Consumed | Chat idx=0 drill=Some(3) exp=false scroll=6 reorder=None"),
        w(&[Key::Down], "Consumed | Chat idx=0 drill=Some(3) exp=false scroll=6 reorder=None"),
        w(&[Key::Char('k')], "Consumed | Chat idx=0 drill=Some(3) exp=false scroll=4 reorder=None"),
        w(&[Key::Up], "Consumed | Chat idx=0 drill=Some(3) exp=false scroll=4 reorder=None"),
        w(&[Key::Char('g')], "Consumed | Chat idx=0 drill=Some(3) exp=false scroll=0 reorder=None"),
        w(&[Key::Char('G')], "Consumed | Chat idx=0 drill=Some(3) exp=false scroll=65535 reorder=None"),
        w(&[Key::Enter], "Consumed | Chat idx=0 drill=Some(3) exp=true scroll=0 reorder=None"),
        w(&[Key::Esc], "Consumed | Sidebar { panel: Tasks, selected: 0 } idx=0 drill=None exp=false scroll=0 reorder=None"),
    ];
    // The non-character Chat rows the exclusive drill-down inherits (R4).
    const INHERITED: &[W] = &[
        w(&[Key::Ctrl('p')], "Consumed | Overlay(CommandPalette) idx=0 drill=Some(3) exp=false scroll=5 reorder=None"),
        w(&[Key::Ctrl('c')], "CancelOrQuit | Chat idx=0 drill=Some(3) exp=false scroll=5 reorder=None"),
        w(&[Key::Ctrl('d')], "ScrollHalfPageDown | Chat idx=0 drill=Some(3) exp=false scroll=5 reorder=None"),
        w(&[Key::Ctrl('u')], "ScrollHalfPageUp | Chat idx=0 drill=Some(3) exp=false scroll=5 reorder=None"),
        w(&[Key::Ctrl('f')], "ScrollFullPageDown | Chat idx=0 drill=Some(3) exp=false scroll=5 reorder=None"),
        w(&[Key::Ctrl('b')], "ScrollFullPageUp | Chat idx=0 drill=Some(3) exp=false scroll=5 reorder=None"),
        w(&[Key::Ctrl('t')], "NewTab | Chat idx=0 drill=Some(3) exp=false scroll=5 reorder=None"),
        w(&[Key::Tab], "CycleInvocationInFocusedTurn | Chat idx=0 drill=Some(3) exp=false scroll=5 reorder=None"),
        w(&[Key::ShiftTab], "SwitchToPrevTab | Chat idx=0 drill=Some(3) exp=false scroll=5 reorder=None"),
        w(&[Key::Alt('v')], "RequestClipboardPaste | Chat idx=0 drill=Some(3) exp=false scroll=5 reorder=None"),
    ];
    run(&Case { inherited: INHERITED, ..local(Context::ChatDrill, drill_state, tasks_salient, ROWS) });
}

// ── ChatWave / WaveOverlay ────────────────────────────────────────────────

/// A retained three-spoke wave in Chat focus.
fn wave_state() -> TuiState {
    let mut state = fresh();
    state.focus = FocusState::Chat;
    state.wave_run = Some(Arc::new(StubWave(3)));
    state
}

/// The wave overlay open on the second of three spokes.
fn overlay_state() -> TuiState {
    let mut state = wave_state();
    state.focus = FocusState::Overlay(OverlayType::WaveOverlay);
    state.wave_overlay_selected = 1;
    state
}

fn wave_salient(state: &TuiState) -> String {
    let (sel, div) = (state.wave_overlay_selected, state.wave_diverge_open);
    format!("{:?} sel={sel} div={div}", state.focus)
}

/// Regression caught: `Esc` / `Enter` on a retained wave dismiss or open the wrong surface (divergence view first).
#[test]
#[rustfmt::skip]
fn chat_wave_rows_dismiss_or_open_the_retained_wave() {
    const ROWS: &[W] = &[
        w(&[Key::Esc], "DismissWave | Chat sel=0 div=false"),
        wp(&[Key::Esc], "CloseDivergeView | Chat sel=0 div=true", |s| s.wave_diverge_open = true),
        w(&[Key::Enter], "OpenWaveOverlay | Chat sel=0 div=false"),
        w(&[Key::Char('d')], "OpenDivergeView | Chat sel=0 div=false"),
        w(&[Key::Char('s')], "CloseDivergeView | Chat sel=0 div=false"),
        wp(&[Key::Char('r')], "OpenWaveOverlay | Overlay(WaveOverlay) sel=0 div=false", |s| s.wave_overlay_selected = 3),
    ];
    // Without a retained wave the same three keys do nothing (the arms are conditional): off-state (R3).
    const OFF: &[W] = &[
        wp(&[Key::Char('d')], "Ignored | Chat sel=0 div=false", |s| s.wave_run = None),
        wp(&[Key::Char('s')], "Ignored | Chat sel=0 div=false", |s| s.wave_run = None),
        wp(&[Key::Char('r')], "Ignored | Chat sel=0 div=false", |s| s.wave_run = None),
    ];
    run(&Case { off: OFF, ..local(Context::ChatWave, wave_state, wave_salient, ROWS) });
}

/// Regression caught: the overlay selection overruns the spoke count or a verb acts on the wrong spoke.
#[test]
#[rustfmt::skip]
fn wave_overlay_rows_select_open_close_and_act_on_the_selected_spoke() {
    const ROWS: &[W] = &[
        w(&[Key::Char('j')], "Consumed | Overlay(WaveOverlay) sel=2 div=false"),
        w(&[Key::Down], "Consumed | Overlay(WaveOverlay) sel=2 div=false"),
        w(&[Key::Char('k')], "Consumed | Overlay(WaveOverlay) sel=0 div=false"),
        w(&[Key::Up], "Consumed | Overlay(WaveOverlay) sel=0 div=false"),
        w(&[Key::Enter], "WaveDrillSpoke(1) | Overlay(WaveOverlay) sel=1 div=false"),
        w(&[Key::Esc], "CloseWaveOverlay | Overlay(WaveOverlay) sel=1 div=false"),
        w(&[Key::Ctrl('c')], "CloseWaveOverlay | Overlay(WaveOverlay) sel=1 div=false"),
        w(&[Key::Char('r')], "WaveRerunSpoke(1) | Overlay(WaveOverlay) sel=1 div=false"),
        w(&[Key::Char('d')], "OpenDivergeView | Overlay(WaveOverlay) sel=1 div=false"),
        w(&[Key::Char('s')], "CloseDivergeView | Overlay(WaveOverlay) sel=1 div=false"),
        w(&[Key::Ctrl('q')], "Quit | Overlay(WaveOverlay) sel=1 div=false"),
    ];
    run(&local(Context::WaveOverlay, overlay_state, wave_salient, ROWS));
}

// ── ChatReorder / SidebarTasks / TasksReorder ─────────────────────────────

/// Regression caught: `Esc` in reorder mode no longer cancels the reorder (it would leave Chat or the mode).
#[test]
#[rustfmt::skip]
fn chat_reorder_esc_cancels_the_reorder() {
    const ROWS: &[W] = &[
        w(&[Key::Esc], "TaskReorderCancel | Chat idx=0 drill=None exp=false scroll=0 reorder=Some(2)"),
    ];
    run(&local(Context::ChatReorder, reorder_state, tasks_salient, ROWS));
}

/// Regression caught: a Tasks-panel verb targets the wrong task, or `Enter` opens the wrong drill-down.
#[test]
#[rustfmt::skip]
fn sidebar_tasks_rows_select_act_and_drill_the_selected_task() {
    const ROWS: &[W] = &[
        w(&[Key::Up], "Consumed | Sidebar { panel: Tasks, selected: 0 } idx=0 drill=None exp=false scroll=0 reorder=None"),
        w(&[Key::Down], "Consumed | Sidebar { panel: Tasks, selected: 2 } idx=2 drill=None exp=false scroll=0 reorder=None"),
        w(&[Key::Char('x')], "TaskCancelPlan | Sidebar { panel: Tasks, selected: 1 } idx=1 drill=None exp=false scroll=0 reorder=None"),
        w(&[Key::Char('p')], "TaskPause(1) | Sidebar { panel: Tasks, selected: 1 } idx=1 drill=None exp=false scroll=0 reorder=None"),
        w(&[Key::Char('s')], "TaskSkip(1) | Sidebar { panel: Tasks, selected: 1 } idx=1 drill=None exp=false scroll=0 reorder=None"),
        w(&[Key::Enter], "Consumed | Chat idx=1 drill=Some(2) exp=false scroll=0 reorder=None"),
        w(&[Key::Char('j')], "Consumed | Sidebar { panel: Tasks, selected: 2 } idx=2 drill=None exp=false scroll=0 reorder=None"),
        w(&[Key::Char('k')], "Consumed | Sidebar { panel: Tasks, selected: 0 } idx=0 drill=None exp=false scroll=0 reorder=None"),
    ];
    // With no task `Enter` falls through to the Sidebar row: off-state (R3).
    const OFF: &[W] = &[wp(&[Key::Enter], "OpenSidebarConversation | Sidebar { panel: Tasks, selected: 1 } idx=1 drill=None exp=false scroll=0 reorder=None", |s| s.task_panel_state.task_count = 0)];
    run(&Case { off: OFF, ..local(Context::SidebarTasks, tasks_state, tasks_salient, ROWS) });
}

/// Regression caught: in reorder mode `j` / `k` / `Enter` select or drill instead of moving and committing the task.
#[test]
#[rustfmt::skip]
fn tasks_reorder_rows_move_and_commit_the_task() {
    const ROWS: &[W] = &[
        w(&[Key::Char('j')], "TaskReorderMove(Down) | Sidebar { panel: Tasks, selected: 1 } idx=1 drill=None exp=false scroll=0 reorder=Some(2)"),
        w(&[Key::Char('k')], "TaskReorderMove(Up) | Sidebar { panel: Tasks, selected: 1 } idx=1 drill=None exp=false scroll=0 reorder=Some(2)"),
        w(&[Key::Enter], "TaskReorderCommit | Sidebar { panel: Tasks, selected: 1 } idx=1 drill=None exp=false scroll=0 reorder=Some(2)"),
    ];
    // The non-character rows the exclusive reorder mode inherits from the Tasks panel and the Sidebar (R4).
    const INHERITED: &[W] = &[
        w(&[Key::Up], "Consumed | Sidebar { panel: Tasks, selected: 0 } idx=0 drill=None exp=false scroll=0 reorder=Some(2)"),
        w(&[Key::Down], "Consumed | Sidebar { panel: Tasks, selected: 2 } idx=2 drill=None exp=false scroll=0 reorder=Some(2)"),
        w(&[Key::Esc], "Consumed | Chat idx=1 drill=None exp=false scroll=0 reorder=Some(2)"),
        w(&[Key::Tab], "Consumed | Input idx=1 drill=None exp=false scroll=0 reorder=Some(2)"),
        w(&[Key::ShiftTab], "SwitchToPrevTab | Sidebar { panel: Tasks, selected: 1 } idx=1 drill=None exp=false scroll=0 reorder=Some(2)"),
        w(&[Key::Ctrl('t')], "NewTab | Sidebar { panel: Tasks, selected: 1 } idx=1 drill=None exp=false scroll=0 reorder=Some(2)"),
        w(&[Key::Ctrl('p')], "Consumed | Overlay(CommandPalette) idx=1 drill=None exp=false scroll=0 reorder=Some(2)"),
        w(&[Key::Ctrl('c')], "CancelOrQuit | Sidebar { panel: Tasks, selected: 1 } idx=1 drill=None exp=false scroll=0 reorder=Some(2)"),
        w(&[Key::Alt('v')], "RequestClipboardPaste | Sidebar { panel: Tasks, selected: 1 } idx=1 drill=None exp=false scroll=0 reorder=Some(2)"),
    ];
    run(&Case { inherited: INHERITED, ..local(Context::TasksReorder, reorder_tasks_state, tasks_salient, ROWS) });
}

// ── Agents / Room / Artifacts ─────────────────────────────────────────────

/// Regression caught: `p` in the Agents list stops toggling the selected agent's pause.
#[test]
#[rustfmt::skip]
fn sidebar_agents_p_toggles_the_selected_agent() {
    const ROWS: &[W] = &[w(&[Key::Char('p')], "AgentPauseToggle | Sidebar { panel: Agents, selected: 0 } sel=0")];
    // With the inspector open `p` is consumed: it must not toggle an agent (R3).
    const OFF: &[W] = &[wp(&[Key::Char('p')], "Consumed | Sidebar { panel: Agents, selected: 0 } sel=0", |s| s.agent_panel_state.drill_down_agent = AgentId::parse("spoke-1").ok())];
    run(&Case { off: OFF, ..local(Context::SidebarAgents, || panel_state(PanelType::Agents), panel_salient, ROWS) });
}

/// Regression caught: `Enter` in the read-only Room falls through and opens a conversation.
#[test]
#[rustfmt::skip]
fn sidebar_room_enter_does_nothing() {
    const ROWS: &[W] = &[w(&[Key::Enter], "Consumed | Sidebar { panel: Room, selected: 0 } sel=0")];
    run(&local(Context::SidebarRoom, || panel_state(PanelType::Room), panel_salient, ROWS));
}

/// One patch artifact (id `aaaaaa…`) folded into the Artifacts panel.
fn with_artifact(state: &mut TuiState) {
    let hex = |c: &str| ContentHash::parse_hex(&c.repeat(64)).expect("hex");
    let artifact = EvidenceArtifact {
        id: ArtifactId::from(hex("a")),
        kind: ArtifactKind::Patch,
        producer: AgentId::parse("spoke-1").expect("agent id"),
        content_hash: hex("0"),
        authority: CapabilityTokenId::default(),
        provenance: vec![ProvenanceTag::UserOriginated],
        depends_on: vec![],
        review: Some(ReviewStatus::Pending),
        host: HostBinding::new("host-A", "ws"),
    };
    let events = vec![RoomEvent::ArtifactCreated { artifact }];
    let id = OrchestrationRoomId::parse("room-test").expect("room id");
    let room = OrchestrationRoom::project_for_host(id, events, "host-A");
    let (host, mut selected) = ("host-A".to_owned(), 0);
    let policy = MergeBackPolicy::default();
    let panel = &mut state.artifacts_panel;
    panel.apply_read(
        room,
        host,
        1,
        0,
        1,
        policy,
        PermissionMode::Plan,
        &mut selected,
    );
}

/// Regression caught: `Enter` on an artifact stops reaching `/artifact show <prefix>`, or an empty panel falls through to history.
#[test]
#[rustfmt::skip]
fn sidebar_artifacts_enter_shows_the_selected_artifact() {
    const ROWS: &[W] = &[
        w(&[Key::Enter], "Consumed | Sidebar { panel: Artifacts, selected: 0 } sel=0"),
        wp(&[Key::Enter], "ExecuteCommand { name: \"artifact\", args: Some(\"show aaaaaa\") } | Sidebar { panel: Artifacts, selected: 0 } sel=0", with_artifact),
    ];
    run(&local(Context::SidebarArtifacts, || panel_state(PanelType::Artifacts), panel_salient, ROWS));
}

// ── A2A log ───────────────────────────────────────────────────────────────

/// The A2A log panel focused over three journal rows (`seq` 1..=3), first selected.
fn log_state() -> TuiState {
    let row = |seq: u64| TransparencyRow {
        rail: None,
        seq,
        recorded_at_ms: Some(seq as i64),
        retracted_at_ms: None,
        direction: Direction::Inbound,
        kind: TransparencyKind::Rejected,
        peer: "peer-a".to_owned(),
        task: Some(format!("task-{seq}")),
        summary: format!("row {seq}"),
        provenance: None,
        principal_collapsed: false,
    };
    let mut state = panel_state(PanelType::TransparencyLog);
    state
        .transparency_panel
        .apply_read((1..=3).map(row).collect(), 10);
    state.sidebar_entry_count = 3;
    state
}

/// The `/` search already open with the filter `ab`.
fn searching(state: &mut TuiState) {
    state.transparency_panel.search = Some("ab".to_owned());
    state.transparency_panel.search_active = true;
}

fn log_salient(state: &TuiState) -> String {
    let panel = &state.transparency_panel;
    let (q, active, drill) = (&panel.search, panel.search_active, panel.drill_seq);
    format!("{:?} q={q:?}/{active} drill={drill:?}", state.focus)
}

/// Regression caught: an A2A log key edits the filter, selection, drill or exit in the wrong order (`Esc` clears before it leaves).
#[test]
#[rustfmt::skip]
fn sidebar_a2a_log_rows_search_select_drill_export_and_leave() {
    const ROWS: &[W] = &[
        w(&[Key::Char('/')], "Consumed | Sidebar { panel: TransparencyLog, selected: 0 } q=Some(\"\")/true drill=None"),
        wp(&[Key::Backspace], "Consumed | Sidebar { panel: TransparencyLog, selected: 0 } q=Some(\"a\")/true drill=None", searching),
        w(&[Key::Enter], "Consumed | Sidebar { panel: TransparencyLog, selected: 0 } q=None/false drill=Some(1)"),
        wp(&[Key::Enter], "Consumed | Sidebar { panel: TransparencyLog, selected: 0 } q=Some(\"ab\")/false drill=None", searching),
        w(&[Key::Esc], "Consumed | Chat q=None/false drill=None"),
        wp(&[Key::Esc], "Consumed | Sidebar { panel: TransparencyLog, selected: 0 } q=None/false drill=None", searching),
        w(&[Key::Char('G')], "Consumed | Sidebar { panel: TransparencyLog, selected: 2 } q=None/false drill=None"),
        w(&[Key::Char('e')], "ExportTransparency | Sidebar { panel: TransparencyLog, selected: 0 } q=None/false drill=None"),
        w(&[Key::Char('j')], "Consumed | Sidebar { panel: TransparencyLog, selected: 1 } q=None/false drill=None"),
        wp(&[Key::Char('k')], "Consumed | Sidebar { panel: TransparencyLog, selected: 1 } q=None/false drill=None", |s| s.sidebar_selected = 2),
    ];
    // While searching every printable types (`q j k G e /` are not commands there): off-state (R3).
    const OFF: &[W] = &[wp(&[Key::Char('q'), Key::Char('j'), Key::Char('k'), Key::Char('G'), Key::Char('e'), Key::Char('/')], "Consumed | Sidebar { panel: TransparencyLog, selected: 0 } q=Some(\"abqjkge/\")/true drill=None", searching)];
    run(&Case { off: OFF, ..local(Context::SidebarA2aLog, log_state, log_salient, ROWS) });
}

// ── Adapters (no own rows) and DF-262 ─────────────────────────────────────

/// Regression caught: the Adapters panel (Sidebar rows only) loses `j`/`k` clamping or its `Enter` action.
#[test]
fn adapters_panel_rows_act_by_value() {
    let mut state = fresh();
    state.sidebar_entry_count = 3;
    state.sidebar_selected = 1;
    state.focus = FocusState::Sidebar {
        panel: PanelType::Adapters,
        selected: 1,
    };
    for (key, selected) in [('j', 2), ('j', 2), ('k', 1), ('k', 0), ('k', 0)] {
        assert_eq!(
            press(&mut state, Key::Char(key)),
            InputAction::Consumed,
            "{key}"
        );
        let focus = FocusState::Sidebar {
            panel: PanelType::Adapters,
            selected,
        };
        assert_eq!(
            (state.sidebar_selected, state.focus.clone()),
            (selected, focus),
            "{key}"
        );
    }
    assert_eq!(
        press(&mut state, Key::Enter),
        InputAction::OpenSidebarConversation
    );
}

/// DF-262 TODAY: shrinking below 120 columns silently drops an open task drill-down, so `r s e c` stop being task verbs.
#[test]
fn narrow_resize_clears_the_drill_and_falls_back_to_chat() {
    let mut state = TuiState::new(160, 40);
    state.sidebar_visible = true;
    state.sidebar_panel = Some(PanelType::Tasks);
    state.focus = FocusState::Chat;
    state.task_panel_state.drill_down_task = Some(2);
    state.task_panel_state.expanded_detail = true;
    state.task_panel_state.detail_scroll_offset = 7;

    // Control: 120 columns still shows the sidebar, so the drill-down survives.
    let resize =
        |state: &mut TuiState, width| handle_input(state, &DomainInputEvent::Resize(width, 40));
    assert_eq!(resize(&mut state, 120), InputAction::Consumed);
    assert_eq!(state.task_panel_state.drill_down_task, Some(2));
    assert_eq!(press(&mut state, Key::Char('r')), InputAction::TaskRetry(2));

    assert_eq!(resize(&mut state, 119), InputAction::Consumed);
    assert_eq!((state.sidebar_visible, state.sidebar_panel), (false, None));
    let t = &state.task_panel_state;
    assert_eq!(
        (t.drill_down_task, t.expanded_detail, t.detail_scroll_offset),
        (None, false, 0)
    );
    assert_eq!(
        (state.focus.clone(), state.terminal_width),
        (FocusState::Chat, 119)
    );
    for key in ['r', 's', 'e'] {
        assert_eq!(
            press(&mut state, Key::Char(key)),
            InputAction::Ignored,
            "{key}"
        );
    }
    assert_eq!(
        press(&mut state, Key::Char('c')),
        InputAction::CopyToClipboard(String::new())
    );
}
