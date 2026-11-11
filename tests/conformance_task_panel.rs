//! Conformance tests for Story 6-3: Task Panel & Progress Monitoring, as re-homed by Story 21.7.
//!
//! The sidebar Tasks panel and the chat-pane drill-down are gone (A17/A19, OD-2): the plan's
//! tasks are the Tasks tab of Mission Control (`tasks::list` / `tasks::sync` rows, one row per
//! task and per sub-task: height == paint), one task is the task window (`task_window::open`,
//! keys through `handle_input`), and a plan that starts opens nothing. This file validates
//! those surfaces, the event-arm helpers (`task_panel_handlers`) and the copy payloads.
//!
//! Behaviour deleted with the sidebar (auto-open, its config switch and one-time hint, the
//! user-suppression toggle, the narrow-terminal gate, drill-down expand/scroll, the sidebar
//! panel focus) is pinned absent by the cutover scanner (K12(d)); each deleted test is named
//! beside its replacement in the comments below.

use std::collections::HashMap;

use rustain::adapters::tui::app::{InputAction, handle_input};
use rustain::adapters::tui::artifact_window::Live;
use rustain::adapters::tui::mission_control::tasks::{self, TaskRef};
use rustain::adapters::tui::mission_control::{self, Fit, McTab};
use rustain::adapters::tui::presentation::markdown_body::BodyMode;
use rustain::adapters::tui::presentation::{Surface, reducer};
use rustain::adapters::tui::state::{TaskPanelState, TuiState};
use rustain::adapters::tui::task_panel_handlers::{
    any_plan_has_running_task, handle_plan_execution_started, handle_plan_task_status_changed,
    resolve_copy_task_payload,
};
use rustain::adapters::tui::task_window;
use rustain::adapters::tui::widgets::task_panel::{
    can_retry, can_skip, pause_label, resolve_panel_plan, status_glyph,
};
use rustain::domain::events::{DomainInputEvent, DomainKey};
use rustain::domain::models::plan::{PlanSubTask, TaskResult};
use rustain::domain::models::{
    Conversation, FocusState, Plan, PlanStatus, PlanTask, PlanTaskStatus, generate_conversation_id,
};

fn make_task(number: u32, title: &str, status: PlanTaskStatus) -> PlanTask {
    PlanTask {
        number,
        title: title.to_string(),
        description: String::new(),
        depends_on: vec![],
        status,
        started_at_ms: None,
        completed_at_ms: None,
        result: None,
        error: None,
        waiting_on: vec![],
        delegated_to: None,
        sub_tasks: vec![],
    }
}

fn make_sub_task(number: u32, title: &str, status: PlanTaskStatus) -> PlanSubTask {
    PlanSubTask {
        number,
        title: title.to_string(),
        description: String::new(),
        status,
        started_at_ms: None,
        completed_at_ms: None,
        result: None,
        error: None,
        delegated_to: None,
    }
}

fn make_plan(status: PlanStatus, tasks: Vec<PlanTask>) -> Plan {
    Plan {
        id: "test-plan".to_string(),
        title: "Test Plan".to_string(),
        tasks,
        estimated_effort: None,
        status,
        created_at: 1_700_000_000,
        resolved_at: None,
        host_message_id: None,
    }
}

fn empty_conv() -> Conversation {
    Conversation {
        id: generate_conversation_id(),
        title: "Test".to_string(),
        messages: vec![],
        turns: Vec::new(),
        created_at: 1_700_000_000,
        updated_at: 1_700_000_000,
        last_response_at: None,
        session_id: None,
        usage: None,
        plans: HashMap::new(),
        fork_source: None,
        compaction: None,
        peer_row_controls: Default::default(),
        peer_row_revision: 0,
    }
}

fn make_conv_with_plan(plan: Plan) -> Conversation {
    let plan_id = plan.id.clone();
    let mut conv = empty_conv();
    conv.plans.insert(plan_id, plan);
    conv
}

/// A state with Mission Control open on the Tasks tab, its rows built from `conv` (what the
/// host does before it paints), and the rail offering `Enter Open` (a painted fit).
fn hub_on_tasks(conv: &Conversation) -> TuiState {
    let mut state = TuiState::new(160, 24);
    reducer::open_mission_control(&mut state, McTab::Tasks);
    tasks::sync(&mut state, conv);
    state.mission_control.set_fit(Fit {
        open: true,
        ..Fit::default()
    });
    state
}

fn press(state: &mut TuiState, key: DomainKey) -> InputAction {
    handle_input(state, &DomainInputEvent::SpecialKey(key))
}

fn task_ref(conv: &Conversation, number: u32) -> TaskRef {
    TaskRef {
        conversation_id: conv.id.clone(),
        plan_id: "test-plan".to_string(),
        number,
    }
}

#[test]
fn ac3_task_panel_state_default() {
    // old: selected_index/auto_open_skipped_for_plan/drill_down_task/task_count == default.
    // Those fields are deleted with the sidebar; the state that remains defaults empty.
    let state = TaskPanelState::default();
    assert_eq!(state.last_executed_plan_id, None);
    assert!(state.reorder.is_none());
}

#[test]
fn ac4_rows_show_status_icons() {
    // old: render_task_panel painted every title; now one list row per task carries the glyph
    // of its status and its title, and is exactly one line tall (height == paint).
    let plan = make_plan(
        PlanStatus::Executing,
        vec![
            make_task(1, "Task one", PlanTaskStatus::Completed),
            make_task(2, "Task two", PlanTaskStatus::Running),
            make_task(3, "Task three", PlanTaskStatus::Pending),
            make_task(4, "Task four", PlanTaskStatus::Failed),
        ],
    );
    let conv = make_conv_with_plan(plan.clone());
    let built = tasks::list(&conv, None, None);
    assert_eq!(built.rows.len(), 4, "one row per task, nothing hidden");
    for (row, task) in built.rows.iter().zip(&plan.tasks) {
        assert!(row.text.contains(&task.title), "{}", row.text);
        assert!(row.text.contains(status_glyph(task.status)), "{}", row.text);
        assert!(!row.text.contains('\n'), "a row is one line: {}", row.text);
        assert_eq!(row.id, tasks::task_id("test-plan", task.number));
    }
    // The same rows reach the hub through sync.
    let state = hub_on_tasks(&conv);
    assert_eq!(state.mission_control.rows().len(), 4);
}

#[test]
fn ac4_render_empty_state() {
    let conv = empty_conv();
    let built = tasks::list(&conv, None, None);
    assert!(built.rows.is_empty());
    assert!(
        built.empty.as_deref().unwrap().contains("No active plan"),
        "{:?}",
        built.empty
    );
    let state = hub_on_tasks(&conv);
    assert!(state.mission_control.list().unwrap().empty.is_some());
}

#[test]
fn ac4_render_completed_plan_fallback() {
    // old: "(last)" and "Plan complete" in the panel; now the note says the plan is complete
    // and that a new plan refreshes the tab.
    let plan = make_plan(
        PlanStatus::Completed,
        vec![make_task(1, "Done task", PlanTaskStatus::Completed)],
    );
    let conv = make_conv_with_plan(plan);
    let built = tasks::list(&conv, None, None);
    assert_eq!(built.rows.len(), 1);
    let note = built.note.expect("a plan has a note");
    assert!(note.contains("complete"), "{note}");
    assert!(note.contains("open a new plan to refresh"), "{note}");
}

#[test]
fn ac5_sub_task_rows_sit_beneath_their_parent() {
    // old: a source-text check for a "10.6" forward-compat comment in task_panel.rs. The seat
    // is now real: each sub-task is one more row directly beneath its parent, indented, and
    // never a hidden line inside the parent's row (height == paint).
    let mut parent = make_task(1, "Build", PlanTaskStatus::Running);
    parent.sub_tasks = vec![
        make_sub_task(1, "compile", PlanTaskStatus::Completed),
        make_sub_task(2, "link", PlanTaskStatus::Running),
    ];
    let plan = make_plan(
        PlanStatus::Executing,
        vec![parent, make_task(2, "Test", PlanTaskStatus::Pending)],
    );
    let conv = make_conv_with_plan(plan);
    let built = tasks::list(&conv, None, None);
    let ids: Vec<&str> = built.rows.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "test-plan#1",
            "test-plan#1.1",
            "test-plan#1.2",
            "test-plan#2"
        ]
    );
    assert!(built.rows[0].text.contains("(1/2 sub-tasks)"));
    assert!(built.rows[1].text.starts_with("    1. "), "indented");
    assert!(built.rows.iter().all(|row| !row.text.contains('\n')));
    // A sub-task row resolves to its parent task.
    assert_eq!(
        built.source.status_of("test-plan#1.2"),
        Some(PlanTaskStatus::Running)
    );
}

#[test]
fn ac9_most_recent_plan_fallback() {
    let mut conv = empty_conv();
    let mut p1 = make_plan(
        PlanStatus::Cancelled,
        vec![make_task(1, "Cancelled", PlanTaskStatus::Cancelled)],
    );
    p1.id = "p1".to_string();
    p1.tasks[0].completed_at_ms = Some(1000);
    let mut p2 = make_plan(
        PlanStatus::Completed,
        vec![make_task(1, "Completed", PlanTaskStatus::Completed)],
    );
    p2.id = "p2".to_string();
    p2.tasks[0].completed_at_ms = Some(2000);
    conv.plans.insert("p1".to_string(), p1);
    conv.plans.insert("p2".to_string(), p2);
    let result = resolve_panel_plan(&conv, None);
    assert!(result.is_some());
    assert_eq!(result.unwrap().tasks[0].title, "Completed");
    // The tab shows the same plan.
    assert!(
        tasks::list(&conv, None, None).rows[0]
            .text
            .contains("Completed")
    );
}

// ac10_resize_clears_drill_down (old): simulated the narrow resize clearing `drill_down_task`
// and the sidebar. Both the drill-down and the Tasks sidebar are deleted (A17/A19), pinned by
// K12(d); the task window that replaces the drill-down is stage-bound and follows its task
// (pinned in src/adapters/tui/task_window.rs tests and the focus/stack reducer tests).
#[test]
fn ac10_plan_start_leaves_the_sidebar_closed_at_any_width() {
    for width in [80u16, 119, 120, 160] {
        let conv = make_conv_with_plan(make_plan(
            PlanStatus::Executing,
            vec![make_task(1, "T", PlanTaskStatus::Running)],
        ));
        let mut state = TuiState::new(width, 24);
        assert!(handle_plan_execution_started(
            &mut state,
            &conv,
            &conv.id,
            "test-plan"
        ));
        assert!(!state.monitor, "width {width}");
        assert!(state.history_window.is_none(), "width {width}");
    }
}

#[test]
fn resolve_panel_plan_prefers_last_id() {
    let mut conv = empty_conv();
    let p1 = make_plan(
        PlanStatus::Completed,
        vec![make_task(1, "A", PlanTaskStatus::Completed)],
    );
    let p2 = make_plan(
        PlanStatus::Executing,
        vec![make_task(1, "B", PlanTaskStatus::Running)],
    );
    conv.plans.insert("p1".to_string(), p1);
    conv.plans.insert("p2".to_string(), p2);
    let result = resolve_panel_plan(&conv, Some("p1"));
    assert!(result.is_some());
    assert_eq!(result.unwrap().tasks[0].title, "A");
}

#[test]
fn resolve_panel_plan_fallback_executing() {
    let mut conv = empty_conv();
    let p2 = make_plan(
        PlanStatus::Executing,
        vec![make_task(1, "B", PlanTaskStatus::Running)],
    );
    conv.plans.insert("p2".to_string(), p2);
    let result = resolve_panel_plan(&conv, Some("nonexistent"));
    assert!(result.is_some());
    assert_eq!(result.unwrap().tasks[0].title, "B");
}

#[test]
fn resolve_panel_plan_returns_none_when_empty() {
    let conv = empty_conv();
    let result = resolve_panel_plan(&conv, None);
    assert!(result.is_none());
}

// ─────────────────────────────────────────────────────────────────────────────
// PD5 — AC1/AC2/AC6/AC7/AC8 conformance via task_panel_handlers
// ─────────────────────────────────────────────────────────────────────────────

// ac1_chord_opens_panel_at_wide_terminal (old): handle_open_panel_tasks opened the sidebar.
// Now `Ctrl+X t` opens Mission Control on the Tasks tab, at any width.
#[test]
fn ac1_chord_opens_tasks_tab() {
    let mut state = TuiState::new(160, 24);
    state.focus = FocusState::Chat;
    assert_eq!(press(&mut state, DomainKey::CtrlX), InputAction::Consumed);
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('t'));
    assert_eq!(action, InputAction::OpenMissionControl(McTab::Tasks));
    assert!(mission_control::is_top(&state));
    assert_eq!(state.mission_control.tab, McTab::Tasks);
    assert!(!state.monitor, "no sidebar opens any more");
}

// ac1_chord_warns_at_narrow_terminal (old): the sidebar needed >= 120 columns. The tab is a
// frame, not a sidebar: the narrow gate is deleted (A17), so the chord works at 80 columns.
#[test]
fn ac1_chord_opens_tasks_tab_at_a_narrow_terminal() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    press(&mut state, DomainKey::CtrlX);
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('t'));
    assert_eq!(action, InputAction::OpenMissionControl(McTab::Tasks));
    assert!(mission_control::is_top(&state));
    assert!(
        state
            .notifications
            .records()
            .all(|n| !n.message.contains(">= 120")),
        "no width warning"
    );
}

// ac1_chord_toggle_close_records_suppression (old): the second chord closed the sidebar and
// recorded a per-conversation auto-open suppression. Both the toggle and the suppression set
// are deleted (nothing auto-opens, so nothing is suppressed): Esc closes the tab like any
// frame, and a plan that starts afterwards still opens nothing.
#[test]
fn ac1_esc_closes_the_tab_and_a_later_plan_start_opens_nothing() {
    let conv = make_conv_with_plan(make_plan(
        PlanStatus::Executing,
        vec![make_task(1, "T", PlanTaskStatus::Pending)],
    ));
    let mut state = hub_on_tasks(&conv);
    assert!(mission_control::is_top(&state));
    press(&mut state, DomainKey::Esc);
    assert!(!mission_control::is_top(&state));
    assert!(state.modal_stack.top().is_none());
    assert!(handle_plan_execution_started(
        &mut state,
        &conv,
        &conv.id,
        "test-plan"
    ));
    assert!(
        state.modal_stack.top().is_none(),
        "a plan start opens no frame"
    );
    assert!(!state.monitor);
}

#[test]
fn ac1_plan_execution_started_opens_nothing_and_follows_the_plan() {
    // old: ac1_auto_open_on_plan_execution_started asserted the sidebar auto-opened. Behaviour
    // deleted by A19 (OD-2); the surviving part is that the started plan is the one the Tasks
    // tab resolves first.
    let conv = make_conv_with_plan(make_plan(
        PlanStatus::Executing,
        vec![
            make_task(1, "T1", PlanTaskStatus::Running),
            make_task(2, "T2", PlanTaskStatus::Pending),
        ],
    ));
    let mut state = TuiState::new(160, 24);
    let focus_before = state.focus.clone();
    assert!(handle_plan_execution_started(
        &mut state,
        &conv,
        &conv.id,
        "test-plan"
    ));
    assert!(!state.monitor);
    assert!(state.history_window.is_none(), "no History frame payload");
    assert!(state.modal_stack.top().is_none(), "no frame opens");
    assert_eq!(state.focus, focus_before, "focus does not move");
    assert_eq!(
        state.task_panel_state.last_executed_plan_id.as_deref(),
        Some("test-plan")
    );
}

// ac1_auto_open_suppressed_by_config (old): `on_task_plan = "none"` suppressed the auto-open.
// With no auto-open (and the `[layout.auto_panels]` key retired, RU-21-7-7) there is nothing for
// the setting to suppress: an old config still loads whatever it says, and a plan start opens
// nothing regardless (same assertion as above, for the old valid and invalid values).
#[test]
fn ac1_plan_start_opens_nothing_whatever_an_old_config_says() {
    use rustain::domain::models::AppConfig;
    for value in ["tasks", "none", "history"] {
        let old = format!("[layout.auto_panels]\non_task_plan = \"{value}\"\n");
        toml::from_str::<AppConfig>(&old).expect("the retired key is ignored, not an error");
        let conv = make_conv_with_plan(make_plan(
            PlanStatus::Executing,
            vec![make_task(1, "T", PlanTaskStatus::Running)],
        ));
        let mut state = TuiState::new(160, 24);
        handle_plan_execution_started(&mut state, &conv, &conv.id, "test-plan");
        assert!(!state.monitor, "{value}");
        assert!(state.modal_stack.top().is_none(), "{value}");
    }
}

// ac1_auto_open_honors_user_suppression_with_one_time_hint (old): a closed panel suppressed
// the next auto-open and posted a one-time "hidden for this session" hint. Behaviour deleted
// by A19 (no auto-open, no suppression set, no hint); pinned by K12(d). The replacement here:
// two consecutive plan starts in one conversation post no notice and open nothing.
#[test]
fn ac1_repeated_plan_starts_post_no_hint_and_open_nothing() {
    let conv = make_conv_with_plan(make_plan(
        PlanStatus::Executing,
        vec![make_task(1, "T", PlanTaskStatus::Pending)],
    ));
    let mut state = TuiState::new(160, 24);
    let before = state.notifications.len();
    for _ in 0..2 {
        handle_plan_execution_started(&mut state, &conv, &conv.id, "test-plan");
    }
    assert_eq!(state.notifications.len(), before, "no hint toast");
    assert!(!state.monitor);
    assert!(state.modal_stack.top().is_none());
}

#[test]
fn ac2_status_change_triggers_redraw_when_tasks_tab_open() {
    let conv = make_conv_with_plan(make_plan(
        PlanStatus::Executing,
        vec![make_task(1, "T", PlanTaskStatus::Running)],
    ));
    let mut state = hub_on_tasks(&conv);
    state.needs_redraw = false;
    let bumped = handle_plan_task_status_changed(&mut state, &conv, &conv.id, "test-plan");
    assert!(bumped);
    assert!(state.needs_redraw);
    assert_eq!(
        state.task_panel_state.last_executed_plan_id.as_deref(),
        Some("test-plan")
    );
}

#[test]
fn ac2_status_change_with_no_tasks_surface_open_does_not_repaint() {
    // The sidebar is gone: with no Tasks surface open the event is matched (the plan the tab
    // resolves first follows it) but nothing needs repainting.
    let conv = make_conv_with_plan(make_plan(
        PlanStatus::Executing,
        vec![make_task(1, "T", PlanTaskStatus::Running)],
    ));
    let mut state = TuiState::new(160, 24);
    state.needs_redraw = false;
    let bumped = handle_plan_task_status_changed(&mut state, &conv, &conv.id, "test-plan");
    assert!(bumped);
    assert!(!state.needs_redraw);
    assert_eq!(
        state.task_panel_state.last_executed_plan_id.as_deref(),
        Some("test-plan")
    );
}

#[test]
fn ac2_status_change_other_conversation_ignored() {
    let conv = make_conv_with_plan(make_plan(
        PlanStatus::Executing,
        vec![make_task(1, "T", PlanTaskStatus::Running)],
    ));
    let mut state = hub_on_tasks(&conv);
    state.needs_redraw = false;
    let bumped = handle_plan_task_status_changed(
        &mut state,
        &conv,
        "different-conversation-id",
        "test-plan",
    );
    assert!(!bumped);
    assert!(!state.needs_redraw);
    assert!(state.task_panel_state.last_executed_plan_id.is_none());
}

#[test]
fn ac2_status_change_unknown_plan_ignored() {
    let conv = make_conv_with_plan(make_plan(
        PlanStatus::Executing,
        vec![make_task(1, "T", PlanTaskStatus::Running)],
    ));
    let mut state = hub_on_tasks(&conv);
    let bumped = handle_plan_task_status_changed(&mut state, &conv, &conv.id, "phantom-plan");
    assert!(!bumped);
    assert!(state.task_panel_state.last_executed_plan_id.is_none());
}

// ac6_drill_down_state_round_trip (old): Enter set `drill_down_task`, Esc cleared it. Now
// Enter on a Tasks row returns OpenTaskWindow, `task_window::open` opens the window, and Esc
// backs to the hub beneath.
#[test]
fn ac6_task_window_open_and_back_round_trip() {
    let conv = make_conv_with_plan(make_plan(
        PlanStatus::Executing,
        vec![
            make_task(1, "T1", PlanTaskStatus::Completed),
            make_task(2, "T2", PlanTaskStatus::Pending),
            make_task(3, "T3", PlanTaskStatus::Pending),
        ],
    ));
    let mut state = hub_on_tasks(&conv);
    press(&mut state, DomainKey::Down);
    let action = press(&mut state, DomainKey::Enter);
    let InputAction::OpenTaskWindow(task) = action else {
        panic!("Enter on a task row opens its window, got {action:?}");
    };
    assert_eq!(task, task_ref(&conv, 2));
    assert!(task_window::open(&mut state, &conv, &task));
    assert!(task_window::is_top(&state));
    assert_eq!(
        state.modal_stack.top().map(|frame| frame.surface),
        Some(Surface::TaskWindow)
    );
    assert_eq!(state.task_window.as_ref().unwrap().number, 2);
    press(&mut state, DomainKey::Esc);
    assert!(!task_window::is_top(&state));
    assert!(mission_control::is_top(&state), "back to the hub beneath");
}

#[test]
fn ac6_task_window_open_refuses_another_conversation_and_a_missing_task() {
    let conv = make_conv_with_plan(make_plan(
        PlanStatus::Executing,
        vec![make_task(1, "T1", PlanTaskStatus::Pending)],
    ));
    let mut state = hub_on_tasks(&conv);
    let mut elsewhere = task_ref(&conv, 1);
    elsewhere.conversation_id = "another-conversation".to_string();
    assert!(!task_window::open(&mut state, &conv, &elsewhere));
    assert!(!task_window::open(&mut state, &conv, &task_ref(&conv, 9)));
    assert!(state.task_window.is_none());
    assert!(!task_window::is_top(&state));
}

// ac7_reserved_keys_only_emit_on_failed_drill_down (old): `drill_down_task_status` decided
// which reserved keys applied (retry/edit only on a Failed task). Now the status rules are the
// pure `can_*` / `pause_label` functions the rail offers from, and the window emits an action
// only for a key whose hint the last paint offered whole (`Live`).
#[test]
fn ac7_status_rules_offer_retry_and_edit_only_on_a_failed_task() {
    use PlanTaskStatus::*;
    for status in [
        Pending, Running, Waiting, Completed, Skipped, Cancelled, Paused,
    ] {
        assert!(!can_retry(status), "{status:?}");
    }
    assert!(can_retry(Failed));
    assert!(can_skip(Pending) && can_skip(Failed));
    assert!(!can_skip(Completed) && !can_skip(Running) && !can_skip(Paused));
    assert_eq!(pause_label(Pending), Some("Pause"));
    assert_eq!(pause_label(Running), Some("Pause"));
    assert_eq!(pause_label(Paused), Some("Resume"));
    assert_eq!(pause_label(Completed), None);
    assert_eq!(pause_label(Failed), None);
}

#[test]
fn ac7_task_window_keys_emit_actions_only_where_the_rail_offered_them() {
    let mut conv = make_conv_with_plan(make_plan(
        PlanStatus::Completed,
        vec![
            make_task(1, "Done", PlanTaskStatus::Completed),
            make_task(2, "Bombed", PlanTaskStatus::Failed),
        ],
    ));
    conv.plans.get_mut("test-plan").unwrap().tasks[1].error = Some("fail".to_string());
    let mut state = hub_on_tasks(&conv);
    assert!(task_window::open(&mut state, &conv, &task_ref(&conv, 2)));
    // Nothing painted yet: every action key is consumed and does nothing.
    for key in ['r', 'e', 's', 'p'] {
        assert_eq!(
            handle_input(&mut state, &DomainInputEvent::KeyPress(key)),
            InputAction::Consumed,
            "{key} is inert before its hint is painted"
        );
    }
    // The failed task's rail offers pause? no (None), but retry / edit / skip yes.
    let live = Live {
        retry: true,
        edit: true,
        skip: true,
        ..Live::default()
    };
    state.task_window.as_ref().unwrap().text.set_live(live);
    let target = task_ref(&conv, 2);
    assert_eq!(
        handle_input(&mut state, &DomainInputEvent::KeyPress('r')),
        InputAction::TaskRetry(target.clone())
    );
    assert_eq!(
        handle_input(&mut state, &DomainInputEvent::KeyPress('e')),
        InputAction::TaskEdit(target.clone())
    );
    assert_eq!(
        handle_input(&mut state, &DomainInputEvent::KeyPress('s')),
        InputAction::TaskSkip(target.clone())
    );
    // `p` stays inert: its hint was not offered.
    assert_eq!(
        handle_input(&mut state, &DomainInputEvent::KeyPress('p')),
        InputAction::Consumed
    );
    state.task_window.as_ref().unwrap().text.set_live(Live {
        pause: true,
        ..Live::default()
    });
    assert_eq!(
        handle_input(&mut state, &DomainInputEvent::KeyPress('p')),
        InputAction::TaskPause(target)
    );
}

#[test]
fn ac8_copy_completed_task_uses_result_text() {
    let mut plan = make_plan(
        PlanStatus::Completed,
        vec![make_task(1, "T1", PlanTaskStatus::Completed)],
    );
    plan.tasks[0].result = Some(TaskResult {
        text: "deliverable contents".to_string(),
        tool_call_count: 3,
        token_count: Some(420),
    });
    let conv = make_conv_with_plan(plan);
    let payload = resolve_copy_task_payload(&conv, Some("test-plan"), 1, None)
        .expect("completed task with result must produce a payload");
    assert_eq!(payload.text, "deliverable contents");
    assert!(payload.flash_message.contains("Copied"));
}

#[test]
fn ac8_copy_failed_task_uses_error_text() {
    let mut plan = make_plan(
        PlanStatus::Completed,
        vec![make_task(1, "T1", PlanTaskStatus::Failed)],
    );
    plan.tasks[0].error = Some("compilation error".to_string());
    let conv = make_conv_with_plan(plan);
    let payload = resolve_copy_task_payload(&conv, None, 1, Some("test-plan"))
        .expect("failed task with error must produce a payload");
    assert_eq!(payload.text, "compilation error");
    assert!(payload.flash_message.contains("error"));
}

#[test]
fn ac8_copy_cancelled_task_uses_its_own_wording() {
    let mut plan = make_plan(
        PlanStatus::Cancelled,
        vec![make_task(1, "T1", PlanTaskStatus::Cancelled)],
    );
    plan.tasks[0].error = Some("plan cancelled".to_string());
    let conv = make_conv_with_plan(plan);
    let payload = resolve_copy_task_payload(&conv, Some("test-plan"), 1, None)
        .expect("a cancelled task with a reason must produce a payload");
    assert_eq!(payload.text, "plan cancelled");
    assert_eq!(payload.flash_message, "Copied cancel reason to clipboard.");
}

#[test]
fn pd2_running_detector_only_true_for_executing_plan_with_running_task() {
    // No plans → false.
    let empty = empty_conv();
    assert!(!any_plan_has_running_task(&empty));
    // Executing plan with Running task → true.
    let running = make_conv_with_plan(make_plan(
        PlanStatus::Executing,
        vec![make_task(1, "T", PlanTaskStatus::Running)],
    ));
    assert!(any_plan_has_running_task(&running));
    // Completed plan with Running task (impossible state, but defensive) → false.
    let stale = make_conv_with_plan(make_plan(
        PlanStatus::Completed,
        vec![make_task(1, "T", PlanTaskStatus::Running)],
    ));
    assert!(!any_plan_has_running_task(&stale));
    // Executing plan without any Running task → false.
    let pending = make_conv_with_plan(make_plan(
        PlanStatus::Executing,
        vec![make_task(1, "T", PlanTaskStatus::Pending)],
    ));
    assert!(!any_plan_has_running_task(&pending));
}

#[test]
fn ac1_plan_start_event_for_other_conversation_ignored() {
    // old: ac1_auto_open_event_for_other_conversation_ignored.
    let conv = make_conv_with_plan(make_plan(
        PlanStatus::Executing,
        vec![make_task(1, "T", PlanTaskStatus::Pending)],
    ));
    let mut state = TuiState::new(160, 24);
    let matched =
        handle_plan_execution_started(&mut state, &conv, "different-conversation-id", "test-plan");
    assert!(!matched);
    assert!(!state.monitor);
    assert!(state.task_panel_state.last_executed_plan_id.is_none());
}

// ─────────────────────────────────────────────────────────────────────────────
// AC6: Enter on a Tasks row names the right task (old: Enter on the sidebar drilled in)
// ─────────────────────────────────────────────────────────────────────────────

fn three_task_conv() -> Conversation {
    make_conv_with_plan(make_plan(
        PlanStatus::Executing,
        vec![
            make_task(1, "T1", PlanTaskStatus::Pending),
            make_task(2, "T2", PlanTaskStatus::Pending),
            make_task(3, "T3", PlanTaskStatus::Pending),
        ],
    ))
}

#[test]
fn ac6_enter_on_task_1_opens_task_1() {
    let conv = three_task_conv();
    let mut state = hub_on_tasks(&conv);
    assert_eq!(
        press(&mut state, DomainKey::Enter),
        InputAction::OpenTaskWindow(task_ref(&conv, 1))
    );
}

#[test]
fn ac6_enter_on_task_2_opens_task_2() {
    let conv = three_task_conv();
    let mut state = hub_on_tasks(&conv);
    press(&mut state, DomainKey::Down);
    assert_eq!(
        press(&mut state, DomainKey::Enter),
        InputAction::OpenTaskWindow(task_ref(&conv, 2))
    );
}

#[test]
fn ac6_enter_on_a_sub_task_row_opens_its_parent_task() {
    let mut parent = make_task(1, "Build", PlanTaskStatus::Running);
    parent.sub_tasks = vec![make_sub_task(1, "compile", PlanTaskStatus::Running)];
    let conv = make_conv_with_plan(make_plan(PlanStatus::Executing, vec![parent]));
    let mut state = hub_on_tasks(&conv);
    press(&mut state, DomainKey::Down);
    assert_eq!(
        state.mission_control.selected_id(),
        Some("test-plan#1.1"),
        "the second row is the sub-task"
    );
    assert_eq!(
        press(&mut state, DomainKey::Enter),
        InputAction::OpenTaskWindow(task_ref(&conv, 1))
    );
}

#[test]
fn ac6_arrow_down_navigates_tasks_tab() {
    // old: Down moved `selected_index` and `sidebar_selected`; now it moves the tab's
    // selection, which names the task a key acts on.
    let conv = three_task_conv();
    let mut state = hub_on_tasks(&conv);
    assert_eq!(
        tasks::selected_ref(&state),
        Some(task_ref(&conv, 1)),
        "row 1 starts selected"
    );
    press(&mut state, DomainKey::Down);
    assert_eq!(tasks::selected_ref(&state), Some(task_ref(&conv, 2)));
    press(&mut state, DomainKey::Down);
    press(&mut state, DomainKey::Down);
    assert_eq!(
        tasks::selected_ref(&state),
        Some(task_ref(&conv, 3)),
        "the selection stops at the last row"
    );
}

#[test]
fn ac6_arrow_up_navigates_tasks_tab() {
    let conv = three_task_conv();
    let mut state = hub_on_tasks(&conv);
    for _ in 0..2 {
        press(&mut state, DomainKey::Down);
    }
    assert_eq!(tasks::selected_ref(&state), Some(task_ref(&conv, 3)));
    press(&mut state, DomainKey::Up);
    assert_eq!(tasks::selected_ref(&state), Some(task_ref(&conv, 2)));
}

// ac7_enter_in_task_detail_toggles_expanded (old): Enter inside the drill-down toggled
// `expanded_detail` (result collapsed/expanded). The collapse is deleted (A8): the task window
// shows the whole result, scrolls it with the RU-25 keys and renders a completed task's result
// as markdown by default while an error / skip reason stays literal.
#[test]
fn ac7_task_window_shows_the_whole_result_rendered_and_an_error_literal() {
    let mut plan = make_plan(
        PlanStatus::Completed,
        vec![
            make_task(1, "Done", PlanTaskStatus::Completed),
            make_task(2, "Bombed", PlanTaskStatus::Failed),
        ],
    );
    plan.tasks[0].result = Some(TaskResult {
        text: "**all** green".to_string(),
        tool_call_count: 1,
        token_count: None,
    });
    plan.tasks[1].error = Some("**not markdown**".to_string());
    let conv = make_conv_with_plan(plan);
    let mut state = hub_on_tasks(&conv);
    assert!(task_window::open(&mut state, &conv, &task_ref(&conv, 1)));
    assert_eq!(
        state.task_window.as_ref().unwrap().text.mode,
        BodyMode::Rendered
    );
    assert!(task_window::open(&mut state, &conv, &task_ref(&conv, 2)));
    let window = state.task_window.as_ref().unwrap();
    assert_eq!(
        window.number, 2,
        "opening another task retargets the one window"
    );
    assert_eq!(window.text.mode, BodyMode::Source);
    assert_eq!(window.status(), PlanTaskStatus::Failed);
}
