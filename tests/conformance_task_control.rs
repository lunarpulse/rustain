//! Conformance tests for Story 6-4: Task Control & Plan Deviation.
//!
//! Tests handler functions directly (no TUI/event-loop needed).
//! Follows the `conformance_task_panel.rs` pattern from 6-3 PD5.

use std::collections::HashMap;

use rustain::adapters::tui::mission_control::tasks;
use rustain::adapters::tui::state::TuiState;
use rustain::adapters::tui::task_panel_handlers::{
    dependents, handle_task_pause, handle_task_skip,
};
use rustain::domain::models::{
    Conversation, Plan, PlanStatus, PlanTask, PlanTaskStatus, generate_conversation_id,
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

fn make_task_with_deps(
    number: u32,
    title: &str,
    status: PlanTaskStatus,
    deps: Vec<u32>,
) -> PlanTask {
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
        delegated_to: None,
        sub_tasks: vec![],
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
        origin_restrictions: None,
    }
}

fn make_conv(plan: Plan) -> (Conversation, TuiState) {
    let plan_id = plan.id.clone();
    let conv = Conversation {
        id: generate_conversation_id(),
        title: "Test".to_string(),
        messages: vec![],
        turns: Vec::new(),
        created_at: 1_700_000_000,
        updated_at: 1_700_000_000,
        last_response_at: None,
        session_id: None,
        usage: None,
        plans: HashMap::from([(plan_id.clone(), plan)]),
        fork_source: None,
        compaction: None,
        peer_row_controls: Default::default(),
        peer_row_revision: 0,
    };
    let mut state = TuiState::new(160, 24);
    state.task_panel_state.last_executed_plan_id = Some(plan_id);
    (conv, state)
}

// ── AC1: Pause / Resume ──────────────────────────────────────────────────────

#[test]
fn ac1_pause_pending_flips_to_paused() {
    let plan = make_plan(
        PlanStatus::Executing,
        vec![make_task(1, "T1", PlanTaskStatus::Pending)],
    );
    let (mut conv, _state) = make_conv(plan);

    let outcome = handle_task_pause(&mut conv, "test-plan", 1);
    // Should have no notices for a simple pause
    assert!(!outcome.should_resume_advance);
    assert!(outcome.running_task_paused.is_none());

    let plan = conv.plans.get("test-plan").unwrap();
    assert_eq!(plan.tasks[0].status, PlanTaskStatus::Paused);
}

#[test]
fn ac1_pause_running_marks_pending_cancel() {
    let plan = make_plan(
        PlanStatus::Executing,
        vec![make_task(1, "T1", PlanTaskStatus::Running)],
    );
    let (mut conv, _state) = make_conv(plan);

    let outcome = handle_task_pause(&mut conv, "test-plan", 1);
    assert!(!outcome.should_resume_advance);
    // Running task pause should return running_task_paused = Some(n)
    // so the caller can cancel the token
    assert_eq!(outcome.running_task_paused, Some(1));
}

#[test]
fn ac1_pause_propagates_to_dependents() {
    let plan = make_plan(
        PlanStatus::Executing,
        vec![
            make_task(1, "T1", PlanTaskStatus::Pending),
            make_task_with_deps(2, "T2", PlanTaskStatus::Pending, vec![1]),
        ],
    );
    let (mut conv, _state) = make_conv(plan);

    handle_task_pause(&mut conv, "test-plan", 1);
    let plan = conv.plans.get("test-plan").unwrap();
    assert_eq!(plan.tasks[0].status, PlanTaskStatus::Paused);
    assert_eq!(plan.tasks[1].status, PlanTaskStatus::Paused);
}

#[test]
fn ac1_resume_paused_back_to_pending() {
    let plan = make_plan(
        PlanStatus::Executing,
        vec![make_task(1, "T1", PlanTaskStatus::Paused)],
    );
    let (mut conv, _state) = make_conv(plan);

    let outcome = handle_task_pause(&mut conv, "test-plan", 1);
    assert!(outcome.should_resume_advance);
    assert!(outcome.running_task_paused.is_none());

    let plan = conv.plans.get("test-plan").unwrap();
    assert_eq!(plan.tasks[0].status, PlanTaskStatus::Pending);
}

#[test]
fn ac1_pause_non_pausable_emits_notice() {
    let plan = make_plan(
        PlanStatus::Executing,
        vec![make_task(1, "T1", PlanTaskStatus::Completed)],
    );
    let (mut conv, _state) = make_conv(plan);

    let outcome = handle_task_pause(&mut conv, "test-plan", 1);
    assert!(
        outcome
            .notices
            .iter()
            .any(|n| n.message.contains("cannot be paused"))
    );
    // Status must NOT change
    let plan = conv.plans.get("test-plan").unwrap();
    assert_eq!(plan.tasks[0].status, PlanTaskStatus::Completed);
}

// ── AC1.2: Resume with Running reset ─────────────────────────────────────────

#[test]
fn ac1_resume_was_running_resets_started_at_ms() {
    // A task that was Running when paused should reset started_at_ms on resume
    let plan = make_plan(
        PlanStatus::Executing,
        vec![PlanTask {
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
            delegated_to: None,
            sub_tasks: vec![],
        }],
    );
    let (mut conv, _state) = make_conv(plan);

    let outcome = handle_task_pause(&mut conv, "test-plan", 1);
    assert!(outcome.should_resume_advance);

    let plan = conv.plans.get("test-plan").unwrap();
    assert_eq!(plan.tasks[0].status, PlanTaskStatus::Pending);
    assert_eq!(plan.tasks[0].started_at_ms, None);
}

// ── AC2: Skip ────────────────────────────────────────────────────────────────

/// The skip-cascade decision queued by the skip handler, if any.
fn queued_cascade(
    state: &TuiState,
) -> Option<rustain::adapters::tui::decision_queue::SkipCascadePending> {
    use rustain::adapters::tui::decision_queue::DecisionSubject;
    state
        .decisions
        .items()
        .find_map(|item| match &item.subject {
            DecisionSubject::SkipCascade(payload) => Some(payload.0.clone()),
            _ => None,
        })
}

#[test]
fn ac2_skip_pending_no_dependents() {
    let plan = make_plan(
        PlanStatus::Executing,
        vec![make_task(1, "T1", PlanTaskStatus::Pending)],
    );
    let (mut conv, mut state) = make_conv(plan);

    let _notices = handle_task_skip(&mut state, &mut conv, "test-plan", 1);
    let plan = conv.plans.get("test-plan").unwrap();
    assert_eq!(plan.tasks[0].status, PlanTaskStatus::Skipped);
    assert_eq!(plan.tasks[0].error.as_deref(), Some("Skipped by user"));
    // No cascade card for no dependents
    assert!(queued_cascade(&state).is_none());
    assert!(state.decisions.is_empty());
}

#[test]
fn ac2_skip_with_dependents_shows_cascade() {
    let plan = make_plan(
        PlanStatus::Executing,
        vec![
            make_task(1, "T1", PlanTaskStatus::Pending),
            make_task_with_deps(2, "T2", PlanTaskStatus::Pending, vec![1]),
        ],
    );
    let (mut conv, mut state) = make_conv(plan);

    handle_task_skip(&mut state, &mut conv, "test-plan", 1);
    let plan = conv.plans.get("test-plan").unwrap();
    assert_eq!(plan.tasks[0].status, PlanTaskStatus::Skipped);
    // The cascade decision is queued (a frame) because T2 depends on T1
    let pending = queued_cascade(&state).expect("the cascade decision is queued");
    assert_eq!(
        state
            .decisions
            .items()
            .next()
            .unwrap()
            .conversation_id
            .as_deref(),
        Some(conv.id.as_str()),
        "it waits in the conversation that raised it"
    );
    assert_eq!(pending.source_task, 1);
    assert_eq!(pending.source_prior_status, PlanTaskStatus::Pending);
    assert_eq!(pending.downstream, vec![2]);
}

#[test]
fn ac2_skip_non_skippable_emits_notice() {
    let plan = make_plan(
        PlanStatus::Executing,
        vec![make_task(1, "T1", PlanTaskStatus::Completed)],
    );
    let (mut conv, mut state) = make_conv(plan);

    let notices = handle_task_skip(&mut state, &mut conv, "test-plan", 1);
    assert!(
        notices
            .iter()
            .any(|n| n.message.contains("cannot be skipped"))
    );
    let plan = conv.plans.get("test-plan").unwrap();
    // Status must NOT change
    assert_eq!(plan.tasks[0].status, PlanTaskStatus::Completed);
}

// ── AC2: Skip cascade card persists prior status ─────────────────────────────

#[test]
fn ac2_skip_failed_preserves_prior_status_in_cascade() {
    let plan = make_plan(
        PlanStatus::Executing,
        vec![
            PlanTask {
                number: 1,
                title: "T1".to_string(),
                description: String::new(),
                depends_on: vec![],
                status: PlanTaskStatus::Failed,
                started_at_ms: None,
                completed_at_ms: Some(2000),
                result: None,
                error: Some("fail".into()),
                waiting_on: vec![],
                delegated_to: None,
                sub_tasks: vec![],
            },
            make_task_with_deps(2, "T2", PlanTaskStatus::Pending, vec![1]),
        ],
    );
    let (mut conv, mut state) = make_conv(plan);

    handle_task_skip(&mut state, &mut conv, "test-plan", 1);
    let pending = queued_cascade(&state).expect("the cascade decision is queued");
    assert_eq!(pending.source_prior_status, PlanTaskStatus::Failed);
}

// ── AC11: the status a task action is offered from ───────────────────────────
//
// old: `drill_down_task_status` read the drilled task's status from the panel state. The
// drill-down is deleted (A8/A17); the Tasks tab's rows carry the status of every task row
// (`TasksSource::status_of`, what the rail offers p/s/r/e from), and a ref names its task
// instead of a "currently drilled" cursor.

#[test]
fn ac11_tasks_source_returns_status_for_the_named_task() {
    let plan = make_plan(
        PlanStatus::Executing,
        vec![
            make_task(1, "T1", PlanTaskStatus::Completed),
            make_task(2, "T2", PlanTaskStatus::Failed),
            make_task(3, "T3", PlanTaskStatus::Paused),
        ],
    );
    let (conv, state) = make_conv(plan);
    let built = tasks::list(
        &conv,
        state.task_panel_state.last_executed_plan_id.as_deref(),
        None,
    );

    assert_eq!(
        built.source.status_of(&tasks::task_id("test-plan", 1)),
        Some(PlanTaskStatus::Completed)
    );
    assert_eq!(
        built.source.status_of(&tasks::task_id("test-plan", 2)),
        Some(PlanTaskStatus::Failed)
    );
    assert_eq!(
        built.source.status_of(&tasks::task_id("test-plan", 3)),
        Some(PlanTaskStatus::Paused)
    );
    // No such task, or a task of another plan: nothing.
    assert_eq!(
        built.source.status_of(&tasks::task_id("test-plan", 9)),
        None
    );
    assert_eq!(
        built.source.status_of(&tasks::task_id("other-plan", 1)),
        None
    );
    // No plan at all: no rows and nothing to act on.
    let empty = tasks::list(&Conversation::default(), None, None);
    assert!(empty.rows.is_empty());
    assert_eq!(
        empty.source.status_of(&tasks::task_id("test-plan", 1)),
        None
    );
}

// ── Helpers: dependents ──────────────────────────────────────────────────────

#[test]
fn dependents_no_deps() {
    let plan = make_plan(
        PlanStatus::Executing,
        vec![make_task(1, "T1", PlanTaskStatus::Pending)],
    );
    assert!(dependents(&plan, 1).is_empty());
}

#[test]
fn dependents_single_dep() {
    let plan = make_plan(
        PlanStatus::Executing,
        vec![
            make_task(1, "T1", PlanTaskStatus::Pending),
            make_task_with_deps(2, "T2", PlanTaskStatus::Pending, vec![1]),
        ],
    );
    assert_eq!(dependents(&plan, 1), vec![2]);
}

#[test]
fn dependents_transitive_chain() {
    let plan = make_plan(
        PlanStatus::Executing,
        vec![
            make_task(1, "T1", PlanTaskStatus::Pending),
            make_task_with_deps(2, "T2", PlanTaskStatus::Pending, vec![1]),
            make_task_with_deps(3, "T3", PlanTaskStatus::Pending, vec![2]),
        ],
    );
    let mut deps = dependents(&plan, 1);
    deps.sort();
    assert_eq!(deps, vec![2, 3]);
}

#[test]
fn dependents_cycle_defensive() {
    // depends_on should never form a cycle, but defensively
    let plan = make_plan(
        PlanStatus::Executing,
        vec![
            make_task_with_deps(1, "T1", PlanTaskStatus::Pending, vec![2]),
            make_task_with_deps(2, "T2", PlanTaskStatus::Pending, vec![1]),
        ],
    );
    // Should not hang — cycle is detected via seen set
    let deps = dependents(&plan, 1);
    assert_eq!(deps, vec![2]); // only T2 depends on T1; T1 is already seen
}

// ── Helpers: task numbers of the Tasks tab's rows ────────────────────────────
//
// old: `resolve_panel_task_number(state, conv, index)` mapped a sidebar index to a task
// number. A row now carries its id (`<plan>#<number>`), `parse_id` gives the number back
// and a row index past the end names nothing.

#[test]
fn task_numbers_of_the_rows_resolve_by_id() {
    let plan = make_plan(
        PlanStatus::Executing,
        vec![
            make_task(1, "T1", PlanTaskStatus::Pending),
            make_task(2, "T2", PlanTaskStatus::Pending),
        ],
    );
    let (conv, state) = make_conv(plan);
    let built = tasks::list(
        &conv,
        state.task_panel_state.last_executed_plan_id.as_deref(),
        None,
    );

    let number_at = |index: usize| {
        built
            .rows
            .get(index)
            .and_then(|row| tasks::parse_id(&row.id))
            .map(|(_, number)| number)
    };
    assert_eq!(number_at(0), Some(1));
    assert_eq!(number_at(1), Some(2));
    assert_eq!(number_at(99), None);
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
    let plan = make_plan(
        PlanStatus::Executing,
        vec![
            make_task(1, "T1", PlanTaskStatus::Pending),
            make_task(2, "T2", PlanTaskStatus::Pending),
        ],
    );
    assert!(validate_reorder(&plan, 1, 0).is_ok()); // at position 0 already
    assert!(validate_reorder(&plan, 1, 0).is_ok());
}

#[test]
fn ac4_validate_reorder_dep_violation() {
    use rustain::domain::services::plan_runtime::validate_reorder;
    let plan = make_plan(
        PlanStatus::Executing,
        vec![
            make_task(1, "T1", PlanTaskStatus::Pending),
            make_task_with_deps(2, "T2", PlanTaskStatus::Pending, vec![1]),
        ],
    );
    // Cannot push T2 before T1 (it depends on T1)
    assert!(validate_reorder(&plan, 2, 0).is_err());
}

#[test]
fn ac4_validate_reorder_only_pending() {
    use rustain::domain::services::plan_runtime::validate_reorder;
    let plan = make_plan(
        PlanStatus::Executing,
        vec![
            make_task(1, "T1", PlanTaskStatus::Completed),
            make_task(2, "T2", PlanTaskStatus::Pending),
        ],
    );
    assert!(validate_reorder(&plan, 1, 1).is_err()); // not Pending
}
