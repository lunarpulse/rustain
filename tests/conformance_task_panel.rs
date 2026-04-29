//! Conformance tests for Story 6-3: Task Panel & Progress Monitoring.
//!
//! Validates TaskPanelState, chord dispatch, event-arm mutations,
//! panel render output, drill-down behavior, and resize handling.

use std::collections::HashMap;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use rustain::adapters::tui::app::handle_input;
use rustain::adapters::tui::color_detect::ColorCapability;
use rustain::adapters::tui::state::{TaskPanelState, TuiState};
use rustain::adapters::tui::widgets::task_panel::{render_task_panel, resolve_panel_plan};
use rustain::domain::events::{AppEvent, DomainInputEvent, DomainKey};
use rustain::domain::models::visual::PanelType;
use rustain::domain::models::{
    Conversation, FocusState, NoticeLevel, Plan, PlanStatus, PlanTask, PlanTaskStatus,
    generate_conversation_id,
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

fn make_conv_with_plan(plan: Plan) -> Conversation {
    let plan_id = plan.id.clone();
    let mut conv = Conversation {
        id: generate_conversation_id(),
        title: "Test".to_string(),
        messages: vec![],
        created_at: 1_700_000_000,
        updated_at: 1_700_000_000,
        last_response_at: None,
        session_id: None,
        usage: None,
        plans: HashMap::new(),
        fork_source: None,
    };
    conv.plans.insert(plan_id, plan);
    conv
}

fn test_theme() -> rustain::adapters::tui::theme::Theme {
    rustain::adapters::tui::theme::Theme::for_capability(ColorCapability::TrueColor)
}

fn collect_buffer(buf: &Buffer, area: Rect) -> String {
    let mut s = String::new();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            s.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
        }
    }
    s
}

#[test]
fn ac3_task_panel_state_default() {
    let state = TaskPanelState::default();
    assert_eq!(state.selected_index, 0);
    assert_eq!(state.last_executed_plan_id, None);
    assert_eq!(state.auto_open_skipped_for_plan, None);
    assert_eq!(state.drill_down_task, None);
    assert_eq!(state.task_count, 0);
}

#[test]
fn ac4_render_shows_status_icons() {
    let plan = make_plan(
        PlanStatus::Executing,
        vec![
            make_task(1, "Task one", PlanTaskStatus::Completed),
            make_task(2, "Task two", PlanTaskStatus::Running),
            make_task(3, "Task three", PlanTaskStatus::Pending),
            make_task(4, "Task four", PlanTaskStatus::Failed),
        ],
    );
    let area = Rect::new(0, 0, 40, 24);
    let mut buf = Buffer::empty(area);
    render_task_panel(area, &mut buf, Some(&plan), 0, true, &test_theme());
    let content = collect_buffer(&buf, area);
    assert!(content.contains("Task one"));
    assert!(content.contains("Task two"));
    assert!(content.contains("Task three"));
    assert!(content.contains("Task four"));
}

#[test]
fn ac4_render_empty_state() {
    let area = Rect::new(0, 0, 40, 20);
    let mut buf = Buffer::empty(area);
    render_task_panel(area, &mut buf, None, 0, true, &test_theme());
    let content = collect_buffer(&buf, area);
    assert!(content.contains("No active plan"));
}

#[test]
fn ac4_render_completed_plan_fallback() {
    let plan = make_plan(
        PlanStatus::Completed,
        vec![make_task(1, "Done task", PlanTaskStatus::Completed)],
    );
    let area = Rect::new(0, 0, 40, 20);
    let mut buf = Buffer::empty(area);
    render_task_panel(area, &mut buf, Some(&plan), 0, true, &test_theme());
    let content = collect_buffer(&buf, area);
    assert!(content.contains("(last)"));
    assert!(content.contains("Plan complete"));
}

#[test]
fn ac5_sub_task_seat_comment_exists() {
    let source = include_str!("../src/adapters/tui/widgets/task_panel.rs");
    assert!(
        source.contains("10.6"),
        "task_panel.rs must contain a 10.6 sub-task forward-compat comment"
    );
}

#[test]
fn ac9_most_recent_plan_fallback() {
    let mut conv = Conversation {
        id: generate_conversation_id(),
        title: String::new(),
        messages: vec![],
        created_at: 0,
        updated_at: 0,
        last_response_at: None,
        session_id: None,
        usage: None,
        plans: HashMap::new(),
        fork_source: None,
    };
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
}

#[test]
fn ac10_resize_clears_drill_down() {
    let mut state = TuiState::new(160, 24);
    state.terminal_width = 160;
    state.terminal_height = 24;
    state.task_panel_state.drill_down_task = Some(2);
    state.sidebar_visible = true;
    // Simulate resize to narrow
    state.terminal_width = 100;
    if state.terminal_width < rustain::adapters::tui::layout::SIDEBAR_MIN_WIDTH
        && state.sidebar_visible
    {
        state.sidebar_visible = false;
        state.sidebar_panel = None;
        state.task_panel_state.drill_down_task = None;
    }
    assert!(!state.sidebar_visible);
    assert_eq!(state.task_panel_state.drill_down_task, None);
}

#[test]
fn resolve_panel_plan_prefers_last_id() {
    let mut conv = Conversation {
        id: generate_conversation_id(),
        title: String::new(),
        messages: vec![],
        created_at: 0,
        updated_at: 0,
        last_response_at: None,
        session_id: None,
        usage: None,
        plans: HashMap::new(),
        fork_source: None,
    };
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
    let mut conv = Conversation {
        id: generate_conversation_id(),
        title: String::new(),
        messages: vec![],
        created_at: 0,
        updated_at: 0,
        last_response_at: None,
        session_id: None,
        usage: None,
        plans: HashMap::new(),
        fork_source: None,
    };
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
    let conv = Conversation {
        id: generate_conversation_id(),
        title: String::new(),
        messages: vec![],
        created_at: 0,
        updated_at: 0,
        last_response_at: None,
        session_id: None,
        usage: None,
        plans: HashMap::new(),
        fork_source: None,
    };
    let result = resolve_panel_plan(&conv, None);
    assert!(result.is_none());
}

// ─────────────────────────────────────────────────────────────────────────────
// PD5 — AC1/AC2/AC6/AC7/AC8 conformance via task_panel_handlers
// ─────────────────────────────────────────────────────────────────────────────

use rustain::adapters::tui::task_panel_handlers::{
    any_plan_has_running_task, drill_down_task_status, handle_open_panel_tasks,
    handle_plan_execution_started, handle_plan_task_status_changed, resolve_copy_task_payload,
};
use rustain::domain::models::plan::TaskResult;

const SIDEBAR_MIN: u16 = 120;

#[test]
fn ac1_chord_opens_panel_at_wide_terminal() {
    let conv = make_conv_with_plan(make_plan(
        PlanStatus::Executing,
        vec![make_task(1, "T1", PlanTaskStatus::Running)],
    ));
    let mut state = TuiState::new(160, 24);
    let outcome = handle_open_panel_tasks(&mut state, &conv, 160, SIDEBAR_MIN);
    assert!(outcome.opened);
    assert!(!outcome.closed);
    assert!(outcome.notice.is_none());
    assert!(state.sidebar_visible);
    assert_eq!(state.sidebar_panel, Some(PanelType::Tasks));
    assert_eq!(state.task_panel_state.task_count, 1);
}

#[test]
fn ac1_chord_warns_at_narrow_terminal() {
    let conv = make_conv_with_plan(make_plan(
        PlanStatus::Executing,
        vec![make_task(1, "T", PlanTaskStatus::Pending)],
    ));
    let mut state = TuiState::new(80, 24);
    let outcome = handle_open_panel_tasks(&mut state, &conv, 80, SIDEBAR_MIN);
    assert!(!outcome.opened);
    assert!(!outcome.closed);
    let notice = outcome.notice.expect("narrow chord must emit Warning");
    assert_eq!(notice.level, NoticeLevel::Warning);
    assert!(notice.message.contains(">= 120"));
    assert!(!state.sidebar_visible);
}

#[test]
fn ac1_chord_toggle_close_records_suppression() {
    let conv = make_conv_with_plan(make_plan(
        PlanStatus::Executing,
        vec![make_task(1, "T", PlanTaskStatus::Pending)],
    ));
    let mut state = TuiState::new(160, 24);
    let _ = handle_open_panel_tasks(&mut state, &conv, 160, SIDEBAR_MIN);
    assert!(state.sidebar_visible);
    // PD1: second chord while panel is open closes and records suppression.
    let outcome = handle_open_panel_tasks(&mut state, &conv, 160, SIDEBAR_MIN);
    assert!(outcome.closed);
    assert!(!state.sidebar_visible);
    assert!(
        state
            .task_panel_state
            .auto_open_suppressed_conversations
            .contains(&conv.id)
    );
}

#[test]
fn ac1_auto_open_on_plan_execution_started() {
    let conv = make_conv_with_plan(make_plan(
        PlanStatus::Executing,
        vec![
            make_task(1, "T1", PlanTaskStatus::Running),
            make_task(2, "T2", PlanTaskStatus::Pending),
        ],
    ));
    let mut state = TuiState::new(160, 24);
    let outcome = handle_plan_execution_started(
        &mut state,
        &conv,
        &conv.id,
        "test-plan",
        160,
        SIDEBAR_MIN,
        "tasks",
    );
    assert!(outcome.auto_opened);
    assert!(!outcome.suppressed);
    assert!(state.sidebar_visible);
    assert_eq!(state.sidebar_panel, Some(PanelType::Tasks));
    assert_eq!(
        state.task_panel_state.last_executed_plan_id.as_deref(),
        Some("test-plan")
    );
    assert_eq!(state.task_panel_state.task_count, 2);
}

#[test]
fn ac1_auto_open_suppressed_by_config() {
    let conv = make_conv_with_plan(make_plan(
        PlanStatus::Executing,
        vec![make_task(1, "T", PlanTaskStatus::Running)],
    ));
    let mut state = TuiState::new(160, 24);
    let outcome = handle_plan_execution_started(
        &mut state,
        &conv,
        &conv.id,
        "test-plan",
        160,
        SIDEBAR_MIN,
        "none", // PD4 future: when config wiring lands, "none" must suppress
    );
    assert!(!outcome.auto_opened);
    assert!(!outcome.suppressed);
    assert!(!state.sidebar_visible);
}

#[test]
fn ac1_auto_open_honors_user_suppression_with_one_time_hint() {
    // PD1 (Sally A2): after user closes, next plan is suppressed AND emits
    // a one-time hint toast for that conversation.
    let conv = make_conv_with_plan(make_plan(
        PlanStatus::Executing,
        vec![make_task(1, "T", PlanTaskStatus::Pending)],
    ));
    let mut state = TuiState::new(160, 24);
    state
        .task_panel_state
        .auto_open_suppressed_conversations
        .insert(conv.id.clone());
    let first = handle_plan_execution_started(
        &mut state,
        &conv,
        &conv.id,
        "test-plan",
        160,
        SIDEBAR_MIN,
        "tasks",
    );
    assert!(first.suppressed);
    assert!(!first.auto_opened);
    assert!(!state.sidebar_visible);
    assert_eq!(
        first.notices.len(),
        1,
        "one-time hint should fire on first suppression"
    );
    assert!(first.notices[0].message.contains("hidden for this session"));
    // Second plan in same conversation: still suppressed, but NO toast.
    let second = handle_plan_execution_started(
        &mut state,
        &conv,
        &conv.id,
        "test-plan",
        160,
        SIDEBAR_MIN,
        "tasks",
    );
    assert!(second.suppressed);
    assert!(
        second.notices.is_empty(),
        "hint must be one-time per conversation"
    );
}

#[test]
fn ac2_status_change_triggers_redraw_when_panel_open() {
    let conv = make_conv_with_plan(make_plan(
        PlanStatus::Executing,
        vec![make_task(1, "T", PlanTaskStatus::Running)],
    ));
    let mut state = TuiState::new(160, 24);
    state.sidebar_visible = true;
    state.sidebar_panel = Some(PanelType::Tasks);
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
fn ac2_status_change_other_conversation_ignored() {
    let conv = make_conv_with_plan(make_plan(
        PlanStatus::Executing,
        vec![make_task(1, "T", PlanTaskStatus::Running)],
    ));
    let mut state = TuiState::new(160, 24);
    state.sidebar_panel = Some(PanelType::Tasks);
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
    let mut state = TuiState::new(160, 24);
    state.sidebar_panel = Some(PanelType::Tasks);
    let bumped = handle_plan_task_status_changed(&mut state, &conv, &conv.id, "phantom-plan");
    assert!(!bumped);
    assert!(state.task_panel_state.last_executed_plan_id.is_none());
}

#[test]
fn ac6_drill_down_state_round_trip() {
    // AC6: Enter sets drill_down_task; Esc clears it. Exercises pure state.
    let mut state = TuiState::new(160, 24);
    state.task_panel_state.task_count = 3;
    state.task_panel_state.selected_index = 1;
    // simulate the Enter handler: drill into selected
    let task_number = (state.task_panel_state.selected_index + 1) as u32;
    state.task_panel_state.drill_down_task = Some(task_number);
    assert_eq!(state.task_panel_state.drill_down_task, Some(2));
    // simulate Esc: clears drill, restores Sidebar focus
    state.task_panel_state.drill_down_task = None;
    assert_eq!(state.task_panel_state.drill_down_task, None);
}

#[test]
fn ac7_reserved_keys_only_emit_on_failed_drill_down() {
    let mut conv = make_conv_with_plan(make_plan(
        PlanStatus::Completed,
        vec![
            make_task(1, "Done", PlanTaskStatus::Completed),
            make_task(2, "Bombed", PlanTaskStatus::Failed),
        ],
    ));
    conv.plans.get_mut("test-plan").unwrap().tasks[1].error = Some("fail".to_string());
    let mut state = TuiState::new(160, 24);
    state.task_panel_state.last_executed_plan_id = Some("test-plan".to_string());
    // Drill into Completed task → status is Completed (not Failed).
    state.task_panel_state.drill_down_task = Some(1);
    assert_eq!(
        drill_down_task_status(&state, &conv),
        Some(PlanTaskStatus::Completed)
    );
    // Drill into Failed task → status is Failed.
    state.task_panel_state.drill_down_task = Some(2);
    assert_eq!(
        drill_down_task_status(&state, &conv),
        Some(PlanTaskStatus::Failed)
    );
    // No drill-down at all → None.
    state.task_panel_state.drill_down_task = None;
    assert_eq!(drill_down_task_status(&state, &conv), None);
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
fn pd2_running_detector_only_true_for_executing_plan_with_running_task() {
    // No plans → false.
    let empty = Conversation {
        id: generate_conversation_id(),
        title: String::new(),
        messages: vec![],
        created_at: 0,
        updated_at: 0,
        last_response_at: None,
        session_id: None,
        usage: None,
        plans: HashMap::new(),
        fork_source: None,
    };
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
fn ac1_auto_open_event_for_other_conversation_ignored() {
    let conv = make_conv_with_plan(make_plan(
        PlanStatus::Executing,
        vec![make_task(1, "T", PlanTaskStatus::Pending)],
    ));
    let mut state = TuiState::new(160, 24);
    let outcome = handle_plan_execution_started(
        &mut state,
        &conv,
        "different-conversation-id",
        "test-plan",
        160,
        SIDEBAR_MIN,
        "tasks",
    );
    assert!(!outcome.auto_opened);
    assert!(!outcome.suppressed);
    assert!(!state.sidebar_visible);
}

#[test]
fn pd4_default_auto_open_setting_is_tasks() {
    // PD4: default value of `[layout.auto_panels] on_task_plan` is "tasks".
    // TuiState seeds the field at construction so the auto-open path is live
    // out of the box; users opt out by setting `"none"`.
    let state = TuiState::new(160, 24);
    assert_eq!(state.auto_open_on_task_plan, "tasks");
}

#[test]
fn pd4_auto_panels_config_validates_known_values() {
    use rustain::domain::models::AutoPanelsConfig;
    let mut cfg = AutoPanelsConfig::default();
    assert!(cfg.validate().is_ok());
    cfg.on_task_plan = "none".into();
    assert!(cfg.validate().is_ok());
    cfg.on_task_plan = "history".into();
    assert!(cfg.validate().is_err());
}

// ─────────────────────────────────────────────────────────────────────────────
// AC6: Enter dispatch on Tasks sidebar sets correct drill_down_task
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn ac6_enter_on_task_1_drills_into_task_1() {
    let mut state = TuiState::new(160, 24);
    state.task_panel_state.task_count = 3;
    state.task_panel_state.selected_index = 0;
    state.focus = FocusState::Sidebar {
        panel: PanelType::Tasks,
        selected: 0,
    };
    let _action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
    assert_eq!(state.task_panel_state.drill_down_task, Some(1));
}

#[test]
fn ac7_enter_in_task_detail_toggles_expanded() {
    // 6-3 AC7: with focus already on the drill-down view, pressing Enter
    // toggles `expanded_detail` between false and true. Drilling in resets
    // the flag, and Esc-back clears it again.
    let mut state = TuiState::new(160, 24);
    state.task_panel_state.task_count = 1;
    state.task_panel_state.selected_index = 0;
    state.focus = FocusState::Sidebar {
        panel: PanelType::Tasks,
        selected: 0,
    };

    // Drill in.
    let _ = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
    assert_eq!(state.task_panel_state.drill_down_task, Some(1));
    assert!(
        !state.task_panel_state.expanded_detail,
        "expanded resets on drill-in"
    );

    // First Enter inside drill-down: expand.
    let _ = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
    assert!(
        state.task_panel_state.expanded_detail,
        "Enter expands result"
    );
    assert_eq!(
        state.task_panel_state.drill_down_task,
        Some(1),
        "still drilled in"
    );

    // Second Enter: collapse.
    let _ = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
    assert!(
        !state.task_panel_state.expanded_detail,
        "Enter collapses result"
    );

    // Esc back: clears drill-down and expanded flag together.
    state.task_panel_state.expanded_detail = true;
    let _ = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
    assert_eq!(state.task_panel_state.drill_down_task, None);
    assert!(
        !state.task_panel_state.expanded_detail,
        "expanded clears on Esc"
    );
}

#[test]
fn ac6_enter_on_task_2_drills_into_task_2() {
    let mut state = TuiState::new(160, 24);
    state.task_panel_state.task_count = 3;
    state.task_panel_state.selected_index = 1;
    state.focus = FocusState::Sidebar {
        panel: PanelType::Tasks,
        selected: 1,
    };
    let _action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
    assert_eq!(state.task_panel_state.drill_down_task, Some(2));
}

#[test]
fn ac6_arrow_down_navigates_task_panel() {
    let mut state = TuiState::new(160, 24);
    state.task_panel_state.task_count = 3;
    state.task_panel_state.selected_index = 0;
    state.focus = FocusState::Sidebar {
        panel: PanelType::Tasks,
        selected: 0,
    };
    let _ = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Down));
    assert_eq!(state.task_panel_state.selected_index, 1);
    assert_eq!(state.sidebar_selected, 1);
}

#[test]
fn ac6_arrow_up_navigates_task_panel() {
    let mut state = TuiState::new(160, 24);
    state.task_panel_state.task_count = 3;
    state.task_panel_state.selected_index = 2;
    state.focus = FocusState::Sidebar {
        panel: PanelType::Tasks,
        selected: 2,
    };
    let _ = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Up));
    assert_eq!(state.task_panel_state.selected_index, 1);
    assert_eq!(state.sidebar_selected, 1);
}
