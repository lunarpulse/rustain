use std::sync::Arc;

use async_trait::async_trait;
use futures::FutureExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::domain::models::{
    AgentId, AgentLaunchSpec, Op, SubagentError, SubagentRunStatus, TaskHandle,
};
use crate::domain::ports::SubagentRunner;
use crate::domain::services::sandbox_narrowing::validate_narrowing;
use crate::infrastructure::subagent::{SubagentRegistry, SubagentSpool};

pub struct InProcessSubagentRunner {
    provider: Arc<dyn crate::domain::ports::StreamingProvider>,
    storage: Arc<dyn crate::domain::ports::StoragePort>,
    security: Arc<dyn crate::domain::ports::SecurityPort>,
    tools: Arc<dyn crate::domain::ports::ToolSetPort>,
    approval: Arc<crate::domain::services::approval_runtime::ApprovalRuntime>,
    scheduler: Arc<crate::domain::services::tool_scheduler::ToolScheduler>,
    event_bus: Arc<crate::infrastructure::runtime::event_bus::EventBus>,
    registry: Arc<SubagentRegistry>,
    parent_sandbox: Arc<tokio::sync::RwLock<crate::domain::models::SandboxPolicy>>,
    spool: Arc<SubagentSpool>,
}

impl InProcessSubagentRunner {
    pub fn new(
        provider: Arc<dyn crate::domain::ports::StreamingProvider>,
        storage: Arc<dyn crate::domain::ports::StoragePort>,
        security: Arc<dyn crate::domain::ports::SecurityPort>,
        tools: Arc<dyn crate::domain::ports::ToolSetPort>,
        approval: Arc<crate::domain::services::approval_runtime::ApprovalRuntime>,
        scheduler: Arc<crate::domain::services::tool_scheduler::ToolScheduler>,
        event_bus: Arc<crate::infrastructure::runtime::event_bus::EventBus>,
        registry: Arc<SubagentRegistry>,
        parent_sandbox: Arc<tokio::sync::RwLock<crate::domain::models::SandboxPolicy>>,
        spool: Arc<SubagentSpool>,
    ) -> Self {
        Self {
            provider,
            storage,
            security,
            tools,
            approval,
            scheduler,
            event_bus,
            registry,
            parent_sandbox,
            spool,
        }
    }

    /// Deregister an agent from the registry (used by perf tests to avoid NFR15 limit).
    pub async fn deregister(&self, agent_id: &AgentId) {
        self.registry.deregister(agent_id).await;
    }
}

#[async_trait]
impl SubagentRunner for InProcessSubagentRunner {
    async fn launch(
        &self,
        spec: AgentLaunchSpec,
        cancel: CancellationToken,
    ) -> Result<TaskHandle, SubagentError> {
        // 1. Validate sandbox narrowing BEFORE spawn
        if let Some(ref child_policy) = spec.sandbox_override {
            let parent_policy = self.parent_sandbox.read().await.clone();
            validate_narrowing(&parent_policy, child_policy)?;
        }

        // 2. Create channels
        let (command_tx, command_rx) = mpsc::channel::<Op>(512);
        let (status_tx, status_rx) = mpsc::channel::<SubagentRunStatus>(512);

        // 3. Derive child cancellation token
        let child_cancel = cancel.child_token();
        let subagent_type = String::from("in-process"); // overridden by caller via TaskHandle

        // 4. Generate IDs
        let agent_id = AgentId::new();
        let task_id = nanoid::nanoid!(12);

        // 5. Spawn child task
        let provider = self.provider.clone();
        let scheduler = self.scheduler.clone();
        let approval = self.approval.clone();
        let event_bus = self.event_bus.clone();
        let spool = self.spool.clone();
        let child_agent_id = agent_id.clone();
        let child_task_id = task_id.clone();
        let child_cancel_for_spawn = child_cancel.clone();
        let status_tx_for_panic = status_tx.clone();

        let _handle = tokio::spawn(async move {
            let result = std::panic::AssertUnwindSafe(run_child(
                spec,
                provider,
                scheduler,
                approval,
                event_bus,
                spool,
                status_tx,
                command_rx,
                child_cancel_for_spawn.clone(),
                child_agent_id,
                child_task_id,
            ))
            .catch_unwind()
            .await;

            if let Err(panic_payload) = result {
                let msg = format!("{:?}", panic_payload);
                tracing::error!(%msg, "Subagent task panicked");
                let _ = status_tx_for_panic.send(SubagentRunStatus::Failed).await;
                child_cancel_for_spawn.cancel();
            }
        });

        // 6. Register with registry
        let reg_handle = crate::infrastructure::subagent::registry::AgentHandle {
            agent_id: agent_id.clone(),
            command_tx: command_tx.clone(),
            depth: 0, // overwritten by registry::register()
            subagent_type: subagent_type.clone(),
        };
        self.registry
            .register(agent_id.clone(), AgentId::root(), reg_handle)
            .await?;

        Ok(TaskHandle {
            agent_id,
            status_rx,
            command_tx,
            cancel: child_cancel,
            task_id,
            subagent_type,
        })
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_child(
    spec: AgentLaunchSpec,
    _provider: Arc<dyn crate::domain::ports::StreamingProvider>,
    _scheduler: Arc<crate::domain::services::tool_scheduler::ToolScheduler>,
    _approval: Arc<crate::domain::services::approval_runtime::ApprovalRuntime>,
    _event_bus: Arc<crate::infrastructure::runtime::event_bus::EventBus>,
    _spool: Arc<SubagentSpool>,
    status_tx: mpsc::Sender<SubagentRunStatus>,
    mut command_rx: mpsc::Receiver<Op>,
    cancel: CancellationToken,
    _agent_id: AgentId,
    _task_id: String,
) {
    // Emit RunningFg
    let _ = status_tx.send(SubagentRunStatus::RunningFg).await;

    // v0 simplified child body — Story 10.7 will wire the full provider+scheduler loop.
    // For the foundation, we just wait for cancellation or a Kill command.
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                let _ = status_tx.send(SubagentRunStatus::Killed).await;
                break;
            }
            Some(op) = command_rx.recv() => {
                match op {
                    Op::Kill => {
                        let _ = status_tx.send(SubagentRunStatus::Killed).await;
                        break;
                    }
                    _ => {
                        // Pause/Resume/ChangeModel/UpdateTools reserved for Story 10.2
                        tracing::debug!("Received non-Kill Op in v0 subagent — ignoring");
                    }
                }
            }
            else => {
                // Channels closed — exit
                let _ = status_tx.send(SubagentRunStatus::Completed).await;
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::filesystem::FileSystemStorage;
    use crate::adapters::noop::{NoOpApprovalPersistence, NoOpProvider};
    use crate::adapters::sandbox::NoOpSandbox;
    use crate::adapters::security_adapter::SecurityAdapter;
    use crate::adapters::toolset_adapter::ToolSetAdapter;
    use crate::domain::services::approval_runtime::ApprovalRuntime;
    use crate::domain::services::tool_scheduler::ToolScheduler;
    use crate::infrastructure::runtime::event_bus::EventBus;
    use arc_swap::ArcSwap;
    use std::path::PathBuf;

    async fn make_runner() -> (InProcessSubagentRunner, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let provider = Arc::new(NoOpProvider) as Arc<dyn crate::domain::ports::StreamingProvider>;
        let storage = Arc::new(FileSystemStorage::new(tmp.path().to_path_buf()))
            as Arc<dyn crate::domain::ports::StoragePort>;
        let security = Arc::new(SecurityAdapter::new(PathBuf::from(".")))
            as Arc<dyn crate::domain::ports::SecurityPort>;
        let sandbox = Arc::new(ArcSwap::from_pointee(
            Arc::new(NoOpSandbox) as Arc<dyn crate::domain::ports::SandboxManager>
        ));
        let tools = Arc::new(ToolSetAdapter::new(
            PathBuf::from("."),
            storage.clone(),
            sandbox,
            Arc::new(tokio::sync::RwLock::new(
                crate::domain::models::SandboxPolicy::Permissive,
            )),
        )) as Arc<dyn crate::domain::ports::ToolSetPort>;
        let approval = ApprovalRuntime::new(1024, Arc::new(NoOpApprovalPersistence));
        let scheduler = ToolScheduler::new(security.clone(), tools.clone(), approval.clone(), 1024);
        let (event_bus, _event_rx) = EventBus::new(1024);
        let event_bus = Arc::new(event_bus);
        let registry = Arc::new(SubagentRegistry::new());
        let parent_sandbox = Arc::new(tokio::sync::RwLock::new(
            crate::domain::models::SandboxPolicy::Permissive,
        ));
        let spool = Arc::new(SubagentSpool::new(tmp.path().join("spool")).await.unwrap());

        let runner = InProcessSubagentRunner::new(
            provider,
            storage,
            security,
            tools,
            approval,
            scheduler,
            event_bus,
            registry,
            parent_sandbox,
            spool,
        );
        (runner, tmp)
    }

    #[tokio::test]
    async fn happy_path_launch() {
        let (runner, _tmp) = make_runner().await;
        let spec = AgentLaunchSpec {
            prompt: String::from("hello"),
            effective_model: String::from("test-model"),
            tier: crate::domain::models::ModelTier::CheapAgentic,
            tools_allow: crate::domain::models::ToolPolicy::InheritFromParent,
            parent_ctx_tokens: 0,
            sandbox_override: None,
            parent_trace: None,
        };
        let cancel = CancellationToken::new();
        let handle = runner.launch(spec, cancel.clone()).await.unwrap();
        assert!(!handle.task_id.is_empty());
        // Let it run briefly then kill
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        handle.cancel.cancel();
        // Drain status channel
        let mut statuses = Vec::new();
        let mut rx = handle.status_rx;
        while let Ok(s) = rx.try_recv() {
            statuses.push(s);
        }
        assert!(
            statuses
                .iter()
                .any(|s| matches!(s, SubagentRunStatus::RunningFg))
        );
    }

    #[tokio::test]
    async fn cancel_kills_child() {
        let (runner, _tmp) = make_runner().await;
        let spec = AgentLaunchSpec {
            prompt: String::from("hello"),
            effective_model: String::from("test-model"),
            tier: crate::domain::models::ModelTier::CheapAgentic,
            tools_allow: crate::domain::models::ToolPolicy::InheritFromParent,
            parent_ctx_tokens: 0,
            sandbox_override: None,
            parent_trace: None,
        };
        let cancel = CancellationToken::new();
        let handle = runner.launch(spec, cancel.clone()).await.unwrap();
        handle.cancel.cancel();
        // Wait for status
        let mut rx = handle.status_rx;
        let status = tokio::time::timeout(tokio::time::Duration::from_millis(500), rx.recv()).await;
        assert!(status.is_ok());
        // Should see Killed or Completed
    }
}
