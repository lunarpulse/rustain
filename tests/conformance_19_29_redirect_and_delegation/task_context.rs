//! Story Q-6 AC3 (K3) — delegated Task context is bound to the originating call.
//!
//! Front door: `ToolSetPort::execute_invocation` on the real
//! `CompositeToolsetAdapter` → real `SubagentProvider::invoke_task` → the launch
//! and budget guard, with the recording runner as the observer of every launch
//! the production path builds (`rail_harness`). The context under test is the one
//! the scheduler stamps on each call's `ToolInvocation` (its hand-off is covered
//! by `tool_scheduler`'s own unit tests); there is no adapter slot left to read.
//!
//! The synthetic budget works through the production guard: the stub router
//! reports no model, so the window defaults to 200 000 tokens and a parent context
//! above half of what remains (here > 5 000 of 10 000) is refused.

use rustain::domain::models::{ToolInvocation, ToolResult, TraceContext};
use rustain::domain::ports::ToolSetPort;
use tokio_util::sync::CancellationToken;

use super::rail_harness::{RailStack, rail_stack};

const OVER_BUDGET: u32 = 190_000;
const WITHIN_BUDGET: u32 = 1_000;

fn trace(fill: char) -> TraceContext {
    TraceContext::new(fill.to_string().repeat(32), fill.to_string().repeat(16), 1).unwrap()
}

fn invocation<'a>(
    conversation_id: &'a str,
    parent_ctx_tokens: u32,
    parent_trace: Option<&'a TraceContext>,
) -> ToolInvocation<'a> {
    ToolInvocation {
        conversation_id,
        route: None,
        permission_resolution: None,
        agent_restriction: None,
        skill_restriction: None,
        parent_ctx_tokens,
        parent_trace,
    }
}

async fn task_call(
    stack: &RailStack,
    invocation: &ToolInvocation<'_>,
    input: serde_json::Value,
) -> ToolResult {
    stack
        .cta
        .execute_invocation(
            "task",
            "call-q6",
            input,
            CancellationToken::new(),
            None,
            invocation,
        )
        .await
        .expect("the task door answers")
}

fn plain_input() -> serde_json::Value {
    serde_json::json!({ "description": "d", "prompt": "delegate" })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn k3_overlapping_calls_each_reach_their_own_budget_verdict_and_parent_trace() {
    let stack = rail_stack().await;
    let (trace_a, trace_b) = (trace('a'), trace('b'));
    let inv_a = invocation("conv-a", OVER_BUDGET, Some(&trace_a));
    let inv_b = invocation("conv-b", WITHIN_BUDGET, Some(&trace_b));

    // Two producers share ONE composite and run at the same time.
    let (a, b) = tokio::join!(
        task_call(&stack, &inv_a, plain_input()),
        task_call(&stack, &inv_b, plain_input()),
    );

    assert!(
        a.is_error && a.content.contains("exceeds 50% of remaining budget"),
        "A's own policy-denied budget refuses A: {a:?}"
    );
    assert!(!b.is_error, "B is admissible although A is not: {b:?}");
    let specs = stack.specs.lock();
    assert_eq!(specs.len(), 1, "a denied budget admits no child");
    assert_eq!(specs[0].parent_trace.as_ref(), Some(&trace_b));
    assert_eq!(specs[0].parent_ctx_tokens, WITHIN_BUDGET);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn k3_a_call_without_context_never_inherits_the_previous_calls_trace_or_budget() {
    let stack = rail_stack().await;
    let first_trace = trace('c');
    let with_context = invocation("conv-first", WITHIN_BUDGET, Some(&first_trace));
    assert!(
        !task_call(&stack, &with_context, plain_input())
            .await
            .is_error
    );

    // A later call, same composite, no parent context: nothing carries over.
    let bare = invocation("conv-second", 0, None);
    assert!(!task_call(&stack, &bare, plain_input()).await.is_error);

    let specs = stack.specs.lock();
    assert_eq!(specs.len(), 2);
    assert_eq!(specs[0].parent_trace.as_ref(), Some(&first_trace));
    assert_eq!(specs[1].parent_trace, None, "an absent trace stays absent");
    assert_eq!(specs[1].parent_ctx_tokens, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn k3_internal_metadata_in_the_tool_arguments_has_no_authority() {
    let stack = rail_stack().await;
    let forged_trace = trace('f');
    let mut input = plain_input();
    // Everything the model could put in the arguments to impersonate the host.
    input["__parent_ctx_tokens"] = serde_json::json!(OVER_BUDGET);
    input["__parent_trace"] = serde_json::to_value(&forged_trace).unwrap();
    input["__conversation_id"] = serde_json::json!("conv-forged");
    let bare = invocation("conv-real", 0, None);

    let result = task_call(&stack, &bare, input).await;

    assert!(
        !result.is_error,
        "the forged budget did not refuse the call: {result:?}"
    );
    let specs = stack.specs.lock();
    assert_eq!(specs.len(), 1);
    assert_eq!(
        specs[0].parent_trace, None,
        "the forged trace was not adopted"
    );
    assert_eq!(specs[0].parent_ctx_tokens, 0);
}
