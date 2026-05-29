//! Stub subagent runner for integration tests.
//!
//! Emits a chosen terminal status immediately and writes a known string into the spool.

use rustain::domain::models::{AgentLaunchSpec, SubagentRunStatus, TaskHandle};
use rustain::domain::ports::SubagentRunner;
use rustain::infrastructure::subagent::SubagentSpool;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// A stub runner that immediately returns a pre-configured terminal status.
#[allow(dead_code)]
pub struct StubSubagentRunner {
    outcome: SubagentRunStatus,
    spool_text: String,
}

impl StubSubagentRunner {
    pub fn new(outcome: SubagentRunStatus, spool_text: impl Into<String>) -> Self {
        Self {
            outcome,
            spool_text: spool_text.into(),
        }
    }
}

#[async_trait::async_trait]
impl SubagentRunner for StubSubagentRunner {
    async fn launch(
        &self,
        spec: AgentLaunchSpec,
        cancel: CancellationToken,
    ) -> Result<TaskHandle, rustain::domain::models::SubagentError> {
        let (status_tx, status_rx) = mpsc::channel(4);
        let _ = status_tx.send(self.outcome).await;

        Ok(TaskHandle {
            agent_id: rustain::domain::models::AgentId::new(),
            task_id: format!("stub-{}", spec.effective_model),
            status_rx,
            command_tx: mpsc::channel(1).0,
            cancel,
            subagent_type: "stub".to_string(),
            spawned_at: chrono::Utc::now().timestamp_millis(),
        })
    }
}

/// A stub runner that always fails to launch.
pub struct FailingSubagentRunner;

#[async_trait::async_trait]
impl SubagentRunner for FailingSubagentRunner {
    async fn launch(
        &self,
        _spec: AgentLaunchSpec,
        _cancel: CancellationToken,
    ) -> Result<TaskHandle, rustain::domain::models::SubagentError> {
        Err(rustain::domain::models::SubagentError::Internal(
            "injected launch failure".to_string(),
        ))
    }
}

/// Write text into the spool for a given task_id.
#[allow(dead_code)]
pub async fn write_spool(spool: &SubagentSpool, task_id: &str, text: &str) {
    spool.append(task_id, text.as_bytes()).await.ok();
}
