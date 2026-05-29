//! Integration test: Story 10.5 — Delegation summary aggregation (AC-10-5-11/12).
//!
//! 3-task plan where task 2 delegates to "code-reviewer".
//! After all tasks complete, verifies:
//! - PlanSummary ChatMessage contains "Plan complete:", "3/3 tasks completed",
//!   and the delegated result text
//! - plan.tasks[1].delegated_to.agent_name == "code-reviewer"

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rustain::domain::events::AppEvent;
use rustain::domain::models::{
    AgentDef, Conversation, PermissionMode, Plan, PlanStatus, PlanTask, PlanTaskStatus,
    generate_conversation_id,
};
use rustain::domain::ports::EventEmitter;
use rustain::domain::services::plan_runtime::{PlanRuntime, TaskTurnOutcome};
use rustain::infrastructure::subagent::SubagentSpool;

mod common;
use common::stub_subagent::StubSubagentRunner;

struct CapturedEvents {
    events: Mutex<Vec<AppEvent>>,
}

impl CapturedEvents {
    fn new() -> Arc<Self> {
        Arc::new(CapturedEvents {
            events: Mutex::new(Vec::new()),
        })
    }
}

impl EventEmitter for CapturedEvents {
    fn emit(&self, event: AppEvent) {
        self.events.lock().unwrap().push(event);
    }
}

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
    }
}

fn make_plan(tasks: Vec<PlanTask>) -> Plan {
    Plan {
        id: "plan-summary".to_string(),
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

#[tokio::test]
async fn integration_plan_runtime_delegation_summary() {
    let plan = make_plan(vec![
        make_task(1, "Setup", "Initial setup"),
        make_task(2, "Review auth", "Review authentication module"),
        make_task(3, "Cleanup", "Final cleanup"),
    ]);
    let mut conv = make_conv_with_plan(plan);
    let conv_id = conv.id.clone();
    let plan_id = "plan-summary".to_string();

    let runtime = PlanRuntime::new();
    let emitter = CapturedEvents::new();

    runtime.clone().start(
        conv_id.clone(),
        plan_id.clone(),
        &mut conv,
        emitter.as_ref(),
        &[],
        PermissionMode::Normal,
    );

    let plan = conv.plans.get_mut(&plan_id).unwrap();
    plan.tasks[0].status = PlanTaskStatus::Pending;
    plan.tasks[0].started_at_ms = None;
    plan.tasks[1].status = PlanTaskStatus::Pending;
    plan.tasks[1].started_at_ms = None;
    plan.tasks[2].status = PlanTaskStatus::Pending;
    plan.tasks[2].started_at_ms = None;

    let stub_runner = Arc::new(StubSubagentRunner::new(
        rustain::domain::models::SubagentRunStatus::Completed,
        "Delegated result for task 2",
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

    runtime
        .clone()
        .delegate_task(
            &conv_id,
            &plan_id,
            2,
            &agent,
            stub_runner,
            spool,
            &mut conv,
            emitter.clone(),
            "claude-sonnet-4-6",
        )
        .await
        .unwrap();

    let outcome_t2 = TaskTurnOutcome::Success {
        result_text: "Delegated result for task 2".to_string(),
        tool_call_count: 0,
        token_count: None,
    };
    runtime
        .on_turn_complete(&conv_id, &plan_id, 2, outcome_t2, &mut conv, emitter.as_ref(), &[], PermissionMode::Normal)
        .await;

    {
        let plan = conv.plans.get(&plan_id).unwrap();
        assert_eq!(plan.tasks[1].status, PlanTaskStatus::Completed);
        assert_eq!(
            plan.tasks[1].delegated_to.as_ref().unwrap().agent_name,
            "code-reviewer"
        );
    }

    let outcome_t1 = TaskTurnOutcome::Success {
        result_text: "Setup done".to_string(),
        tool_call_count: 0,
        token_count: None,
    };
    {
        let plan = conv.plans.get_mut(&plan_id).unwrap();
        plan.tasks[0].status = PlanTaskStatus::Running;
        plan.tasks[0].started_at_ms = Some(chrono::Utc::now().timestamp_millis());
    }
    runtime
        .on_turn_complete(&conv_id, &plan_id, 1, outcome_t1, &mut conv, emitter.as_ref(), &[], PermissionMode::Normal)
        .await;

    let outcome_t3 = TaskTurnOutcome::Success {
        result_text: "Cleanup done".to_string(),
        tool_call_count: 0,
        token_count: None,
    };
    {
        let plan = conv.plans.get_mut(&plan_id).unwrap();
        plan.tasks[2].status = PlanTaskStatus::Running;
        plan.tasks[2].started_at_ms = Some(chrono::Utc::now().timestamp_millis());
    }
    runtime
        .on_turn_complete(&conv_id, &plan_id, 3, outcome_t3, &mut conv, emitter.as_ref(), &[], PermissionMode::Normal)
        .await;

    let summary_msg = conv.messages.iter().find(|m| {
        m.content.contains("Plan complete:")
    });
    assert!(
        summary_msg.is_some(),
        "A PlanSummary ChatMessage should be added to conversation.messages"
    );
    let content = &summary_msg.unwrap().content;
    assert!(
        content.contains("3/3 tasks completed"),
        "Summary should show 3/3 tasks completed, got: {}",
        content
    );
    assert!(
        content.contains("Delegated result for task 2"),
        "Summary should include the delegated result text, got: {}",
        content
    );
}
