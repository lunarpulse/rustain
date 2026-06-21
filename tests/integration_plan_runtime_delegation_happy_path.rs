//! Integration test: Story 10.5 — PlanRuntime delegation happy path.
//!
//! Verifies that `PlanRuntime::delegate_task` successfully spawns a subagent,
//! marks the task as delegated, and emits the terminal event.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rustain::domain::events::AppEvent;
use rustain::domain::models::{
    AgentDef, Conversation, PermissionMode, Plan, PlanStatus, PlanTask, PlanTaskStatus,
    SubTaskFailurePolicy, generate_conversation_id,
};
use rustain::domain::ports::EventEmitter;
use rustain::domain::services::plan_runtime::PlanRuntime;
use rustain::infrastructure::subagent::SubagentSpool;

mod common;
use common::stub_subagent::StubSubagentRunner;

fn make_task(number: u32, title: &str, description: &str) -> PlanTask {
    PlanTask {
        number,
        title: title.to_string(),
        description: description.to_string(),
        depends_on: vec![],
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
        id: "plan-1".to_string(),
        title: "Test Plan".to_string(),
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

#[tokio::test]
async fn integration_plan_runtime_delegation_happy_path() {
    // Arrange: Plan with 1 task matching keyword
    let plan = make_plan(vec![make_task(
        1,
        "Review code",
        "Please review the authentication module for security issues",
    )]);
    let mut conv = make_conv_with_plan(plan);
    let conv_id = conv.id.clone();
    let plan_id = "plan-1".to_string();

    let runtime = PlanRuntime::new();

    // Start the plan to initialise runtime state, then reset task to Pending
    // so delegate_task can be exercised directly.
    runtime.clone().start(
        conv_id.clone(),
        plan_id.clone(),
        &mut conv,
        &CapturedEvents::new(),
        &[],
        PermissionMode::Normal,
        SubTaskFailurePolicy::default(),
    );
    {
        let plan = conv.plans.get_mut(&plan_id).unwrap();
        plan.tasks[0].status = PlanTaskStatus::Pending;
        plan.tasks[0].started_at_ms = None;
    }

    // Stub runner returning Completed
    let stub_runner = Arc::new(StubSubagentRunner::new(
        rustain::domain::models::NodeState::Completed,
        "Code review completed successfully",
    ));

    let spool_dir = tempfile::tempdir().unwrap().keep();
    let spool = Arc::new(SubagentSpool::new(spool_dir).await.unwrap());

    let agent = AgentDef {
        name: "code-reviewer".to_string(),
        description: "Security-focused code reviewer".to_string(),
        file: std::path::PathBuf::from("/dev/null"),
        model: None,
        allowed_tools: None,
        exclude_tools: None,
    };

    let emitter = CapturedEvents::new();

    // Act: delegate task 1
    let result = runtime
        .clone()
        .delegate_task(
            &conv_id,
            &plan_id,
            1,
            &agent,
            stub_runner,
            spool.clone(),
            &mut conv,
            emitter.clone(),
            "default-model",
        )
        .await;

    // Assert: delegation succeeded
    assert!(result.is_ok(), "delegate_task should succeed: {:?}", result);

    let plan = conv.plans.get(&plan_id).unwrap();
    let task = &plan.tasks[0];

    // Task shows delegated_to
    assert!(task.delegated_to.is_some(), "Task should show delegated_to");
    let info = task.delegated_to.as_ref().unwrap();
    assert_eq!(info.agent_name, "code-reviewer");
    assert!(info.agent_id.is_some());
    assert!(info.spool_task_id.is_some());

    // Status flipped to Running
    assert_eq!(task.status, PlanTaskStatus::Running);
    assert!(task.started_at_ms.is_some());

    // Wait for background task to emit terminal event
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let events = emitter.take();
    let has_completed = events.iter().any(|e| {
        matches!(
            e,
            AppEvent::PlanTaskDelegationCompleted { task_number: 1, .. }
        )
    });
    assert!(
        has_completed,
        "Should emit PlanTaskDelegationCompleted for task 1. Events: {:?}",
        events
    );
}
