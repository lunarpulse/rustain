//! Conformance tests for Story 6-2a: Sequential Task Execution & Dependencies.
//!
//! Validates PlanRuntime sequential walk, per-task status FSM, dependency blocking,
//! failure cascade, cancellation, unified summary generation, event ordering,
//! and serde backward compatibility.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rustain::domain::events::AppEvent;
use rustain::domain::models::{
    ChatMessage, ContentBlockType, Conversation, MessageRole, NoticeLevel,
    Plan, PlanStatus, PlanTask, PlanTaskStatus, TaskResult,
    generate_conversation_id, generate_message_id,
};
use rustain::domain::ports::EventEmitter;
use rustain::domain::services::plan_runtime::{PlanRuntime, TaskTurnOutcome};

fn make_task(number: u32, title: &str) -> PlanTask {
    PlanTask {
        number,
        title: title.to_string(),
        description: String::new(),
        depends_on: vec![],
        status: PlanTaskStatus::Pending,
        started_at_ms: None,
        completed_at_ms: None,
        result: None,
        error: None,
        waiting_on: vec![],
    }
}

fn make_task_with_deps(number: u32, title: &str, deps: Vec<u32>) -> PlanTask {
    PlanTask {
        number,
        title: title.to_string(),
        description: String::new(),
        depends_on: deps,
        status: PlanTaskStatus::Pending,
        started_at_ms: None,
        completed_at_ms: None,
        result: None,
        error: None,
        waiting_on: vec![],
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

fn make_plan(tasks: Vec<PlanTask>) -> Plan {
    Plan {
        id: "test-plan-id".to_string(),
        title: "Test Plan".to_string(),
        tasks,
        estimated_effort: None,
        status: PlanStatus::Executing,
        created_at: 1_700_000_000,
        resolved_at: Some(1_700_000_060),
        host_message_id: Some("msg-1".to_string()),
    }
}

struct CapturedEvents {
    events: Mutex<Vec<AppEvent>>,
}

impl CapturedEvents {
    fn new() -> Arc<Self> {
        Arc::new(CapturedEvents {
            events: Mutex::new(Vec::new()),
        })
    }

    fn take(&self) -> Vec<AppEvent> {
        self.events.lock().unwrap().drain(..).collect()
    }
}

impl EventEmitter for CapturedEvents {
    fn emit(&self, event: AppEvent) {
        self.events.lock().unwrap().push(event);
    }
}

fn add_assistant_msg(conv: &mut Conversation, text: &str) {
    conv.messages.push(ChatMessage {
        id: generate_message_id(),
        role: MessageRole::Assistant,
        content: text.to_string(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: 1_700_000_100,
        token_count: Some(50),
        stop_reason: Some(rustain::domain::models::StopReason::EndTurn),
        synthetic: true,
        images: vec![],
    });
}

#[tokio::test]
async fn ac1_runtime_dispatches_first_task() {
    let plan = make_plan(vec![
        make_task(1, "Setup"),
        make_task(2, "Build"),
        make_task(3, "Test"),
    ]);
    let plan_id = plan.id.clone();
    let conv_id = {
        let mut conv = make_conv_with_plan(plan);
        let conv_id = conv.id.clone();
        let captured = CapturedEvents::new();
        let runtime = PlanRuntime::new();
        runtime.clone().start(conv_id.clone(), plan_id.clone(), &mut conv, captured.as_ref());

        assert_eq!(conv.plans[&plan_id].tasks[0].status, PlanTaskStatus::Running);
        assert!(conv.plans[&plan_id].tasks[0].started_at_ms.is_some());
        assert_eq!(conv.plans[&plan_id].tasks[1].status, PlanTaskStatus::Pending);
        assert_eq!(conv.plans[&plan_id].tasks[2].status, PlanTaskStatus::Pending);

        let events = captured.take();
        assert!(events.iter().any(|e| matches!(e, AppEvent::AgentThenSubmit { synthetic: true, .. })));
        assert!(events.iter().any(|e| matches!(e, AppEvent::PlanTaskStatusChanged { status: PlanTaskStatus::Running, .. })));
        conv_id
    };
    let _ = (conv_id, plan_id);
}

#[tokio::test]
async fn ac2_per_task_turn_records_result() {
    let plan = make_plan(vec![
        make_task(1, "Setup"),
        make_task(2, "Build"),
    ]);
    let plan_id = plan.id.clone();
    let mut conv = make_conv_with_plan(plan);
    let captured = CapturedEvents::new();
    let runtime = PlanRuntime::new();
    let conv_id = conv.id.clone();

    runtime.clone().start(conv_id.clone(), plan_id.clone(), &mut conv, captured.as_ref());
    add_assistant_msg(&mut conv, "Setup complete. Files created.");

    runtime.on_turn_complete(
        &conv_id,
        &plan_id,
        1,
        TaskTurnOutcome::Success {
            result_text: "Setup complete.".to_string(),
            tool_call_count: 2,
            token_count: Some(50),
        },
        &mut conv,
        captured.as_ref(),
    ).await;

    let task = &conv.plans[&plan_id].tasks[0];
    assert_eq!(task.status, PlanTaskStatus::Completed);
    assert!(task.completed_at_ms.is_some());
    assert!(task.result.is_some());
    let result = task.result.as_ref().unwrap();
    assert_eq!(result.text, "Setup complete.");
    assert_eq!(result.tool_call_count, 2);
    assert_eq!(result.token_count, Some(50));

    let task2 = &conv.plans[&plan_id].tasks[1];
    assert_eq!(task2.status, PlanTaskStatus::Running);
}

#[tokio::test]
async fn ac3_dependency_blocks_downstream_on_failure() {
    let plan = make_plan(vec![
        make_task(1, "Setup"),
        make_task_with_deps(2, "Build", vec![1]),
        make_task_with_deps(3, "Test", vec![2]),
        make_task_with_deps(4, "Deploy", vec![1]),
    ]);
    let plan_id = plan.id.clone();
    let mut conv = make_conv_with_plan(plan);
    let captured = CapturedEvents::new();
    let runtime = PlanRuntime::new();
    let conv_id = conv.id.clone();

    runtime.clone().start(conv_id.clone(), plan_id.clone(), &mut conv, captured.as_ref());

    runtime.on_turn_complete(
        &conv_id, &plan_id, 1,
        TaskTurnOutcome::Success { result_text: "done".into(), tool_call_count: 0, token_count: None },
        &mut conv, captured.as_ref(),
    ).await;

    runtime.on_turn_complete(
        &conv_id, &plan_id, 2,
        TaskTurnOutcome::Failure { error: "Build failed".into() },
        &mut conv, captured.as_ref(),
    ).await;

    assert_eq!(conv.plans[&plan_id].tasks[2].status, PlanTaskStatus::Skipped);
    assert!(conv.plans[&plan_id].tasks[2].error.as_ref().unwrap().contains("failed task 2"));

    // Task 4 depends on task 1 (completed), so it gets dispatched next
    assert_eq!(conv.plans[&plan_id].tasks[3].status, PlanTaskStatus::Running);

    assert_eq!(conv.plans[&plan_id].tasks[0].status, PlanTaskStatus::Completed);

    // Drive task 4 to completion — plan should now finish
    runtime.on_turn_complete(
        &conv_id, &plan_id, 4,
        TaskTurnOutcome::Success { result_text: "deployed".into(), tool_call_count: 0, token_count: None },
        &mut conv, captured.as_ref(),
    ).await;

    assert_eq!(conv.plans[&plan_id].tasks[3].status, PlanTaskStatus::Completed);
    assert_eq!(conv.plans[&plan_id].status, PlanStatus::Completed);
    let events = captured.take();
    assert!(events.iter().any(|e| matches!(e, AppEvent::PlanCompleted { .. })));
}

#[tokio::test]
async fn ac3_dependency_chain_full_skip() {
    let plan = make_plan(vec![
        make_task(1, "Setup"),
        make_task_with_deps(2, "Build", vec![1]),
        make_task_with_deps(3, "Test", vec![2]),
        make_task_with_deps(4, "Deploy", vec![3]),
    ]);
    let plan_id = plan.id.clone();
    let mut conv = make_conv_with_plan(plan);
    let captured = CapturedEvents::new();
    let runtime = PlanRuntime::new();
    let conv_id = conv.id.clone();

    runtime.clone().start(conv_id.clone(), plan_id.clone(), &mut conv, captured.as_ref());

    runtime.on_turn_complete(
        &conv_id, &plan_id, 1,
        TaskTurnOutcome::Failure { error: "Setup failed".into() },
        &mut conv, captured.as_ref(),
    ).await;

    assert_eq!(conv.plans[&plan_id].tasks[1].status, PlanTaskStatus::Skipped);
    assert_eq!(conv.plans[&plan_id].tasks[2].status, PlanTaskStatus::Skipped);
    assert_eq!(conv.plans[&plan_id].tasks[3].status, PlanTaskStatus::Skipped);
}

#[tokio::test]
async fn ac4_serde_round_trip_with_new_fields() {
    let task = PlanTask {
        number: 1,
        title: "Task".to_string(),
        description: "desc".to_string(),
        depends_on: vec![2],
        status: PlanTaskStatus::Completed,
        started_at_ms: Some(1000),
        completed_at_ms: Some(5000),
        result: Some(TaskResult {
            text: "done".to_string(),
            tool_call_count: 3,
            token_count: Some(100),
        }),
        error: None,
        waiting_on: vec![],
    };
    let json = serde_json::to_string(&task).unwrap();
    let back: PlanTask = serde_json::from_str(&json).unwrap();
    assert_eq!(back, task);

    let old_json = r#"{"number":1,"title":"Old","description":"","dependsOn":[],"status":"pending"}"#;
    let old_task: PlanTask = serde_json::from_str(old_json).unwrap();
    assert_eq!(old_task.started_at_ms, None);
    assert_eq!(old_task.result, None);
    assert_eq!(old_task.status, PlanTaskStatus::Pending);
}

#[tokio::test]
async fn ac5_skipped_emits_warning_notice() {
    let plan = make_plan(vec![
        make_task(1, "Setup"),
        make_task_with_deps(2, "Build", vec![1]),
    ]);
    let plan_id = plan.id.clone();
    let mut conv = make_conv_with_plan(plan);
    let captured = CapturedEvents::new();
    let runtime = PlanRuntime::new();
    let conv_id = conv.id.clone();

    runtime.clone().start(conv_id.clone(), plan_id.clone(), &mut conv, captured.as_ref());

    runtime.on_turn_complete(
        &conv_id, &plan_id, 1,
        TaskTurnOutcome::Failure { error: "failed".into() },
        &mut conv, captured.as_ref(),
    ).await;

    let events = captured.take();
    let notices: Vec<_> = events.iter().filter_map(|e| match e {
        AppEvent::SystemNotice { level: NoticeLevel::Warning, message, .. } => Some(message.clone()),
        _ => None,
    }).collect();
    assert!(notices.iter().any(|m| m.contains("Auto-skipped task 2")));
}

#[tokio::test]
async fn ac6_summary_message_appended() {
    let plan = make_plan(vec![make_task(1, "Setup")]);
    let plan_id = plan.id.clone();
    let mut conv = make_conv_with_plan(plan);
    let captured = CapturedEvents::new();
    let runtime = PlanRuntime::new();
    let conv_id = conv.id.clone();

    runtime.clone().start(conv_id.clone(), plan_id.clone(), &mut conv, captured.as_ref());

    runtime.on_turn_complete(
        &conv_id, &plan_id, 1,
        TaskTurnOutcome::Success { result_text: "done".into(), tool_call_count: 0, token_count: None },
        &mut conv, captured.as_ref(),
    ).await;

    let last_msg = conv.messages.last().unwrap();
    assert!(last_msg.synthetic);
    assert!(last_msg.content_blocks.contains(&ContentBlockType::PlanSummary));
    assert!(last_msg.content.contains("Plan complete:"));
    assert!(last_msg.content.contains("completed"));
    assert!(last_msg.content.contains("| # | Task |"));
}

#[tokio::test]
async fn ac6_summary_aggregation_math() {
    let plan = make_plan(vec![
        make_task(1, "Setup"),
        make_task(2, "Build"),
        make_task_with_deps(3, "Test", vec![2]),
    ]);
    let plan_id = plan.id.clone();
    let mut conv = make_conv_with_plan(plan);
    let captured = CapturedEvents::new();
    let runtime = PlanRuntime::new();
    let conv_id = conv.id.clone();

    runtime.clone().start(conv_id.clone(), plan_id.clone(), &mut conv, captured.as_ref());

    runtime.on_turn_complete(
        &conv_id, &plan_id, 1,
        TaskTurnOutcome::Success { result_text: "done1".into(), tool_call_count: 1, token_count: Some(50) },
        &mut conv, captured.as_ref(),
    ).await;

    runtime.on_turn_complete(
        &conv_id, &plan_id, 2,
        TaskTurnOutcome::Failure { error: "build error".into() },
        &mut conv, captured.as_ref(),
    ).await;

    let last_msg = conv.messages.last().unwrap();
    let content = &last_msg.content;
    assert!(content.contains("1/3 tasks completed"));
    assert!(content.contains("1 failed"));
    assert!(content.contains("1 skipped"));
}

#[tokio::test]
async fn ac7_events_fire_in_order() {
    let plan = make_plan(vec![make_task(1, "Setup"), make_task(2, "Build")]);
    let plan_id = plan.id.clone();
    let mut conv = make_conv_with_plan(plan);
    let captured = CapturedEvents::new();
    let runtime = PlanRuntime::new();
    let conv_id = conv.id.clone();

    runtime.clone().start(conv_id.clone(), plan_id.clone(), &mut conv, captured.as_ref());

    runtime.on_turn_complete(
        &conv_id, &plan_id, 1,
        TaskTurnOutcome::Success { result_text: "done1".into(), tool_call_count: 0, token_count: None },
        &mut conv, captured.as_ref(),
    ).await;

    runtime.on_turn_complete(
        &conv_id, &plan_id, 2,
        TaskTurnOutcome::Success { result_text: "done2".into(), tool_call_count: 0, token_count: None },
        &mut conv, captured.as_ref(),
    ).await;

    let events = captured.take();
    let status_events: Vec<_> = events.iter().filter_map(|e| match e {
        AppEvent::PlanTaskStatusChanged { task_number, status, .. } => Some((*task_number, *status)),
        _ => None,
    }).collect();

    assert!(status_events.contains(&(1, PlanTaskStatus::Running)));
    assert!(status_events.contains(&(1, PlanTaskStatus::Completed)));
    assert!(status_events.contains(&(2, PlanTaskStatus::Running)));
    assert!(status_events.contains(&(2, PlanTaskStatus::Completed)));

    let running1_pos = status_events.iter().position(|(n, s)| *n == 1 && *s == PlanTaskStatus::Running).unwrap();
    let completed1_pos = status_events.iter().position(|(n, s)| *n == 1 && *s == PlanTaskStatus::Completed).unwrap();
    let running2_pos = status_events.iter().position(|(n, s)| *n == 2 && *s == PlanTaskStatus::Running).unwrap();
    assert!(running1_pos < completed1_pos);
    assert!(completed1_pos < running2_pos);

    assert!(events.iter().any(|e| matches!(e, AppEvent::PlanCompleted { .. })));
}

#[tokio::test]
async fn ac8_failure_heuristic_conservative() {
    let success = PlanRuntime::classify_outcome("Updated 4 files. Tests pass.", 2, true, None);
    assert!(matches!(success, TaskTurnOutcome::Success { .. }));

    let failure = PlanRuntime::classify_outcome("I cannot find the auth module.", 0, false, None);
    assert!(matches!(failure, TaskTurnOutcome::Failure { .. }));

    let success_with_error = PlanRuntime::classify_outcome(
        "Tried to read but encountered an error. Cached version worked.",
        1, true, None,
    );
    assert!(matches!(success_with_error, TaskTurnOutcome::Success { .. }));
}

#[tokio::test]
async fn ac9_cancelled_task_stops_walk() {
    let plan = make_plan(vec![
        make_task(1, "Setup"),
        make_task(2, "Build"),
        make_task(3, "Test"),
        make_task(4, "Deploy"),
    ]);
    let plan_id = plan.id.clone();
    let mut conv = make_conv_with_plan(plan);
    let captured = CapturedEvents::new();
    let runtime = PlanRuntime::new();
    let conv_id = conv.id.clone();

    runtime.clone().start(conv_id.clone(), plan_id.clone(), &mut conv, captured.as_ref());

    runtime.on_turn_complete(
        &conv_id, &plan_id, 1,
        TaskTurnOutcome::Success { result_text: "done".into(), tool_call_count: 0, token_count: None },
        &mut conv, captured.as_ref(),
    ).await;

    runtime.on_turn_complete(
        &conv_id, &plan_id, 2,
        TaskTurnOutcome::Cancelled { reason: "turn-cancelled".into() },
        &mut conv, captured.as_ref(),
    ).await;

    assert_eq!(conv.plans[&plan_id].tasks[0].status, PlanTaskStatus::Completed);
    assert_eq!(conv.plans[&plan_id].tasks[1].status, PlanTaskStatus::Cancelled);
    assert_eq!(conv.plans[&plan_id].tasks[2].status, PlanTaskStatus::Pending);
    assert_eq!(conv.plans[&plan_id].tasks[3].status, PlanTaskStatus::Pending);
    assert_eq!(conv.plans[&plan_id].status, PlanStatus::Cancelled);

    let events = captured.take();
    assert!(events.iter().any(|e| matches!(e, AppEvent::PlanCancelled { .. })));
    assert!(!events.iter().any(|e| matches!(e, AppEvent::PlanCompleted { .. })));
    assert!(!conv.messages.iter().any(|m| m.content_blocks.contains(&ContentBlockType::PlanSummary)));
}
