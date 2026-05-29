//! Cancel-race conformance helpers (Epic 6 retro AI-6.1).
//!
//! Captures the recurring pattern flagged in 3 of 8 Epic 6 stories:
//!
//!   *Cancel intent set on a token, but the observed task completes its natural
//!   turn (Success/Failure) before the cancel actually propagates. The intent is
//!   silently lost.*
//!
//! Source incidents:
//!   - 6-2a deferred D1 (DF-252) — `ToolCallTransitionBridged` race with `ToolResult`
//!   - 6-4 supplemental review — whole-plan-cancel race when task succeeds during
//!     the cancel window
//!   - 6-4 supplemental review — `pause_pending_tasks` entry leaks if Running task
//!     completes naturally (only the `Cancelled` branch drains the set)
//!
//! Why this lives outside `conformance_plan_runtime.rs`: Epic 16's streaming
//! reducer (16.2) interleaves `TextDelta` / `ToolCallStarted` / `ToolCallCompleted`
//! events that compose with parallel-safe tool execution from 6-0b. The same
//! race-shape will recur in the reducer; helpers in a dedicated file invite reuse
//! across `conformance_plan_runtime`, future `conformance_reducer`, and any
//! integration test that needs a deterministic race fixture.
//!
//! Helpers exported (call from any conformance test):
//!   - `make_minimal_plan(num_tasks)` — N-task plan in `Executing` state
//!   - `make_conv_with_plan(plan)` — `Conversation` carrying the plan
//!   - `add_assistant_msg(conv, text)` — synthetic Assistant message (advances
//!     the assistant-count cursor `PlanRuntime::start` reads from)
//!   - `CapturedEvents` — `EventEmitter` that records emissions for assertion

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rustain::domain::events::AppEvent;
use rustain::domain::models::{
    ChatMessage, Conversation, MessageRole, PermissionMode, Plan, PlanStatus, PlanTask,
    PlanTaskStatus, generate_conversation_id, generate_message_id,
};
use rustain::domain::ports::EventEmitter;
use rustain::domain::services::plan_runtime::{PlanRuntime, TaskTurnOutcome};

// -------------------- Helpers (pub for cross-test reuse) --------------------

pub fn make_task(number: u32, title: &str) -> PlanTask {
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
        delegated_to: None,
    }
}

pub fn make_minimal_plan(num_tasks: u32) -> Plan {
    let tasks = (1..=num_tasks)
        .map(|n| make_task(n, &format!("Task {n}")))
        .collect();
    Plan {
        id: "race-plan".to_string(),
        title: "Race Test Plan".to_string(),
        tasks,
        estimated_effort: None,
        status: PlanStatus::Executing,
        created_at: 1_700_000_000,
        resolved_at: Some(1_700_000_060),
        host_message_id: Some("msg-1".to_string()),
    }
}

pub fn make_conv_with_plan(plan: Plan) -> Conversation {
    let plan_id = plan.id.clone();
    let mut conv = Conversation {
        id: generate_conversation_id(),
        title: "Race Test".to_string(),
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

pub fn add_assistant_msg(conv: &mut Conversation, text: &str) {
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

pub struct CapturedEvents {
    events: Mutex<Vec<AppEvent>>,
}

impl CapturedEvents {
    pub fn new() -> Arc<Self> {
        Arc::new(CapturedEvents {
            events: Mutex::new(Vec::new()),
        })
    }

    pub fn take(&self) -> Vec<AppEvent> {
        self.events.lock().unwrap().drain(..).collect()
    }
}

impl EventEmitter for CapturedEvents {
    fn emit(&self, event: AppEvent) {
        self.events.lock().unwrap().push(event);
    }
}

// -------------------- Tests --------------------

/// Race shape: `mark_whole_plan_cancel_pending` set; Running task completes via
/// `TaskTurnOutcome::Success` before the cancel token propagates. Plan must end
/// in `Cancelled`, not `Completed`.
///
/// Caught at 6-4 supplemental review (2026-04-28). Patch landed in `on_turn_complete`
/// Success branch.
#[tokio::test]
async fn whole_plan_cancel_intent_honored_on_natural_success() {
    let plan = make_minimal_plan(3);
    let plan_id = plan.id.clone();
    let mut conv = make_conv_with_plan(plan);
    let captured = CapturedEvents::new();
    let runtime = PlanRuntime::new();
    let conv_id = conv.id.clone();

    runtime.clone().start(
        conv_id.clone(),
        plan_id.clone(),
        &mut conv,
        captured.as_ref(),
        &[],
        PermissionMode::Normal,
    );

    // Task 1 is now Running. User invokes `!cancel-plan`.
    runtime.mark_whole_plan_cancel_pending(&plan_id).await;

    // Race: the task completes naturally before the cancel token has propagated
    // to the executing turn.
    add_assistant_msg(&mut conv, "Done.");
    runtime
        .on_turn_complete(
            &conv_id,
            &plan_id,
            1,
            TaskTurnOutcome::Success {
                result_text: "Done.".to_string(),
                tool_call_count: 0,
                token_count: Some(10),
            },
            &mut conv,
            captured.as_ref(),
            &[],
            PermissionMode::Normal,
        )
        .await;

    // Cancel intent must win — plan is Cancelled, not Completed.
    let plan = &conv.plans[&plan_id];
    assert_eq!(
        plan.status,
        PlanStatus::Cancelled,
        "whole_plan_cancel_pending must override natural Success completion"
    );
    // Tasks 2 and 3 must be Cancelled (not Pending).
    assert_eq!(plan.tasks[1].status, PlanTaskStatus::Cancelled);
    assert_eq!(plan.tasks[2].status, PlanTaskStatus::Cancelled);

    // PlanCancelled event must be emitted.
    let events = captured.take();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AppEvent::PlanCancelled { .. })),
        "PlanCancelled event missing — cancel intent silently dropped"
    );
}

/// Race shape: `mark_whole_plan_cancel_pending` set; Running task fails via
/// `TaskTurnOutcome::Failure` before the cancel token propagates. Same expected
/// outcome as the Success variant — cancel wins, plan ends in `Cancelled`.
#[tokio::test]
async fn whole_plan_cancel_intent_honored_on_natural_failure() {
    let plan = make_minimal_plan(2);
    let plan_id = plan.id.clone();
    let mut conv = make_conv_with_plan(plan);
    let captured = CapturedEvents::new();
    let runtime = PlanRuntime::new();
    let conv_id = conv.id.clone();

    runtime.clone().start(
        conv_id.clone(),
        plan_id.clone(),
        &mut conv,
        captured.as_ref(),
        &[],
        PermissionMode::Normal,
    );

    runtime.mark_whole_plan_cancel_pending(&plan_id).await;

    add_assistant_msg(&mut conv, "Error.");
    runtime
        .on_turn_complete(
            &conv_id,
            &plan_id,
            1,
            TaskTurnOutcome::Failure {
                error: "tool execution failed".to_string(),
            },
            &mut conv,
            captured.as_ref(),
            &[],
            PermissionMode::Normal,
        )
        .await;

    let plan = &conv.plans[&plan_id];
    assert_eq!(plan.status, PlanStatus::Cancelled);
    assert_eq!(plan.tasks[1].status, PlanTaskStatus::Cancelled);
}

/// Race shape: `mark_pause_pending(task)` set; Running task completes via
/// `TaskTurnOutcome::Success` before the pause cancel propagates. The task ends
/// `Completed` (success was real), but the `pause_pending_tasks` entry must be
/// **drained** — otherwise it leaks into the HashSet for the lifetime of the plan
/// state and any future pause check on this task number is fooled.
///
/// Flagged in 6-4 supplemental review as patched checked-box, but as of
/// 2026-04-28 only the `Cancelled` branch drains the set. This test pins the
/// expected behavior; the source fix lives alongside it.
#[tokio::test]
async fn pause_pending_drained_after_natural_success() {
    let plan = make_minimal_plan(2);
    let plan_id = plan.id.clone();
    let mut conv = make_conv_with_plan(plan);
    let captured = CapturedEvents::new();
    let runtime = PlanRuntime::new();
    let conv_id = conv.id.clone();

    runtime.clone().start(
        conv_id.clone(),
        plan_id.clone(),
        &mut conv,
        captured.as_ref(),
        &[],
        PermissionMode::Normal,
    );

    runtime.mark_pause_pending(&plan_id, 1).await;
    // Pre-condition: entry is present.
    assert!(
        runtime
            .snapshot(&plan_id)
            .await
            .unwrap()
            .pause_pending_tasks
            .contains(&1)
    );

    add_assistant_msg(&mut conv, "Done.");
    runtime
        .on_turn_complete(
            &conv_id,
            &plan_id,
            1,
            TaskTurnOutcome::Success {
                result_text: "Done.".to_string(),
                tool_call_count: 0,
                token_count: Some(10),
            },
            &mut conv,
            captured.as_ref(),
            &[],
            PermissionMode::Normal,
        )
        .await;

    // Task succeeded — that's correct.
    assert_eq!(
        conv.plans[&plan_id].tasks[0].status,
        PlanTaskStatus::Completed
    );
    // The pause-pending entry must NOT leak.
    let snapshot = runtime
        .snapshot(&plan_id)
        .await
        .expect("plan state present");
    assert!(
        !snapshot.pause_pending_tasks.contains(&1),
        "pause_pending_tasks entry for task 1 leaked into Success branch — \
         only the Cancelled branch drains the set today"
    );
}

/// Same race as above, but task fails naturally instead of succeeding.
/// Same drain requirement.
#[tokio::test]
async fn pause_pending_drained_after_natural_failure() {
    let plan = make_minimal_plan(2);
    let plan_id = plan.id.clone();
    let mut conv = make_conv_with_plan(plan);
    let captured = CapturedEvents::new();
    let runtime = PlanRuntime::new();
    let conv_id = conv.id.clone();

    runtime.clone().start(
        conv_id.clone(),
        plan_id.clone(),
        &mut conv,
        captured.as_ref(),
        &[],
        PermissionMode::Normal,
    );

    runtime.mark_pause_pending(&plan_id, 1).await;

    add_assistant_msg(&mut conv, "Error.");
    runtime
        .on_turn_complete(
            &conv_id,
            &plan_id,
            1,
            TaskTurnOutcome::Failure {
                error: "tool execution failed".to_string(),
            },
            &mut conv,
            captured.as_ref(),
            &[],
            PermissionMode::Normal,
        )
        .await;

    assert_eq!(conv.plans[&plan_id].tasks[0].status, PlanTaskStatus::Failed);
    let snapshot = runtime
        .snapshot(&plan_id)
        .await
        .expect("plan state present");
    assert!(
        !snapshot.pause_pending_tasks.contains(&1),
        "pause_pending_tasks entry for task 1 leaked into Failure branch"
    );
}

/// G6-P20 protection (6-2a review): `on_turn_complete` must validate that
/// `task_number` matches the currently `Running` task. A wrong number must not
/// corrupt a different task's status.
///
/// This is the symmetric race: not "intent dropped on natural completion" but
/// "stale completion event applied to wrong target." Same risk profile for
/// Epic 16's reducer when `ToolCallCompleted` arrives after the invocation has
/// already been replaced by reorder/retry.
#[tokio::test]
async fn on_turn_complete_ignores_mismatched_task_number() {
    let plan = make_minimal_plan(3);
    let plan_id = plan.id.clone();
    let mut conv = make_conv_with_plan(plan);
    let captured = CapturedEvents::new();
    let runtime = PlanRuntime::new();
    let conv_id = conv.id.clone();

    runtime.clone().start(
        conv_id.clone(),
        plan_id.clone(),
        &mut conv,
        captured.as_ref(),
        &[],
        PermissionMode::Normal,
    );

    // Running is task 1. Stale event arrives for task 3.
    runtime
        .on_turn_complete(
            &conv_id,
            &plan_id,
            3,
            TaskTurnOutcome::Success {
                result_text: "stale".to_string(),
                tool_call_count: 0,
                token_count: None,
            },
            &mut conv,
            captured.as_ref(),
            &[],
            PermissionMode::Normal,
        )
        .await;

    // Task 3 must NOT have been mutated to Completed.
    assert_eq!(
        conv.plans[&plan_id].tasks[2].status,
        PlanTaskStatus::Pending
    );
    // Task 1 (the actual Running task) is untouched.
    assert_eq!(
        conv.plans[&plan_id].tasks[0].status,
        PlanTaskStatus::Running
    );
}
