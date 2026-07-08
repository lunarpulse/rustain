//! Story 14-7 (Task 2 foundations) — the additive `ApprovalSource::AcpSession`
//! variant and the approval posture for editor-driven ACP turns.
//!
//! These tests defend three contracts (AC3 / DD7):
//!
//! 1. **Additivity** — the new `AcpSession` variant did not perturb the
//!    `conversation_id` accessor for the pre-existing variants (table-driven).
//! 2. **Scope node-binding** — `AcpSession`'s `scope_agent_id` uses the
//!    length-prefixed join, so distinct `(conversation_id, session_id)` pairs
//!    that share a delimiter cannot collapse onto one approval scope (the 14.6
//!    DD1 guarantee extended to the new variant). An approval minted for one
//!    session must not replay on another.
//! 3. **Ask posture** — an ACP session is `Self`-rooted (the same human
//!    principal as a foreground turn), so it must NOT be swept into the subagent
//!    auto-approve short-circuit: under `AutoApprovePolicy::Deny` a real
//!    `ForegroundSubagent` is auto-rejected while an `AcpSession` request falls
//!    through to the slow path and broadcasts `Requested` (Ask). A mutant that
//!    groups `AcpSession` with the subagent variants reddens this.
//!
//! The posture test drives the **real** `ApprovalRuntime` (no mocking of the
//! decision logic); only the persistence port is faked, which is a true
//! external storage boundary.

use std::sync::Arc;

use rustain::domain::errors::ApprovalPersistenceError;
use rustain::domain::models::tool_call::ApprovalSource;
use rustain::domain::models::{ApprovalScope, AutoApprovePolicy, ToolRisk};
use rustain::domain::ports::ApprovalPersistencePort;
use rustain::domain::services::approval_runtime::{
    ApprovalRuntime, ApprovalRuntimeEvent, SessionApprovalSet,
};

/// Hand-written in-memory persistence fake. ApprovalPersistencePort is a true
/// external storage boundary; a minimal fake is clearer and more stable than a
/// mock, and never asserts on its own call args.
struct NoOpPersistence;

#[async_trait::async_trait]
impl ApprovalPersistencePort for NoOpPersistence {
    async fn load(&self) -> Result<SessionApprovalSet, ApprovalPersistenceError> {
        Ok(SessionApprovalSet::default())
    }
    async fn save(&self, _scope: ApprovalScope) -> Result<(), ApprovalPersistenceError> {
        Ok(())
    }
}

/// The editor correlation id is the **conversation**, not the per-connection
/// session id. A mutant that returns `session_id` would route the approval to
/// the wrong conversation ledger.
#[test]
fn acp_session_conversation_id_exposes_the_conversation_field() {
    let src = ApprovalSource::AcpSession {
        session_id: "sess-7".into(),
        conversation_id: "conv-42".into(),
    };
    assert_eq!(src.conversation_id(), "conv-42");
}

/// DD1 node-binding guarantee for the new variant: two DISTINCT
/// `(conversation_id, session_id)` pairs that share a delimiter character must
/// NOT collapse onto one approval scope — otherwise an approval for one session
/// could replay on another. A bare `format!("{}/{}", …)` (or any unprefixed
/// join) reddens this.
#[test]
fn acp_session_scope_is_collision_free_across_delimiter_sharing_pairs() {
    let a = ApprovalSource::AcpSession {
        conversation_id: "a/b".into(),
        session_id: "c".into(),
    };
    let b = ApprovalSource::AcpSession {
        conversation_id: "a".into(),
        session_id: "b/c".into(),
    };
    assert_ne!(
        a.scope_agent_id(),
        b.scope_agent_id(),
        "distinct (conversation_id, session_id) pairs sharing a delimiter must not collide"
    );
}

/// Additivity guard: adding `AcpSession` must not perturb the `conversation_id`
/// accessor for any variant — each source still files its approval under its own
/// conversation. A regression means the new match arm broke an existing path.
#[test]
fn every_source_variant_exposes_its_conversation_id() {
    let cases: &[(&str, ApprovalSource)] = &[
        (
            "conv-fg",
            ApprovalSource::ForegroundTurn {
                conversation_id: "conv-fg".into(),
            },
        ),
        (
            "conv-sub",
            ApprovalSource::ForegroundSubagent {
                conversation_id: "conv-sub".into(),
                parent_tool_call_id: "tc-1".into(),
                subagent_type: "explore".into(),
            },
        ),
        (
            "conv-acp",
            ApprovalSource::AcpSession {
                session_id: "sess-1".into(),
                conversation_id: "conv-acp".into(),
            },
        ),
        (
            "conv-bg",
            ApprovalSource::BackgroundAgent {
                conversation_id: "conv-bg".into(),
                task_id: "task-1".into(),
                subagent_type: "general".into(),
            },
        ),
    ];
    for (expected, src) in cases {
        assert_eq!(
            src.conversation_id(),
            *expected,
            "conversation_id accessor stable across all variants"
        );
    }
}

/// DD7 Ask posture: an ACP session is `Self`-rooted, so it escapes the subagent
/// auto-approve short-circuit entirely. Under `Deny`, a real `ForegroundSubagent`
/// is auto-rejected (positive control proving the deny arm is armed), while an
/// `AcpSession` request reaches the slow path and broadcasts `Requested`. A
/// mutant that groups `AcpSession` with the subagent variants reddens the
/// AcpSession assertions while leaving the positive control green.
#[tokio::test]
async fn acp_session_is_not_swept_into_the_subagent_auto_deny_short_circuit() {
    let rt = ApprovalRuntime::new_with_subagent_policy(
        64,
        Arc::new(NoOpPersistence),
        AutoApprovePolicy::Deny,
    );
    let mut events = rt.subscribe();

    // Positive control: the deny mechanism is armed for an actual subagent.
    let (sub_id, _sub_rx) = rt
        .request(
            ApprovalSource::ForegroundSubagent {
                conversation_id: "c".into(),
                parent_tool_call_id: "t".into(),
                subagent_type: "code-reviewer".into(),
            },
            "Bash".into(),
            serde_json::json!({"command": "echo hi"}),
            ToolRisk::Elevated,
            None,
            None,
        )
        .await;
    assert!(
        sub_id.is_none(),
        "positive control: Deny auto-rejects a subagent (no slow-path id)"
    );
    assert!(
        matches!(
            events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ),
        "positive control: an auto-rejected subagent emits no Requested broadcast"
    );

    // Contract under test: AcpSession reaches the slow path (Ask).
    let (acp_id, _acp_rx) = rt
        .request(
            ApprovalSource::AcpSession {
                session_id: "sess-1".into(),
                conversation_id: "c".into(),
            },
            "Bash".into(),
            serde_json::json!({"command": "echo hi"}),
            ToolRisk::Elevated,
            None,
            None,
        )
        .await;
    assert!(
        acp_id.is_some(),
        "AcpSession must reach the slow path (Ask), not be auto-rejected by the subagent policy"
    );
    match events.try_recv() {
        Ok(ApprovalRuntimeEvent::Requested { source, .. }) => {
            assert!(
                matches!(source, ApprovalSource::AcpSession { .. }),
                "the Requested broadcast must carry the AcpSession source"
            );
        }
        other => panic!(
            "expected a Requested broadcast for the AcpSession ask, got {:?}",
            other
        ),
    }
    assert_eq!(
        rt.rejected_count(),
        0,
        "the AcpSession ask registered no rejection (it is pending, not denied)"
    );
}
