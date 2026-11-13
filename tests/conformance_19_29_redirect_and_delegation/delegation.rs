//! Story 19.29 AC8 (end-to-end freshness) — a turn's `task` call carries the
//! turn's skill restriction through the composite adapter and the provider to
//! the child's launch spec.
//!
//! Front door: the production turn (`run_turn`) with the real
//! `CompositeToolsetAdapter` + `SubagentProvider` + recording runner as the
//! turn's tools. The restriction is derived from the turn's activation snapshot
//! at the scheduling call (R6: one source, per call), so a snapshot taken
//! before a mid-batch `activate_skill` can never be the one that crosses.
//!
//! ⚠ A `activate_skill` + `task` pair in ONE response is not driven here: the
//! harness has no real skill activator behind the composite adapter. That
//! property rests on R6's single source (`ToolInvocation` is filled from the
//! very values the chain judged, per scheduling call) plus the existing serial
//! activation-loop test `ac1_activation_interleaving_cannot_bypass_agent_gate`
//! in `skill_restriction_notice.rs`.

use std::sync::Arc;

use parking_lot::Mutex as PlMutex;
use rustain::adapters::security_adapter::SecurityAdapter;
use rustain::domain::models::PermissionMode;
use rustain::domain::ports::{SecurityPort, ToolSetPort};

use super::rail_harness::rail_stack;
use crate::skill_restriction_notice::{
    ToolCallProvider, activation_from_skill_md, active_agent, error_for, run_turn_front_door,
    write_skill,
};

#[tokio::test(flavor = "multi_thread")]
async fn ac8_a_turns_task_call_carries_the_active_skills_restriction_to_the_child() {
    let stack = rail_stack().await;
    let workspace = std::fs::canonicalize(stack.workspace.path()).unwrap();
    write_skill(
        &workspace,
        "delegator",
        "---\nname: delegator\ndescription: May delegate\nallowed-tools: Bash(echo:*) task\n---\n# Body\n",
    );
    let activation = activation_from_skill_md(&workspace, "delegator").await;

    let provider = Arc::new(ToolCallProvider {
        calls: std::sync::atomic::AtomicUsize::new(0),
        tool_calls: vec![(
            "task-1".to_string(),
            "task".to_string(),
            serde_json::json!({"description": "d", "prompt": "delegate-from-turn"}),
        )],
        offered: PlMutex::new(Vec::new()),
        results: PlMutex::new(Vec::new()),
    });
    let security = Arc::new(SecurityAdapter::new(workspace.clone()));
    security.set_mode(PermissionMode::Yolo);

    run_turn_front_door(
        &workspace,
        Some(activation),
        Some(active_agent(&workspace, "plain", None, None)),
        provider.clone(),
        security,
        stack.cta.clone() as Arc<dyn ToolSetPort>,
        None,
    )
    .await;

    let results = provider.results.lock().clone();
    assert!(
        !error_for(&results, "task-1").is_error,
        "the delegation must succeed: {results:?}"
    );
    let specs = stack.specs.lock();
    assert_eq!(specs.len(), 1, "exactly one child was launched");
    let inherited = specs[0]
        .inherited_skill_restriction
        .as_ref()
        .expect("the turn's skill restriction must reach the child's spec");
    assert_eq!(inherited.skill_names, vec!["delegator".to_string()]);
    assert_eq!(
        inherited
            .items
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["Bash(echo:*)", "task"]
    );
    assert!(inherited.inherited);
}
