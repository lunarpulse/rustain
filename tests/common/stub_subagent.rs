#![allow(dead_code)] // shared test-support module; helpers used by a subset of integration-test binaries
//! Stub subagent runner for integration tests.
//!
//! Emits a chosen terminal status immediately and writes a known string into the spool.

use rustain::domain::models::{AgentLaunchSpec, NodeState, TaskHandle};
use rustain::domain::ports::SubagentRunner;
use rustain::infrastructure::subagent::SubagentSpool;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// A stub runner that immediately returns a pre-configured terminal status.
#[allow(dead_code)]
pub struct StubSubagentRunner {
    outcome: NodeState,
    spool_text: String,
}

impl StubSubagentRunner {
    pub fn new(outcome: NodeState, spool_text: impl Into<String>) -> Self {
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
        _parent: Option<&rustain::domain::models::TaskHandle>,
    ) -> Result<TaskHandle, rustain::domain::models::SubagentError> {
        let (status_tx, status_rx) = mpsc::channel(4);
        let _ = status_tx.send(self.outcome).await;
        let (parent_disconnect, _parent_disconnect_rx) = tokio::sync::mpsc::unbounded_channel();

        Ok(TaskHandle {
            agent_id: rustain::domain::models::AgentId::new(),
            task_id: format!("stub-{}", spec.effective_model),
            status_rx,
            command_tx: mpsc::channel(1).0,
            cancel,
            subagent_type: "stub".to_string(),
            spawned_at: chrono::Utc::now().timestamp_millis(),
            parent_disconnect,
            yield_rx: None,
            isolation_diff_rx: None,
            effective_workspace: std::path::PathBuf::from("."),
            isolated: false,
            authority: rustain::domain::models::CapabilityTokenId::root(),
            authority_token: None,
            patch_provenance: rustain::domain::models::ProvenanceTag::UserOriginated,
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
        _parent: Option<&rustain::domain::models::TaskHandle>,
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
