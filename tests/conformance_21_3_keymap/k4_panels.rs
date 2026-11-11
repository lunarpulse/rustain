//! Story 21.3 conformance leg K4 (panels): the Tasks tab of Mission Control, the task window
//! and the task reorder sub-context (Story 21.7, 7-D), the wave Chat sub-state and the wave
//! overlay, each witnessed BY VALUE through the real local front door
//! (`support::press` -> `app::handle_input`) by the shared `witness::run` harness.

use std::sync::Arc;

use rustain::adapters::tui::app::{InputAction, handle_input};
use rustain::adapters::tui::artifact_window::Live;
use rustain::adapters::tui::keymap::{Context, Key};
use rustain::adapters::tui::mission_control::{self, Fit, tasks};
use rustain::adapters::tui::state::{Reorder, TuiState};
use rustain::adapters::tui::task_window;
use rustain::domain::events::DomainInputEvent;
use rustain::domain::models::Conversation;
use rustain::domain::models::FocusState;

use super::support::{StubWave, fresh, press, reorder_hub, task_ref, task_window_state, tasks_hub};
use super::witness::{Case, W, local, run, w, wp};

// ── restored task contexts ────────────────────────────────────────────────

#[test]
fn inherited_ctrl_p_opens_palette_and_restores_task_contexts() {
    use rustain::adapters::tui::presentation::{self, Surface};
    for (setup, key, action) in [
        (
            tasks_hub as fn() -> TuiState,
            Key::Enter,
            InputAction::OpenTaskWindow(task_ref(2)),
        ),
        (
            window_failed_task as fn() -> TuiState,
            Key::Char('r'),
            InputAction::TaskRetry(task_ref(3)),
        ),
    ] {
        let mut state = setup();
        let before = (hub_salient(&state), window_salient(&state));
        let under = state.modal_stack.top().map(|frame| frame.surface);
        assert_eq!(press(&mut state, Key::Ctrl('p')), InputAction::Consumed);
        assert_eq!(
            state.modal_stack.top().unwrap().surface,
            Surface::CommandPalette
        );
        assert_eq!(press(&mut state, Key::Esc), InputAction::Consumed);
        assert!(!presentation::is_open(&state, Surface::CommandPalette));
        assert_eq!(
            state.modal_stack.top().map(|frame| frame.surface),
            under,
            "the palette closes back onto the task surface"
        );
        assert_eq!((hub_salient(&state), window_salient(&state)), before);
        assert_eq!(
            press(&mut state, key),
            action,
            "restored task context owns its verb"
        );
    }
    // The reorder context has no palette row: `Ctrl+P` is inert there and the reorder survives
    // (it was `Consumed` + restored through the old Tasks-panel reorder; now the mode is exclusive).
    let mut state = reorder_hub();
    let before = hub_salient(&state);
    assert_eq!(press(&mut state, Key::Ctrl('p')), InputAction::Consumed);
    assert_eq!(hub_salient(&state), before);
    assert!(!presentation::is_open(&state, Surface::CommandPalette));
    assert_eq!(
        press(&mut state, Key::Enter),
        InputAction::TaskReorderCommit
    );
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
    rustain::adapters::tui::presentation::reducer::open(
        &mut state,
        rustain::adapters::tui::presentation::Surface::WaveOverlay,
    );
    state.wave_overlay_selected = 1;
    state
}

fn wave_salient(state: &TuiState) -> String {
    let (sel, div) = (state.wave_overlay_selected, state.wave_diverge_open);
    let frame = state.modal_stack.top().map(|frame| frame.surface);
    format!(
        "{:?} frame={frame:?} sel={sel} div={div} leader={}",
        state.focus,
        state.pending_leader.is_some()
    )
}

/// Regression caught: `Esc` / `Enter` on a retained wave dismiss or open the wrong surface (divergence view first).
#[test]
#[rustfmt::skip]
fn chat_wave_rows_dismiss_or_open_the_retained_wave() {
    const ROWS: &[W] = &[
        w(&[Key::Esc], "DismissWave | Chat frame=None sel=0 div=false leader=false"),
        wp(&[Key::Esc], "CloseDivergeView | Chat frame=Some(WaveOverlay) sel=0 div=true leader=false", |s| {
            let action = press(s, Key::Char('d'));
            assert_eq!(action, InputAction::OpenDivergeView);
            assert!(rustain::adapters::tui::handlers::wave_overlay::apply(s, "", &action));
        }),
        w(&[Key::Enter], "OpenWaveOverlay | Chat frame=Some(WaveOverlay) sel=0 div=false leader=false"),
        w(&[Key::Char('d')], "OpenDivergeView | Chat frame=Some(WaveOverlay) sel=0 div=false leader=false"),
        w(&[Key::Char('s')], "CloseDivergeView | Chat frame=None sel=0 div=false leader=false"),
        wp(&[Key::Char('r')], "OpenWaveOverlay | Chat frame=Some(WaveOverlay) sel=0 div=false leader=false", |s| s.wave_overlay_selected = 3),
    ];
    // Without a retained wave the same three keys do nothing (the arms are conditional): off-state (R3).
    const OFF: &[W] = &[
        wp(&[Key::Char('d')], "Ignored | Chat frame=None sel=0 div=false leader=false", |s| s.wave_run = None),
        wp(&[Key::Char('s')], "Ignored | Chat frame=None sel=0 div=false leader=false", |s| s.wave_run = None),
        wp(&[Key::Char('r')], "Ignored | Chat frame=None sel=0 div=false leader=false", |s| s.wave_run = None),
    ];
    run(&Case { off: OFF, ..local(Context::ChatWave, wave_state, wave_salient, ROWS) });
}

/// Regression caught: the overlay selection overruns the spoke count or a verb acts on the wrong spoke.
#[test]
#[rustfmt::skip]
fn wave_overlay_rows_select_open_close_and_act_on_the_selected_spoke() {
    const ROWS: &[W] = &[
        w(&[Key::Char('j')], "Consumed | Chat frame=Some(WaveOverlay) sel=2 div=false leader=false"),
        w(&[Key::Down], "Consumed | Chat frame=Some(WaveOverlay) sel=2 div=false leader=false"),
        w(&[Key::Char('k')], "Consumed | Chat frame=Some(WaveOverlay) sel=0 div=false leader=false"),
        w(&[Key::Up], "Consumed | Chat frame=Some(WaveOverlay) sel=0 div=false leader=false"),
        w(&[Key::Enter], "WaveDrillSpoke(1) | Chat frame=Some(WaveOverlay) sel=1 div=false leader=false"),
        w(&[Key::Esc], "CloseWaveOverlay | Chat frame=Some(WaveOverlay) sel=1 div=false leader=false"),
        w(&[Key::Ctrl('c')], "CloseWaveOverlay | Chat frame=Some(WaveOverlay) sel=1 div=false leader=false"),
        w(&[Key::Char('r')], "WaveRerunSpoke(1) | Chat frame=Some(WaveOverlay) sel=1 div=false leader=false"),
        w(&[Key::Char('d')], "OpenDivergeView | Chat frame=Some(WaveOverlay) sel=1 div=false leader=false"),
        w(&[Key::Char('s')], "CloseDivergeView | Chat frame=Some(WaveOverlay) sel=1 div=false leader=false"),
        w(&[Key::Ctrl('q')], "Quit | Chat frame=Some(WaveOverlay) sel=1 div=false leader=false"),
        w(&[Key::Ctrl('x')], "Consumed | Chat frame=Some(WaveOverlay) sel=1 div=false leader=true"),
        w(&[Key::Ctrl('p')], "Consumed | Chat frame=Some(CommandPalette) sel=1 div=false leader=false"),
    ];
    run(&local(Context::WaveOverlay, overlay_state, wave_salient, ROWS));
}

// ── Mission Control Tasks tab / TaskWindow / MissionReorder ───────────────

/// What a Mission Control frame summarises for a before/after comparison.
fn hub_salient(state: &TuiState) -> String {
    let mission = &state.mission_control;
    format!(
        "{:?} sel={} pane={:?} top={:?} search={} reorder={} leader={}",
        mission.tab,
        mission.selected_id().unwrap_or("-"),
        mission.pane,
        state.modal_stack.top().map(|frame| frame.surface),
        mission_control::search_editing(state),
        state
            .task_panel_state
            .reorder
            .as_ref()
            .map_or(0, |reorder| reorder.number),
        state.pending_leader.is_some()
    )
}

/// Wide paint facts: master and detail side by side.
fn paint_wide(state: &mut TuiState) {
    let mut fit = state.mission_control.fit();
    fit.wide = true;
    state.mission_control.set_fit(fit);
}

/// The `/` filter hint painted whole.
fn paint_search(state: &mut TuiState) {
    let mut fit = state.mission_control.fit();
    fit.search = true;
    state.mission_control.set_fit(fit);
}

/// A paint that offered no row action (nothing selected, or nothing that can act).
fn paint_nothing(state: &mut TuiState) {
    state.mission_control.set_fit(Fit::default());
}

/// The plan left the conversation: the Tasks tab is rebuilt from a conversation with none.
fn plan_gone(state: &mut TuiState) {
    tasks::sync(state, &Conversation::default());
}

/// Regression caught: a Tasks-tab verb (`p s x Enter`) targets the wrong task, plan or conversation,
/// or acts where its hint was not painted; `Left`/`Right`/`Tab`/`Esc` stop being the hub's own.
/// Replaces the deleted Tasks sidebar panel rows (`Up Down x p s Enter j k`): the task is now named by
/// value (a `TaskRef`), never by an index into whatever is active.
#[test]
#[rustfmt::skip]
fn tasks_tab_rows_act_on_the_selected_task_by_ref() {
    const ROWS: &[W] = &[
        w(&[Key::Left], "OpenMissionControl(Agents) | Agents sel=- pane=Master top=Some(MissionControl) search=false reorder=0 leader=false"),
        w(&[Key::Right], "OpenMissionControl(Messages) | Messages sel=- pane=Master top=Some(MissionControl) search=false reorder=0 leader=false"),
        w(&[Key::Tab], "Consumed | Tasks sel=plan-1#2 pane=Master top=Some(MissionControl) search=false reorder=0 leader=false"),
        wp(&[Key::Tab], "Consumed | Tasks sel=plan-1#2 pane=Detail top=Some(MissionControl) search=false reorder=0 leader=false", paint_wide),
        w(&[Key::Enter], r#"OpenTaskWindow(TaskRef { conversation_id: "conv-1", plan_id: "plan-1", number: 2 }) | Tasks sel=plan-1#2 pane=Master top=Some(MissionControl) search=false reorder=0 leader=false"#),
        w(&[Key::Char('e')], "Consumed | Tasks sel=plan-1#2 pane=Master top=Some(MissionControl) search=false reorder=0 leader=false"),
        w(&[Key::Char('p')], r#"TaskPause(TaskRef { conversation_id: "conv-1", plan_id: "plan-1", number: 2 }) | Tasks sel=plan-1#2 pane=Master top=Some(MissionControl) search=false reorder=0 leader=false"#),
        w(&[Key::Char('x')], r#"TaskCancelPlan(TaskRef { conversation_id: "conv-1", plan_id: "plan-1", number: 2 }) | Tasks sel=plan-1#2 pane=Master top=Some(MissionControl) search=false reorder=0 leader=false"#),
        w(&[Key::Char('s')], r#"TaskSkip(TaskRef { conversation_id: "conv-1", plan_id: "plan-1", number: 2 }) | Tasks sel=plan-1#2 pane=Master top=Some(MissionControl) search=false reorder=0 leader=false"#),
        w(&[Key::Char('c')], "Consumed | Tasks sel=plan-1#2 pane=Master top=Some(MissionControl) search=false reorder=0 leader=false"),
        w(&[Key::Char('/')], "Consumed | Tasks sel=plan-1#2 pane=Master top=Some(MissionControl) search=false reorder=0 leader=false"),
        wp(&[Key::Char('/')], "Consumed | Tasks sel=plan-1#2 pane=Master top=Some(MissionControl) search=true reorder=0 leader=false", paint_search),
        w(&[Key::Esc], "Consumed | Tasks sel=plan-1#2 pane=Master top=None search=false reorder=0 leader=false"),
        w(&[Key::Ctrl('c')], "Consumed | Tasks sel=plan-1#2 pane=Master top=None search=false reorder=0 leader=false"),
        w(&[Key::Ctrl('x')], "Consumed | Tasks sel=plan-1#2 pane=Master top=Some(MissionControl) search=false reorder=0 leader=true"),
        w(&[Key::Ctrl('p')], "Consumed | Tasks sel=plan-1#2 pane=Master top=Some(CommandPalette) search=false reorder=0 leader=false"),
        w(&[Key::Ctrl('q')], "Quit | Tasks sel=plan-1#2 pane=Master top=Some(MissionControl) search=false reorder=0 leader=false"),
    ];
    // The RU-25 rows the hub inherits move the selection (the old panel's `Up Down j k`).
    const INHERITED: &[W] = &[
        w(&[Key::Down], "Consumed | Tasks sel=plan-1#3 pane=Master top=Some(MissionControl) search=false reorder=0 leader=false"),
        w(&[Key::Char('j')], "Consumed | Tasks sel=plan-1#3 pane=Master top=Some(MissionControl) search=false reorder=0 leader=false"),
        w(&[Key::Up], "Consumed | Tasks sel=plan-1#1 pane=Master top=Some(MissionControl) search=false reorder=0 leader=false"),
        w(&[Key::Char('k')], "Consumed | Tasks sel=plan-1#1 pane=Master top=Some(MissionControl) search=false reorder=0 leader=false"),
        w(&[Key::Char('g')], "Consumed | Tasks sel=plan-1#1 pane=Master top=Some(MissionControl) search=false reorder=0 leader=false"),
        w(&[Key::Char('G')], "Consumed | Tasks sel=plan-1#3 pane=Master top=Some(MissionControl) search=false reorder=0 leader=false"),
    ];
    // A verb whose hint was not painted, or with no task to name, is inert (off-state, R3); it
    // never falls through to the composer or a card beneath (replaces the sidebar's `Enter`
    // fallthrough to `OpenSidebarConversation` when there was no task).
    const OFF: &[W] = &[
        wp(&[Key::Enter], "Consumed | Tasks sel=plan-1#2 pane=Master top=Some(MissionControl) search=false reorder=0 leader=false", paint_nothing),
        wp(&[Key::Char('p')], "Consumed | Tasks sel=plan-1#2 pane=Master top=Some(MissionControl) search=false reorder=0 leader=false", paint_nothing),
        wp(&[Key::Char('x')], "Consumed | Tasks sel=plan-1#2 pane=Master top=Some(MissionControl) search=false reorder=0 leader=false", paint_nothing),
        wp(&[Key::Char('s')], "Consumed | Tasks sel=plan-1#2 pane=Master top=Some(MissionControl) search=false reorder=0 leader=false", paint_nothing),
        wp(&[Key::Enter], "Consumed | Tasks sel=- pane=Master top=Some(MissionControl) search=false reorder=0 leader=false", plan_gone),
        wp(&[Key::Char('p')], "Consumed | Tasks sel=- pane=Master top=Some(MissionControl) search=false reorder=0 leader=false", plan_gone),
        wp(&[Key::Char('x')], "Consumed | Tasks sel=- pane=Master top=Some(MissionControl) search=false reorder=0 leader=false", plan_gone),
        wp(&[Key::Char('s')], "Consumed | Tasks sel=- pane=Master top=Some(MissionControl) search=false reorder=0 leader=false", plan_gone),
    ];
    run(&Case { off: OFF, inherited: INHERITED, ..local(Context::MissionControl, tasks_hub, hub_salient, ROWS) });
}

/// What the task window summarises.
fn window_salient(state: &TuiState) -> String {
    format!(
        "top={:?} search={} leader={}",
        state.modal_stack.top().map(|frame| frame.surface),
        task_window::search_editing(state),
        state.pending_leader.is_some()
    )
}

fn window_nothing_painted(state: &mut TuiState) {
    state
        .task_window
        .as_ref()
        .expect("the window is open")
        .text
        .set_live(Live::default());
}

fn window_failed_task() -> TuiState {
    task_window_state(3)
}

/// Regression caught: a task-window verb (`p s r e c`) acts on the wrong task, or acts where its
/// hint was not painted (retrying a task that cannot retry). Replaces the deleted drill-down rows
/// (`r s e p c`, scroll, `Esc`): the window names its task by a `TaskRef` taken from itself.
#[test]
#[rustfmt::skip]
fn task_window_rows_act_on_the_windowed_task_by_ref() {
    const ROWS: &[W] = &[
        w(&[Key::Char('p')], r#"TaskPause(TaskRef { conversation_id: "conv-1", plan_id: "plan-1", number: 3 }) | top=Some(TaskWindow) search=false leader=false"#),
        w(&[Key::Char('s')], r#"TaskSkip(TaskRef { conversation_id: "conv-1", plan_id: "plan-1", number: 3 }) | top=Some(TaskWindow) search=false leader=false"#),
        w(&[Key::Char('r')], r#"TaskRetry(TaskRef { conversation_id: "conv-1", plan_id: "plan-1", number: 3 }) | top=Some(TaskWindow) search=false leader=false"#),
        w(&[Key::Char('e')], r#"TaskEdit(TaskRef { conversation_id: "conv-1", plan_id: "plan-1", number: 3 }) | top=Some(TaskWindow) search=false leader=false"#),
        w(&[Key::Char('c')], "Consumed | top=Some(TaskWindow) search=false leader=false"),
        w(&[Key::Char('/')], "Consumed | top=Some(TaskWindow) search=true leader=false"),
        w(&[Key::Esc], "Consumed | top=None search=false leader=false"),
        w(&[Key::Ctrl('c')], "Consumed | top=None search=false leader=false"),
        w(&[Key::Ctrl('x')], "Consumed | top=Some(TaskWindow) search=false leader=true"),
        w(&[Key::Ctrl('p')], "Consumed | top=Some(CommandPalette) search=false leader=false"),
        w(&[Key::Ctrl('q')], "Quit | top=Some(TaskWindow) search=false leader=false"),
    ];
    // A verb whose hint the last paint did not offer whole is inert (R3).
    const OFF: &[W] = &[
        wp(&[Key::Char('p')], "Consumed | top=Some(TaskWindow) search=false leader=false", window_nothing_painted),
        wp(&[Key::Char('s')], "Consumed | top=Some(TaskWindow) search=false leader=false", window_nothing_painted),
        wp(&[Key::Char('r')], "Consumed | top=Some(TaskWindow) search=false leader=false", window_nothing_painted),
        wp(&[Key::Char('e')], "Consumed | top=Some(TaskWindow) search=false leader=false", window_nothing_painted),
        wp(&[Key::Char('/')], "Consumed | top=Some(TaskWindow) search=false leader=false", window_nothing_painted),
    ];
    run(&Case { off: OFF, ..local(Context::TaskWindow, window_failed_task, window_salient, ROWS) });
}

/// Regression caught: in reorder mode `j` / `k` / `Enter` select or open instead of moving and
/// committing the task, `Esc` / `Ctrl+C` stop cancelling it, or any other key escapes the mode.
/// Replaces the deleted reorder rows (`Esc` in Chat reorder; `j k Enter` in the Tasks panel
/// reorder): the reorder is exclusive to the Tasks tab, so nothing else (`p s x Tab Left`) acts.
#[test]
#[rustfmt::skip]
fn mission_reorder_rows_move_commit_and_cancel_the_task() {
    const ROWS: &[W] = &[
        w(&[Key::Char('j')], "TaskReorderMove(Down) | Tasks sel=plan-1#2 pane=Master top=Some(MissionControl) search=false reorder=2 leader=false"),
        w(&[Key::Down], "TaskReorderMove(Down) | Tasks sel=plan-1#2 pane=Master top=Some(MissionControl) search=false reorder=2 leader=false"),
        w(&[Key::Char('k')], "TaskReorderMove(Up) | Tasks sel=plan-1#2 pane=Master top=Some(MissionControl) search=false reorder=2 leader=false"),
        w(&[Key::Up], "TaskReorderMove(Up) | Tasks sel=plan-1#2 pane=Master top=Some(MissionControl) search=false reorder=2 leader=false"),
        w(&[Key::Enter], "TaskReorderCommit | Tasks sel=plan-1#2 pane=Master top=Some(MissionControl) search=false reorder=2 leader=false"),
        w(&[Key::Esc], "TaskReorderCancel | Tasks sel=plan-1#2 pane=Master top=Some(MissionControl) search=false reorder=2 leader=false"),
        w(&[Key::Ctrl('c')], "TaskReorderCancel | Tasks sel=plan-1#2 pane=Master top=Some(MissionControl) search=false reorder=2 leader=false"),
        w(&[Key::Ctrl('q')], "Quit | Tasks sel=plan-1#2 pane=Master top=Some(MissionControl) search=false reorder=2 leader=false"),
    ];
    run(&local(Context::MissionReorder, reorder_hub, hub_salient, ROWS));
}

/// DF-21-3-REORDER-WAVE-PRECEDENCE: a retained wave's `Esc d s r Enter` in Chat never depend on a
/// reorder record (the old Chat reorder sub-context took `Esc` before the wave did): with a
/// reorder armed the wave rows answer exactly as without it.
#[test]
fn retained_wave_rows_in_chat_ignore_an_armed_reorder() {
    for (key, expect) in [
        (Key::Esc, InputAction::DismissWave),
        (Key::Enter, InputAction::OpenWaveOverlay),
        (Key::Char('d'), InputAction::OpenDivergeView),
        (Key::Char('s'), InputAction::CloseDivergeView),
        (Key::Char('r'), InputAction::OpenWaveOverlay),
    ] {
        let mut plain = wave_state();
        assert_eq!(press(&mut plain, key), expect, "{key:?} without a reorder");
        let mut armed = wave_state();
        armed.task_panel_state.reorder = Some(Reorder {
            conversation_id: "conv-1".to_owned(),
            plan_id: "plan-1".to_owned(),
            number: 2,
            original_order: vec![1, 2, 3],
        });
        assert_eq!(
            press(&mut armed, key),
            expect,
            "{key:?} with a reorder armed"
        );
        assert_eq!(
            armed.task_panel_state.reorder.as_ref().map(|r| r.number),
            Some(2),
            "{key:?}: Chat never cancels the reorder"
        );
    }
}

/// DF-262 / A16: shrinking below 120 columns keeps the monitor preference (the column is simply
/// not painted; the P11 reset is gone); Chat has no task verb at any width. Replaces the old
/// drill-down fallback (an open drill-down survived 120 and was dropped at 119, so `r s e c`
/// stopped being task verbs): there is no drill-down, so `r s e` are Chat keys with nothing to
/// do both before and after the resize, and `c` always copies.
#[test]
fn narrow_resize_keeps_the_monitor_preference_and_chat_has_no_task_verbs() {
    let mut state = TuiState::new(160, 40);
    state.monitor = true;
    state.focus = FocusState::Chat;
    state.task_panel_state.last_executed_plan_id = Some("plan-1".to_owned());

    // Control: 120 columns still shows the sidebar.
    let resize =
        |state: &mut TuiState, width| handle_input(state, &DomainInputEvent::Resize(width, 40));
    assert_eq!(resize(&mut state, 120), InputAction::Consumed);
    assert!(state.monitor);
    assert_eq!(
        press(&mut state, Key::Char('r')),
        InputAction::Ignored,
        "a plan in flight gives Chat no task verb"
    );

    assert_eq!(resize(&mut state, 119), InputAction::Consumed);
    assert!(state.monitor, "the preference is kept below 120 (A16)");
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
