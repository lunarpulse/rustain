//! Integration tests for subagent cascade kill + status bridge (AC-10-2-6, -7).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use rustain::adapters::subagent::InProcessSubagentRunner;
use rustain::domain::models::{
    AgentId, AgentLaunchSpec, Op, SandboxPolicy, StreamChunk, SubagentRunStatus, ToolPolicy,
};
use rustain::domain::ports::{StreamingProvider, SubagentRunner};
use rustain::infrastructure::subagent::{AgentHandle, CascadeKillError, SubagentRegistry};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

struct HangingProvider;

#[async_trait]
impl StreamingProvider for HangingProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<rustain::domain::models::Message>,
        _options: rustain::domain::models::CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, rustain::domain::errors::ProviderError> {
        let chunks = vec![StreamChunk::Text {
            content: "working...".into(),
            parent_tool_use_id: None,
        }];
        let stream = futures::stream::iter(chunks).chain(futures::stream::pending());
        Ok(Box::pin(stream))
    }

    async fn abort(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }

    fn provider_id(&self) -> String {
        "hanging".into()
    }

    fn list_models(&self) -> Vec<rustain::domain::models::provider::ModelDescriptor> {
        vec![]
    }

    async fn health_check(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }
}

async fn make_runner() -> (InProcessSubagentRunner, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let provider = Arc::new(HangingProvider) as Arc<dyn StreamingProvider>;
    let storage = Arc::new(rustain::adapters::filesystem::FileSystemStorage::new(
        tmp.path().to_path_buf(),
    )) as Arc<dyn rustain::domain::ports::StoragePort>;
    let security = Arc::new(rustain::adapters::security_adapter::SecurityAdapter::new(
        std::path::PathBuf::from("."),
    )) as Arc<dyn rustain::domain::ports::SecurityPort>;
    let sandbox = Arc::new(arc_swap::ArcSwap::from_pointee(Arc::new(
        rustain::adapters::sandbox::NoOpSandbox,
    )
        as Arc<dyn rustain::domain::ports::SandboxManager>));
    let tools = Arc::new(rustain::adapters::toolset_adapter::ToolSetAdapter::new(
        std::path::PathBuf::from("."),
        storage.clone(),
        sandbox,
        Arc::new(tokio::sync::RwLock::new(SandboxPolicy::Permissive)),
    )) as Arc<dyn rustain::domain::ports::ToolSetPort>;
    let approval = rustain::domain::services::approval_runtime::ApprovalRuntime::new(
        1024,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let scheduler = rustain::domain::services::tool_scheduler::ToolScheduler::new(
        security.clone(),
        tools.clone(),
        approval.clone(),
        1024,
    );
    let (event_bus, _event_rx) = rustain::infrastructure::runtime::event_bus::EventBus::new(1024);
    let event_bus = Arc::new(event_bus);
    let registry = Arc::new(SubagentRegistry::new());
    let parent_sandbox = Arc::new(tokio::sync::RwLock::new(SandboxPolicy::Permissive));
    let spool = Arc::new(
        rustain::infrastructure::subagent::SubagentSpool::new(tmp.path().join("spool"))
            .await
            .unwrap(),
    );

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
async fn cascade_kill_three_level_subtree() {
    let reg = SubagentRegistry::new();
    let root = AgentId::root();
    let agent_a = AgentId::new();
    let agent_b = AgentId::new();
    let agent_c = AgentId::new();

    // Register parent chain directly (no real child tasks — pure registry test)
    for (agent, parent) in [
        (agent_a.clone(), root.clone()),
        (agent_b.clone(), agent_a.clone()),
        (agent_c.clone(), agent_b.clone()),
    ] {
        let (cmd_tx, mut cmd_rx) = mpsc::channel(1);
        let (status_tx, _status_rx) = watch::channel(SubagentRunStatus::Idle);
        let handle = AgentHandle {
            agent_id: agent.clone(),
            command_tx: cmd_tx,
            depth: 0,
            subagent_type: "test".into(),
            spawned_at: 0,
            status: status_tx,
        };
        reg.register(agent.clone(), parent, handle).await.unwrap();

        // Spawn fake child that reacts to Op::Kill
        let reg_clone = reg.clone();
        let agent_clone = agent.clone();
        tokio::spawn(async move {
            while let Some(op) = cmd_rx.recv().await {
                if matches!(op, Op::Kill) {
                    if let Some(tx) = reg_clone.status_sender(&agent_clone).await {
                        let _: Result<(), watch::error::SendError<SubagentRunStatus>> =
                            tx.send(SubagentRunStatus::Killed);
                    }
                    break;
                }
            }
        });
    }

    // Cascade kill A (generous timeout for CI parallelism)
    let result = reg.cascade_kill(&agent_a, Duration::from_secs(5)).await;
    assert!(result.is_ok(), "cascade_kill failed: {:?}", result);
    let killed = result.unwrap();
    assert_eq!(
        killed,
        vec![agent_c.clone(), agent_b.clone(), agent_a.clone()]
    );

    // Verify registry is empty
    let entries = reg.list().await;
    assert!(
        entries.is_empty(),
        "Registry should be empty after cascade kill"
    );
}

#[tokio::test]
async fn cascade_kill_timeout_returns_partial() {
    let reg = SubagentRegistry::new();
    let root = AgentId::root();
    let a = AgentId::new();
    let (cmd_tx, mut _cmd_rx) = mpsc::channel(1);
    let (status_tx, _status_rx) = watch::channel(SubagentRunStatus::Idle);
    let handle = AgentHandle {
        agent_id: a.clone(),
        command_tx: cmd_tx,
        depth: 1,
        subagent_type: "test".into(),
        spawned_at: 0,
        status: status_tx,
    };
    reg.register(a.clone(), root.clone(), handle).await.unwrap();

    // Keep channel open but don't spawn a child that updates the watch.
    // cascade_kill sends Op::Kill (succeeds), then waits for watch to
    // change to terminal — but nobody updates it, so it times out.
    let result = reg
        .cascade_kill_with_timeout(&a, Duration::from_millis(50))
        .await;
    assert!(
        matches!(result, Err(CascadeKillError::Partial { .. })),
        "Expected Partial error due to timeout, got {:?}",
        result
    );
}

#[tokio::test]
async fn status_bridge_registry_list_reflects_child_status() {
    let (runner, _tmp) = make_runner().await;
    let spec = AgentLaunchSpec {
        prompt: String::from("hello"),
        effective_model: String::from("test-model"),
        tier: rustain::domain::models::ModelTier::CheapAgentic,
        tools_allow: ToolPolicy::InheritFromParent,
        parent_ctx_tokens: 0,
        sandbox_override: None,
        parent_trace: None,
    };

    let cancel = CancellationToken::new();
    let handle = runner.launch(spec, cancel.clone()).await.unwrap();

    // Wait for child to emit RunningFg and bridge to mirror it
    let reg = runner.registry();
    let agent_id = handle.agent_id.clone();

    let mut found_running = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let entries: Vec<_> = reg.list().await;
        if let Some(entry) = entries.iter().find(|e| e.agent_id == agent_id) {
            if entry.current_status == SubagentRunStatus::RunningFg {
                found_running = true;
                break;
            }
        }
    }
    assert!(found_running, "Registry should show RunningFg within 500ms");

    // Cancel the child
    handle.cancel.cancel();

    // Wait for registry to deregister (bridge sees Killed → deregisters)
    let mut found_empty = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let entries: Vec<_> = reg.list().await;
        if entries.is_empty() {
            found_empty = true;
            break;
        }
    }
    assert!(
        found_empty,
        "Registry should be empty within 200ms after cancel"
    );
}
