//! Regression test for the propose_plan / exit_plan_mode silent-drop bug.
//!
//! Root cause (see investigations/propose-plan-silent-drop-investigation.md):
//! `build_tools` constructed the live `ToolSetAdapter` without calling
//! `set_event_tx`, so `execute_propose_plan` hit its `event_tx == None`
//! branch — it logged a warning, returned a false success, and never emitted
//! `AppEvent::PlanProposed`. No approval card rendered.
//!
//! These tests drive the REAL `build_tools` composition path (not a
//! hand-wired adapter) so a future regression that drops the `set_event_tx`
//! wiring is caught.

use std::sync::Arc;

use rustain::domain::events::AppEvent;
use rustain::domain::models::checkpoint::CheckpointId;
use rustain::infrastructure::composition::{ComposeContext, build_tools};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

/// Conversation id the event-loop handler routes the approval card by; the
/// emitted `PlanProposed.conversation_id` must equal it (event_loop.rs).
const CONVERSATION_ID: &str = "conv-test-1";

/// Build a minimal `ComposeContext` with the given `domain_tx`. Mirrors
/// `tests/conformance_adapter_composition.rs::test_compose_ctx`, parameterized
/// on the event channel so we can exercise both the wired and headless paths.
fn compose_ctx(domain_tx: Option<UnboundedSender<AppEvent>>) -> ComposeContext {
    ComposeContext {
        workspace_path: std::path::PathBuf::from("/tmp/test-composition-event-tx"),
        project_context: rustain::domain::models::project_context::ProjectContext::empty(),
        storage: Arc::new(rustain::adapters::noop::NoOpStorage)
            as Arc<dyn rustain::domain::ports::StoragePort>,
        skill_activator: Arc::new(rustain::adapters::skill_activation::SkillActivator::new()),
        mcp_servers: Vec::new(),
        include_builtin_tools: true,
        domain_tx,
        tool_exposure: "static-full".into(),
        skill_exposure: "l1-metadata".into(),
        skill_cache: Arc::new(rustain::infrastructure::skill_cache::SkillCache::new_in_memory()),
        sandbox_adapter: "noop".into(),
        sandbox_startup_policy: rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        sandbox_slot: Arc::new(arc_swap::ArcSwap::from_pointee(Arc::new(
            rustain::adapters::sandbox::NoOpSandbox,
        )
            as Arc<dyn rustain::domain::ports::SandboxManager>)),
        memory_slot: std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(std::sync::Arc::new(
            rustain::adapters::noop::NoOpMemory,
        )
            as std::sync::Arc<dyn rustain::domain::ports::MemoryPort>)),
        sandbox_policy: Arc::new(tokio::sync::RwLock::new(
            rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        )),
        #[cfg(feature = "meta-search")]
        search_config: rustain::domain::models::SearchConfig::default(),
        #[cfg(feature = "meta-search")]
        meta_search_engine: None,
    }
}

/// Minimal valid propose_plan input per `plan_parser::parse_plan_input`:
/// `title` (string) + non-empty `tasks` array, each task requiring `title`.
fn propose_plan_input() -> serde_json::Value {
    serde_json::json!({
        "title": "Test plan",
        "tasks": [{ "title": "step one" }],
    })
}

#[tokio::test]
async fn propose_plan_emits_plan_proposed_with_conversation_id_when_wired() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    let ctx = compose_ctx(Some(tx));

    // Compose through the REAL factory path — this is what AgentCore uses.
    let adapter = build_tools("builtin-full", None, &ctx).expect("builtin-full composes");

    // The real turn loop sets the execution context before any tool runs
    // (turn.rs). Replicate it so the emitted conversation_id is non-empty and
    // matches what the event-loop handler requires to route the approval card
    // — guarding the full "card actually renders" invariant, not just emission.
    adapter
        .set_execution_context(CONVERSATION_ID.to_string(), CheckpointId(0), 0)
        .await;

    let result = adapter
        .execute(
            "propose_plan",
            propose_plan_input(),
            CancellationToken::new(),
        )
        .await
        .expect("propose_plan executes");
    assert!(!result.is_error, "propose_plan should not error");

    let event = rx
        .try_recv()
        .expect("PlanProposed must be emitted on the wired channel");
    match event {
        AppEvent::PlanProposed {
            conversation_id,
            plan,
        } => {
            assert_eq!(
                conversation_id, CONVERSATION_ID,
                "card routes by conversation_id; must match the execution context"
            );
            assert_eq!(plan.title, "Test plan");
            assert_eq!(plan.tasks.len(), 1);
        }
        other => panic!("expected PlanProposed, got {:?}", other),
    }
}

#[tokio::test]
async fn propose_plan_is_silent_no_op_when_domain_tx_none() {
    // Headless/eval path (domain_tx None) must remain valid: no panic,
    // returns success, emits nothing.
    let ctx = compose_ctx(None);
    let adapter = build_tools("builtin-full", None, &ctx).expect("builtin-full composes");

    let result = adapter
        .execute(
            "propose_plan",
            propose_plan_input(),
            CancellationToken::new(),
        )
        .await
        .expect("propose_plan executes without a channel");
    assert!(!result.is_error);
}

#[tokio::test]
async fn propose_plan_rejects_invalid_input_without_emitting() {
    // I/O matrix: invalid plan (empty `tasks`) → InvalidInput, no event.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    let ctx = compose_ctx(Some(tx));
    let adapter = build_tools("builtin-full", None, &ctx).expect("builtin-full composes");

    let result = adapter
        .execute(
            "propose_plan",
            serde_json::json!({ "title": "x", "tasks": [] }),
            CancellationToken::new(),
        )
        .await;
    assert!(
        result.is_err(),
        "empty tasks must be rejected as InvalidInput"
    );
    assert!(
        rx.try_recv().is_err(),
        "no PlanProposed event on invalid input"
    );
}
