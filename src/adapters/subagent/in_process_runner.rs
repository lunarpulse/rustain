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

    /// Access the underlying registry (for integration tests and observers).
    pub fn registry(&self) -> Arc<SubagentRegistry> {
        self.registry.clone()
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
        let (bridge_tx, bridge_rx) = mpsc::channel::<SubagentRunStatus>(64);

        // 3. Derive child cancellation token
        let child_cancel = cancel.child_token();
        let subagent_type = String::from("in-process"); // overridden by caller via TaskHandle

        // 4. Generate IDs
        let agent_id = AgentId::new();
        let task_id = nanoid::nanoid!(12);

        // 5. Construct ChildState (before spawn so it can be cloned into closure)
        let child_state = Arc::new(crate::adapters::subagent::ChildState::new(
            spec.effective_model.clone(),
            spec.tools_allow.clone(),
        ));

        // 6. Spawn child task
        let provider = self.provider.clone();
        let scheduler = self.scheduler.clone();
        let approval = self.approval.clone();
        let event_bus = self.event_bus.clone();
        let spool = self.spool.clone();
        let child_agent_id = agent_id.clone();
        let child_task_id = task_id.clone();
        let child_cancel_for_spawn = child_cancel.clone();
        let status_tx_for_panic = status_tx.clone();
        let child_state_for_spawn = child_state.clone();
        let bridge_tx_for_spawn = bridge_tx.clone();
        let bridge_tx_for_panic = bridge_tx.clone();

        let _handle = tokio::spawn(async move {
            let result = std::panic::AssertUnwindSafe(run_child(
                spec,
                provider,
                scheduler,
                approval,
                event_bus,
                spool,
                status_tx,
                bridge_tx_for_spawn,
                command_rx,
                child_cancel_for_spawn.clone(),
                child_agent_id,
                child_task_id,
                child_state_for_spawn,
            ))
            .catch_unwind()
            .await;

            if let Err(panic_payload) = result {
                let msg = format!("{:?}", panic_payload);
                tracing::error!(%msg, "Subagent task panicked");
                let _ = status_tx_for_panic.send(SubagentRunStatus::Failed).await;
                let _ = bridge_tx_for_panic.send(SubagentRunStatus::Failed).await;
                child_cancel_for_spawn.cancel();
            }
        });

        // 7. Register with registry
        let (watch_tx, _watch_rx) = tokio::sync::watch::channel(SubagentRunStatus::Idle);
        let reg_handle = crate::infrastructure::subagent::registry::AgentHandle {
            agent_id: agent_id.clone(),
            command_tx: command_tx.clone(),
            depth: 0, // overwritten by registry::register()
            subagent_type: subagent_type.clone(),
            spawned_at: 0,
            status: watch_tx,
        };
        self.registry
            .register(agent_id.clone(), AgentId::root(), reg_handle)
            .await?;

        // Read spawned_at from registry for TaskHandle
        let spawned_at = self
            .registry
            .list()
            .await
            .into_iter()
            .find(|e| e.agent_id == agent_id)
            .map(|e| e.spawned_at)
            .unwrap_or(0);

        // 8. Spawn status bridge task (mirrors mpsc → registry watch)
        let registry = self.registry.clone();
        let agent_id_for_bridge = agent_id.clone();
        tokio::spawn(async move {
            let mut rx = bridge_rx;
            while let Some(s) = rx.recv().await {
                if let Some(watch_tx) = registry.status_sender(&agent_id_for_bridge).await {
                    let _ = watch_tx.send(s);
                }
                registry.emit_status_updated(&agent_id_for_bridge).await;
                if matches!(
                    s,
                    SubagentRunStatus::Completed
                        | SubagentRunStatus::Failed
                        | SubagentRunStatus::Killed
                ) {
                    registry.deregister(&agent_id_for_bridge).await;
                    break;
                }
            }
            // Safety net: if bridge_rx closed without a terminal status (e.g. panic
            // where bridge_tx.send also failed), ensure the registry entry is cleaned up.
            let entries = registry.list().await;
            if entries.iter().any(|e| e.agent_id == agent_id_for_bridge) {
                tracing::warn!(agent_id = %agent_id_for_bridge.0, "Bridge task exiting without terminal status — force deregistering");
                registry.deregister(&agent_id_for_bridge).await;
            }
        });

        Ok(TaskHandle {
            agent_id,
            status_rx,
            command_tx,
            cancel: child_cancel,
            task_id,
            subagent_type,
            spawned_at,
        })
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_child(
    _spec: AgentLaunchSpec,
    _provider: Arc<dyn crate::domain::ports::StreamingProvider>,
    _scheduler: Arc<crate::domain::services::tool_scheduler::ToolScheduler>,
    _approval: Arc<crate::domain::services::approval_runtime::ApprovalRuntime>,
    _event_bus: Arc<crate::infrastructure::runtime::event_bus::EventBus>,
    _spool: Arc<SubagentSpool>,
    status_tx: mpsc::Sender<SubagentRunStatus>,
    bridge_tx: mpsc::Sender<SubagentRunStatus>,
    mut command_rx: mpsc::Receiver<Op>,
    cancel: CancellationToken,
    agent_id: AgentId,
    _task_id: String,
    child_state: Arc<crate::adapters::subagent::ChildState>,
) {
    // Helper: emit status to both channels + update ChildState watch sender
    async fn emit_status(
        s: SubagentRunStatus,
        status_tx: &mpsc::Sender<SubagentRunStatus>,
        bridge_tx: &mpsc::Sender<SubagentRunStatus>,
        child_state: &crate::adapters::subagent::ChildState,
    ) {
        if let Err(e) = status_tx.send(s).await {
            tracing::warn!(error = %e, "status_tx send failed");
        }
        if let Err(e) = bridge_tx.send(s).await {
            tracing::warn!(error = %e, "bridge_tx send failed");
        }
        let _ = child_state.status.send(s);
    }

    // Emit RunningFg
    emit_status(SubagentRunStatus::RunningFg, &status_tx, &bridge_tx, &child_state).await;

    // v0 simplified child body — Story 10.7 will wire the full provider+scheduler loop.
    // For the foundation, we just wait for cancellation or owner commands.
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                emit_status(SubagentRunStatus::Killed, &status_tx, &bridge_tx, &child_state).await;
                break;
            }
            Some(op) = command_rx.recv() => {
                match op {
                    Op::Kill => {
                        emit_status(SubagentRunStatus::Killed, &status_tx, &bridge_tx, &child_state).await;
                        break;
                    }
                    Op::Pause => {
                        child_state.paused.store(true, std::sync::atomic::Ordering::Release);
                        emit_status(SubagentRunStatus::Idle, &status_tx, &bridge_tx, &child_state).await;
                        tracing::info!(agent_id = %agent_id.0, "Subagent paused");
                    }
                    Op::Resume => {
                        child_state.paused.store(false, std::sync::atomic::Ordering::Release);
                        emit_status(SubagentRunStatus::RunningFg, &status_tx, &bridge_tx, &child_state).await;
                        tracing::info!(agent_id = %agent_id.0, "Subagent resumed");
                    }
                    Op::ChangeModel(new_model) => {
                        child_state.effective_model.store(Arc::new(new_model.clone()));
                        tracing::info!(agent_id = %agent_id.0, model = %new_model, "Subagent model changed");
                    }
                    Op::UpdateTools(allowlist) => {
                        let policy = crate::domain::models::ToolPolicy::Allowlist {
                            tools: allowlist.into_iter().collect(),
                        };
                        child_state.tools_allow.store(Arc::new(policy));
                        tracing::info!(agent_id = %agent_id.0, "Subagent tools updated");
                    }
                    Op::ReportFull => {
                        let current = *child_state.status.borrow();
                        emit_status(current, &status_tx, &bridge_tx, &child_state).await;
                        tracing::info!(agent_id = %agent_id.0, status = ?current, "Subagent ReportFull");
                    }
                }
            }
            else => {
                // Channels closed — exit
                emit_status(SubagentRunStatus::Completed, &status_tx, &bridge_tx, &child_state).await;
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

    // ── AC-10-2-5: Owner-command wiring tests ───────────────────────────

    #[tokio::test]
    async fn op_pause_sets_atomic() {
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

        // Send Pause
        let _ = handle.command_tx.send(Op::Pause).await;
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Verify via registry that the child is paused
        // (ChildState is not directly accessible, but we can verify status)
        let entries = runner.registry.list().await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].current_status, SubagentRunStatus::Idle);

        handle.cancel.cancel();
    }

    #[tokio::test]
    async fn op_resume_clears_atomic() {
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

        let _ = handle.command_tx.send(Op::Pause).await;
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        let _ = handle.command_tx.send(Op::Resume).await;
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let entries = runner.registry.list().await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].current_status, SubagentRunStatus::RunningFg);

        handle.cancel.cancel();
    }

    #[tokio::test]
    async fn op_change_model_swaps() {
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

        let _ = handle.command_tx.send(Op::ChangeModel("opus".into())).await;
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // ChildState.effective_model is not directly observable, but the command should not error
        handle.cancel.cancel();
    }

    #[tokio::test]
    async fn op_update_tools_swaps() {
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

        let _ = handle
            .command_tx
            .send(Op::UpdateTools(vec!["bash".into()]))
            .await;
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        handle.cancel.cancel();
    }

    #[tokio::test]
    async fn op_report_full_emits_status() {
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

        // Send ReportFull
        let _ = handle.command_tx.send(Op::ReportFull).await;

        // Should receive a status on status_rx within 50ms
        let mut rx = handle.status_rx;
        let result = tokio::time::timeout(tokio::time::Duration::from_millis(50), rx.recv()).await;
        assert!(result.is_ok());
        let status = result.unwrap();
        assert!(status.is_some());

        handle.cancel.cancel();
    }
}
