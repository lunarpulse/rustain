use std::sync::Arc;

use rustain::domain::models::tool_call::ApprovalSource;
use rustain::domain::models::{ApprovalOutcome, AutoApprovePolicy, ToolRisk};
use rustain::domain::ports::ApprovalPersistencePort;
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use tokio::sync::broadcast::error::TryRecvError;

struct NoOpPersistence;

#[async_trait::async_trait]
impl ApprovalPersistencePort for NoOpPersistence {
    async fn load(
        &self,
    ) -> Result<
        rustain::domain::services::approval_runtime::SessionApprovalSet,
        rustain::domain::errors::ApprovalPersistenceError,
    > {
        Ok(Default::default())
    }
    async fn save(
        &self,
        _scope: rustain::domain::models::ApprovalScope,
    ) -> Result<(), rustain::domain::errors::ApprovalPersistenceError> {
        Ok(())
    }
}

#[tokio::test]
async fn deny_no_subagent_tool_enters_awaiting_approval() {
    let rt = ApprovalRuntime::new_with_subagent_policy(
        1024,
        Arc::new(NoOpPersistence),
        AutoApprovePolicy::Deny,
    );
    let mut rx_events = rt.subscribe();
    let (id, rx) = rt
        .request(
            ApprovalSource::ForegroundSubagent {
                conversation_id: "c1".into(),
                parent_tool_call_id: "t1".into(),
                subagent_type: "code-reviewer".into(),
            },
            "Bash".into(),
            serde_json::json!({"command": "echo test"}),
            ToolRisk::Elevated,
            None,
            None,
        )
        .await;
    assert!(
        id.is_none(),
        "deny must return None (no RequestId allocated)"
    );
    let outcome = rx.await.unwrap();
    assert_eq!(
        outcome,
        ApprovalOutcome::Reject {
            feedback: Some("subagent_auto_approve=deny".into()),
        }
    );
    assert!(
        matches!(rx_events.try_recv(), Err(TryRecvError::Empty)),
        "deny must NOT emit Requested broadcast"
    );
}

#[tokio::test]
async fn allow_returns_once_and_no_broadcast() {
    let rt = ApprovalRuntime::new_with_subagent_policy(
        1024,
        Arc::new(NoOpPersistence),
        AutoApprovePolicy::Allow,
    );
    let mut rx_events = rt.subscribe();
    let (id, rx) = rt
        .request(
            ApprovalSource::ForegroundSubagent {
                conversation_id: "c1".into(),
                parent_tool_call_id: "t1".into(),
                subagent_type: "code-reviewer".into(),
            },
            "Bash".into(),
            serde_json::json!({"command": "echo test"}),
            ToolRisk::Elevated,
            None,
            None,
        )
        .await;
    assert!(
        id.is_none(),
        "allow must return None (no RequestId allocated)"
    );
    assert_eq!(rx.await.unwrap(), ApprovalOutcome::Once);
    assert!(
        matches!(rx_events.try_recv(), Err(TryRecvError::Empty)),
        "allow must NOT emit Requested broadcast"
    );
}

#[tokio::test]
async fn ask_still_emits_requested_for_subagent() {
    let rt = ApprovalRuntime::new_with_subagent_policy(
        1024,
        Arc::new(NoOpPersistence),
        AutoApprovePolicy::Ask,
    );
    let mut rx_events = rt.subscribe();
    let (id, _rx) = rt
        .request(
            ApprovalSource::ForegroundSubagent {
                conversation_id: "c1".into(),
                parent_tool_call_id: "t1".into(),
                subagent_type: "code-reviewer".into(),
            },
            "Bash".into(),
            serde_json::json!({"command": "echo test"}),
            ToolRisk::Elevated,
            None,
            None,
        )
        .await;
    assert!(id.is_some(), "ask must return Some(RequestId)");
    let ev = rx_events.recv().await.unwrap();
    assert!(
        matches!(
            ev,
            rustain::domain::services::approval_runtime::ApprovalRuntimeEvent::Requested { .. }
        ),
        "ask must broadcast Requested"
    );
}

#[tokio::test]
async fn foreground_turn_unaffected_by_deny() {
    let rt = ApprovalRuntime::new_with_subagent_policy(
        1024,
        Arc::new(NoOpPersistence),
        AutoApprovePolicy::Deny,
    );
    let mut rx_events = rt.subscribe();
    let (id, _rx) = rt
        .request(
            ApprovalSource::ForegroundTurn {
                conversation_id: "c1".into(),
            },
            "Bash".into(),
            serde_json::json!({"command": "echo test"}),
            ToolRisk::Elevated,
            None,
            None,
        )
        .await;
    assert!(
        id.is_some(),
        "ForegroundTurn with deny must still return Some(RequestId)"
    );
    let ev = rx_events.recv().await.unwrap();
    assert!(
        matches!(
            ev,
            rustain::domain::services::approval_runtime::ApprovalRuntimeEvent::Requested { .. }
        ),
        "ForegroundTurn must still broadcast Requested"
    );
}

#[tokio::test]
async fn background_agent_deny_no_awaiting_approval() {
    let rt = ApprovalRuntime::new_with_subagent_policy(
        1024,
        Arc::new(NoOpPersistence),
        AutoApprovePolicy::Deny,
    );
    let mut rx_events = rt.subscribe();
    let (id, rx) = rt
        .request(
            ApprovalSource::BackgroundAgent {
                conversation_id: "c1".into(),
                task_id: "task-1".into(),
                subagent_type: "background-worker".into(),
            },
            "Bash".into(),
            serde_json::json!({"command": "echo test"}),
            ToolRisk::Elevated,
            None,
            None,
        )
        .await;
    assert!(
        id.is_none(),
        "deny must return None for BackgroundAgent (no RequestId allocated)"
    );
    let outcome = rx.await.unwrap();
    assert_eq!(
        outcome,
        ApprovalOutcome::Reject {
            feedback: Some("subagent_auto_approve=deny".into()),
        }
    );
    assert!(
        matches!(rx_events.try_recv(), Err(TryRecvError::Empty)),
        "deny must NOT emit Requested broadcast for BackgroundAgent"
    );
}

#[tokio::test]
async fn background_agent_allow_returns_once() {
    let rt = ApprovalRuntime::new_with_subagent_policy(
        1024,
        Arc::new(NoOpPersistence),
        AutoApprovePolicy::Allow,
    );
    let mut rx_events = rt.subscribe();
    let (id, rx) = rt
        .request(
            ApprovalSource::BackgroundAgent {
                conversation_id: "c1".into(),
                task_id: "task-1".into(),
                subagent_type: "background-worker".into(),
            },
            "Bash".into(),
            serde_json::json!({"command": "echo test"}),
            ToolRisk::Elevated,
            None,
            None,
        )
        .await;
    assert!(
        id.is_none(),
        "allow must return None for BackgroundAgent (no RequestId allocated)"
    );
    assert_eq!(rx.await.unwrap(), ApprovalOutcome::Once);
    assert!(
        matches!(rx_events.try_recv(), Err(TryRecvError::Empty)),
        "allow must NOT emit Requested broadcast for BackgroundAgent"
    );
}
