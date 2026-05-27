use crate::domain::models::{AgentLaunchSpec, SubagentError, TaskHandle};
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

/// Contract for launching a subagent. Implemented by `InProcessSubagentRunner`
/// (Story 10.0, this story) and `BackgroundSubagentRunner` (deferred Story 10.9).
/// Per ADR-10-1: swapping FG↔BG is a composition-root binding decision, not an
/// architectural change. Per ADR-06-09: shared ports (`ProviderPort`,
/// `StoragePort`, `SecurityPort`, `ApprovalRuntime`, `ToolScheduler`, `EventBus`)
/// are passed in via Arc<dyn Trait> at construction time, not on `launch()`.
#[async_trait]
pub trait SubagentRunner: Send + Sync {
    async fn launch(
        &self,
        spec: AgentLaunchSpec,
        cancel: CancellationToken,
    ) -> Result<TaskHandle, SubagentError>;
}
