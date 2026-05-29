//! Integration test: Story 10.5 — Parallel delegation with diamond deps (AC-10-5-8/12).
//!
//! Diamond dep graph: task 1 → tasks 2, 3 → task 4.
//! Two specialised agents match tasks 2 and 3 by description.
//! After task 1 completes locally, both 2 and 3 are eligible and should
//! receive `PlanTaskDelegationRequested` events via the fan-out path.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rustain::domain::events::AppEvent;
use rustain::domain::models::{
    AgentDef, Conversation, PermissionMode, Plan, PlanStatus, PlanTask, PlanTaskStatus, SubTaskFailurePolicy,
    generate_conversation_id,
};
use rustain::domain::ports::EventEmitter;
use rustain::domain::services::delegation_decider::DelegationDecider;
use rustain::domain::services::plan_runtime::{PlanRuntime, TaskTurnOutcome};

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

fn make_task(number: u32, title: &str, description: &str, depends_on: Vec<u32>) -> PlanTask {
    PlanTask {
        number,
        title: title.to_string(),
        description: description.to_string(),
        depends_on,
        status: PlanTaskStatus::Pending,
        started_at_ms: None,
        completed_at_ms: None,
        result: None,
        error: None,
        waiting_on: vec![],
        delegated_to: None,
    sub_tasks: vec![],
    }
}

fn make_plan(tasks: Vec<PlanTask>) -> Plan {
    Plan {
        id: "plan-diamond".to_string(),
        title: "Diamond Plan".to_string(),
        tasks,
        estimated_effort: None,
        status: PlanStatus::Executing,
        created_at: 1_700_000_000,
        resolved_at: Some(1_700_000_060),
        host_message_id: Some("msg-1".to_string()),
    }
}

fn make_conv_with_plan(plan: Plan) -> Conversation {
    let plan_id = plan.id.clone();
    let mut conv = Conversation {
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
    };
    conv.plans.insert(plan_id, plan);
    conv
}

#[test]
fn fan_out_bound_caps_at_nfr15() {
    assert_eq!(DelegationDecider::fan_out_bound(20, 15), 10);
    assert_eq!(DelegationDecider::fan_out_bound(2, 4), 2);
    assert_eq!(DelegationDecider::fan_out_bound(0, 4), 0);
}

#[tokio::test]
async fn diamond_delegation_requests_both_after_task1() {
    let plan = make_plan(vec![
        make_task(1, "Setup", "Initial setup task", vec![]),
        make_task(2, "Review auth", "Review authentication module for security issues", vec![1]),
        make_task(3, "Write tests", "Write unit tests for the api layer", vec![1]),
        make_task(4, "Finalize", "Final integration and cleanup", vec![2, 3]),
    ]);
    let mut conv = make_conv_with_plan(plan);
    let conv_id = conv.id.clone();
    let plan_id = "plan-diamond".to_string();

    let agents = vec![
        AgentDef {
            name: "code-reviewer".to_string(),
            description: "Security-focused code reviewer for authentication and authorization".to_string(),
            file: std::path::PathBuf::from("/dev/null"),
            model: None,
            allowed_tools: None,
            exclude_tools: None,
        },
        AgentDef {
            name: "test-writer".to_string(),
            description: "Unit test writer for api endpoints".to_string(),
            file: std::path::PathBuf::from("/dev/null"),
            model: None,
            allowed_tools: None,
            exclude_tools: None,
        },
    ];

    let emitter = CapturedEvents::new();
    let runtime = PlanRuntime::new();

    runtime.clone().start(
        conv_id.clone(),
        plan_id.clone(),
        &mut conv,
        emitter.as_ref(),
        &agents,
        PermissionMode::Yolo,
        SubTaskFailurePolicy::default(),
    );

    let events = emitter.take();
    let local_dispatches: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, AppEvent::AgentThenSubmit { .. }))
        .collect();
    assert_eq!(local_dispatches.len(), 1, "Task 1 should dispatch locally");

    let delegation_requests: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, AppEvent::PlanTaskDelegationRequested { .. }))
        .collect();
    assert_eq!(
        delegation_requests.len(), 0,
        "Task 1 has no matching agent (setup), so no delegation request"
    );

    let outcome = TaskTurnOutcome::Success {
        result_text: "Setup complete".to_string(),
        tool_call_count: 0,
        token_count: None,
    };
    runtime
        .on_turn_complete(&conv_id, &plan_id, 1, outcome, &mut conv, emitter.as_ref(), &agents, PermissionMode::Yolo)
        .await;

    let events_after = emitter.take();
    let delegation_reqs: Vec<u32> = events_after
        .iter()
        .filter_map(|e| match e {
            AppEvent::PlanTaskDelegationRequested { task_number, .. } => Some(*task_number),
            _ => None,
        })
        .collect();

    assert_eq!(
        delegation_reqs.len(), 2,
        "Both task 2 and task 3 should receive delegation requests via fan-out"
    );
    assert!(
        delegation_reqs.contains(&2),
        "Task 2 (code-reviewer match) should be requested"
    );
    assert!(
        delegation_reqs.contains(&3),
        "Task 3 (test-writer match) should be requested"
    );
}
