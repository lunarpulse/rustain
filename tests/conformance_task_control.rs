//! Conformance tests for Story 6-4: Task Control & Plan Deviation.
//!
//! Tests handler functions directly (no TUI/event-loop needed).
//! Follows the `conformance_task_panel.rs` pattern from 6-3 PD5.

use std::collections::HashMap;

use rustain::adapters::tui::state::{TaskPanelState, TuiState};
use rustain::adapters::tui::task_panel_handlers::{
    dependents, drill_down_task_status,
    handle_task_pause, handle_task_skip, resolve_panel_task_number,
};
use rustain::domain::models::{
    Conversation, FocusState, Plan, PlanStatus, PlanTask, PlanTaskStatus,
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

fn make_task_with_deps(number: u32, title: &str, status: PlanTaskStatus, deps: Vec<u32>) -> PlanTask {
    PlanTask {
        number,
        title: title.to_string(),
        description: String::new(),
        depends_on: deps,
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

fn make_conv(plan: Plan) -> (Conversation, TuiState) {
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
        plans: HashMap::from([(plan_id.clone(), plan)]),
        fork_source: None,
    };
    let mut state = TuiState::new(160, 24);
    state.task_panel_state.last_executed_plan_id = Some(plan_id);
    state.task_panel_state.selected_index = 0;
    (conv, state)
}


// ── AC1: Pause / Resume ──────────────────────────────────────────────────────


#[test]
fn ac1_pause_pending_flips_to_paused() {
    let plan = make_plan(PlanStatus::Executing, vec![
        make_task(1, "T1", PlanTaskStatus::Pending),
    ]);
    let (mut conv, mut state) = make_conv(plan);

    let outcome = handle_task_pause(&mut state, &mut conv, 1);
    // Should have no notices for a simple pause
    assert!(!outcome.should_resume_advance);
    assert!(outcome.running_task_paused.is_none());

    let plan = conv.plans.get("test-plan").unwrap();
    assert_eq!(plan.tasks[0].status, PlanTaskStatus::Paused);
}


#[test]
fn ac1_pause_running_marks_pending_cancel() {
    let plan = make_plan(PlanStatus::Executing, vec![
        make_task(1, "T1", PlanTaskStatus::Running),
    ]);
    let (mut conv, mut state) = make_conv(plan);

    let outcome = handle_task_pause(&mut state, &mut conv, 1);
    assert!(!outcome.should_resume_advance);
    // Running task pause should return running_task_paused = Some(n)
    // so the caller can cancel the token
    assert_eq!(outcome.running_task_paused, Some(1));
}


#[test]
fn ac1_pause_propagates_to_dependents() {
    let plan = make_plan(PlanStatus::Executing, vec![
        make_task(1, "T1", PlanTaskStatus::Pending),
        make_task_with_deps(2, "T2", PlanTaskStatus::Pending, vec![1]),
    ]);
    let (mut conv, mut state) = make_conv(plan);

    handle_task_pause(&mut state, &mut conv, 1);
    let plan = conv.plans.get("test-plan").unwrap();
    assert_eq!(plan.tasks[0].status, PlanTaskStatus::Paused);
    assert_eq!(plan.tasks[1].status, PlanTaskStatus::Paused);
}


#[test]
fn ac1_resume_paused_back_to_pending() {
    let plan = make_plan(PlanStatus::Executing, vec![
        make_task(1, "T1", PlanTaskStatus::Paused),
    ]);
    let (mut conv, mut state) = make_conv(plan);

    let outcome = handle_task_pause(&mut state, &mut conv, 1);
    assert!(outcome.should_resume_advance);
    assert!(outcome.running_task_paused.is_none());

    let plan = conv.plans.get("test-plan").unwrap();
    assert_eq!(plan.tasks[0].status, PlanTaskStatus::Pending);
}


#[test]
fn ac1_pause_non_pausable_emits_notice() {
    let plan = make_plan(PlanStatus::Executing, vec![
        make_task(1, "T1", PlanTaskStatus::Completed),
    ]);
    let (mut conv, mut state) = make_conv(plan);

    let outcome = handle_task_pause(&mut state, &mut conv, 1);
    assert!(outcome.notices.iter().any(|n| n.message.contains("cannot be paused")));
    // Status must NOT change
    let plan = conv.plans.get("test-plan").unwrap();
    assert_eq!(plan.tasks[0].status, PlanTaskStatus::Completed);
}


// ── AC1.2: Resume with Running reset ─────────────────────────────────────────


#[test]
fn ac1_resume_was_running_resets_started_at_ms() {
    // A task that was Running when paused should reset started_at_ms on resume
    let plan = make_plan(PlanStatus::Executing, vec![
        PlanTask {
            number: 1,
            title: "T1".to_string(),
            description: String::new(),
            depends_on: vec![],
            status: PlanTaskStatus::Paused,
            started_at_ms: Some(1000),
            completed_at_ms: None,
            result: None,
            error: None,
            waiting_on: vec![],
        },
    ]);
    let (mut conv, mut state) = make_conv(plan);

    let outcome = handle_task_pause(&mut state, &mut conv, 1);
    assert!(outcome.should_resume_advance);

    let plan = conv.plans.get("test-plan").unwrap();
    assert_eq!(plan.tasks[0].status, PlanTaskStatus::Pending);
    assert_eq!(plan.tasks[0].started_at_ms, None);
}


// ── AC2: Skip ────────────────────────────────────────────────────────────────


#[test]
fn ac2_skip_pending_no_dependents() {
    let plan = make_plan(PlanStatus::Executing, vec![
        make_task(1, "T1", PlanTaskStatus::Pending),
    ]);
    let (mut conv, mut state) = make_conv(plan);

    let notices = handle_task_skip(&mut state, &mut conv, 1);
    let plan = conv.plans.get("test-plan").unwrap();
    assert_eq!(plan.tasks[0].status, PlanTaskStatus::Skipped);
    assert_eq!(plan.tasks[0].error.as_deref(), Some("Skipped by user"));
    // No cascade card for no dependents
    assert!(state.task_panel_state.skip_cascade_pending.is_none());
}


#[test]
fn ac2_skip_with_dependents_shows_cascade() {
    let plan = make_plan(PlanStatus::Executing, vec![
        make_task(1, "T1", PlanTaskStatus::Pending),
        make_task_with_deps(2, "T2", PlanTaskStatus::Pending, vec![1]),
    ]);
    let (mut conv, mut state) = make_conv(plan);

    handle_task_skip(&mut state, &mut conv, 1);
    let plan = conv.plans.get("test-plan").unwrap();
    assert_eq!(plan.tasks[0].status, PlanTaskStatus::Skipped);
    // Cascade card should be pending because T2 depends on T1
    assert!(state.task_panel_state.skip_cascade_pending.is_some());
    let pending = state.task_panel_state.skip_cascade_pending.as_ref().unwrap();
    assert_eq!(pending.source_task, 1);
    assert_eq!(pending.source_prior_status, PlanTaskStatus::Pending);
    assert_eq!(pending.downstream, vec![2]);
}


#[test]
fn ac2_skip_non_skippable_emits_notice() {
    let plan = make_plan(PlanStatus::Executing, vec![
        make_task(1, "T1", PlanTaskStatus::Completed),
    ]);
    let (mut conv, mut state) = make_conv(plan);

    let notices = handle_task_skip(&mut state, &mut conv, 1);
    assert!(notices.iter().any(|n| n.message.contains("cannot be skipped")));
    let plan = conv.plans.get("test-plan").unwrap();
    // Status must NOT change
    assert_eq!(plan.tasks[0].status, PlanTaskStatus::Completed);
}


// ── AC2: Skip cascade card persists prior status ─────────────────────────────


#[test]
fn ac2_skip_failed_preserves_prior_status_in_cascade() {
    let plan = make_plan(PlanStatus::Executing, vec![
        PlanTask {
            number: 1, title: "T1".to_string(), description: String::new(),
            depends_on: vec![], status: PlanTaskStatus::Failed,
            started_at_ms: None, completed_at_ms: Some(2000),
            result: None, error: Some("fail".into()), waiting_on: vec![],
        },
        make_task_with_deps(2, "T2", PlanTaskStatus::Pending, vec![1]),
    ]);
    let (mut conv, mut state) = make_conv(plan);

    handle_task_skip(&mut state, &mut conv, 1);
    let pending = state.task_panel_state.skip_cascade_pending.as_ref().unwrap();
    assert_eq!(pending.source_prior_status, PlanTaskStatus::Failed);
}


// ── AC11: drill_down_task_status ─────────────────────────────────────────────


#[test]
fn ac11_drill_down_task_status_returns_status_for_drilled_task() {
    let plan = make_plan(PlanStatus::Executing, vec![
        make_task(1, "T1", PlanTaskStatus::Completed),
        make_task(2, "T2", PlanTaskStatus::Failed),
        make_task(3, "T3", PlanTaskStatus::Paused),
    ]);
    let (conv, mut state) = make_conv(plan);

    state.task_panel_state.drill_down_task = Some(1);
    assert_eq!(drill_down_task_status(&state, &conv), Some(PlanTaskStatus::Completed));

    state.task_panel_state.drill_down_task = Some(2);
    assert_eq!(drill_down_task_status(&state, &conv), Some(PlanTaskStatus::Failed));

    state.task_panel_state.drill_down_task = Some(3);
    assert_eq!(drill_down_task_status(&state, &conv), Some(PlanTaskStatus::Paused));

    state.task_panel_state.drill_down_task = None;
    assert_eq!(drill_down_task_status(&state, &conv), None);
}


// ── Helpers: dependents ──────────────────────────────────────────────────────


#[test]
fn dependents_no_deps() {
    let plan = make_plan(PlanStatus::Executing, vec![
        make_task(1, "T1", PlanTaskStatus::Pending),
    ]);
    assert!(dependents(&plan, 1).is_empty());
}


#[test]
fn dependents_single_dep() {
    let plan = make_plan(PlanStatus::Executing, vec![
        make_task(1, "T1", PlanTaskStatus::Pending),
        make_task_with_deps(2, "T2", PlanTaskStatus::Pending, vec![1]),
    ]);
    assert_eq!(dependents(&plan, 1), vec![2]);
}


#[test]
fn dependents_transitive_chain() {
    let plan = make_plan(PlanStatus::Executing, vec![
        make_task(1, "T1", PlanTaskStatus::Pending),
        make_task_with_deps(2, "T2", PlanTaskStatus::Pending, vec![1]),
        make_task_with_deps(3, "T3", PlanTaskStatus::Pending, vec![2]),
    ]);
    let mut deps = dependents(&plan, 1);
    deps.sort();
    assert_eq!(deps, vec![2, 3]);
}


#[test]
fn dependents_cycle_defensive() {
    // depends_on should never form a cycle, but defensively
    let plan = make_plan(PlanStatus::Executing, vec![
        make_task_with_deps(1, "T1", PlanTaskStatus::Pending, vec![2]),
        make_task_with_deps(2, "T2", PlanTaskStatus::Pending, vec![1]),
    ]);
    // Should not hang — cycle is detected via seen set
    let deps = dependents(&plan, 1);
    assert_eq!(deps, vec![2]); // only T2 depends on T1; T1 is already seen
}


// ── Helpers: resolve_panel_task_number ───────────────────────────────────────


#[test]
fn resolve_panel_task_number_works() {
    let plan = make_plan(PlanStatus::Executing, vec![
        make_task(1, "T1", PlanTaskStatus::Pending),
        make_task(2, "T2", PlanTaskStatus::Pending),
    ]);
    let (conv, state) = make_conv(plan);

    assert_eq!(resolve_panel_task_number(&state, &conv, 0), Some(1));
    assert_eq!(resolve_panel_task_number(&state, &conv, 1), Some(2));
    assert_eq!(resolve_panel_task_number(&state, &conv, 99), None);
}


// ── Paused normalization in is_terminal ──────────────────────────────────────


#[test]
fn ac3_is_terminal_paused_is_false() {
    // Paused is NOT terminal — find_next_eligible skips it but resume flips back
    use rustain::domain::services::plan_runtime::is_terminal_pub;
    assert!(!is_terminal_pub(PlanTaskStatus::Pending));
    assert!(!is_terminal_pub(PlanTaskStatus::Running));
    assert!(!is_terminal_pub(PlanTaskStatus::Waiting));
    assert!(!is_terminal_pub(PlanTaskStatus::Paused));
    assert!(is_terminal_pub(PlanTaskStatus::Completed));
    assert!(is_terminal_pub(PlanTaskStatus::Failed));
    assert!(is_terminal_pub(PlanTaskStatus::Skipped));
    assert!(is_terminal_pub(PlanTaskStatus::Cancelled));
}


// ── Reorder validation ───────────────────────────────────────────────────────


#[test]
fn ac4_validate_reorder_identity_ok() {
    use rustain::domain::services::plan_runtime::validate_reorder;
    let plan = make_plan(PlanStatus::Executing, vec![
        make_task(1, "T1", PlanTaskStatus::Pending),
        make_task(2, "T2", PlanTaskStatus::Pending),
    ]);
    assert!(validate_reorder(&plan, 1, 0).is_ok()); // at position 0 already
    assert!(validate_reorder(&plan, 1, 0).is_ok());
}


#[test]
fn ac4_validate_reorder_dep_violation() {
    use rustain::domain::services::plan_runtime::validate_reorder;
    let plan = make_plan(PlanStatus::Executing, vec![
        make_task(1, "T1", PlanTaskStatus::Pending),
        make_task_with_deps(2, "T2", PlanTaskStatus::Pending, vec![1]),
    ]);
    // Cannot push T2 before T1 (it depends on T1)
    assert!(validate_reorder(&plan, 2, 0).is_err());
}


#[test]
fn ac4_validate_reorder_only_pending() {
    use rustain::domain::services::plan_runtime::validate_reorder;
    let plan = make_plan(PlanStatus::Executing, vec![
        make_task(1, "T1", PlanTaskStatus::Completed),
        make_task(2, "T2", PlanTaskStatus::Pending),
    ]);
    assert!(validate_reorder(&plan, 1, 1).is_err()); // not Pending
}
