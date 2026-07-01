use std::sync::Arc;

use async_trait::async_trait;
use rustain::domain::errors::{PermissionError, ToolError};
use rustain::domain::models::tool_call::ApprovalSource;
use rustain::domain::models::{
    AgentLaunchSpec, ModelTier, PermissionMode, SandboxPolicy, ToolPolicy,
};
use rustain::domain::ports::{SecurityPort, SubagentRunner, ToolSetPort};
use rustain::domain::services::permission_chain;
use tokio_util::sync::CancellationToken;

struct DummySecurity;

#[async_trait]
impl SecurityPort for DummySecurity {
    fn check_blocklist(&self, _command: &str) -> Result<(), PermissionError> {
        Ok(())
    }
    fn check_workspace_access(
        &self,
        _path: &std::path::Path,
        _op: rustain::domain::models::FileOperation,
    ) -> Result<rustain::domain::models::PathAccessType, PermissionError> {
        Ok(rustain::domain::models::PathAccessType::Workspace)
    }
    fn current_mode(&self) -> PermissionMode {
        PermissionMode::Normal
    }
    fn set_mode(&self, _mode: PermissionMode) {}
}

struct DummyTools;

#[async_trait]
impl ToolSetPort for DummyTools {
    fn available_tools(&self) -> Vec<rustain::domain::models::ToolDefinition> {
        Vec::new()
    }
    async fn execute(
        &self,
        _tool_name: &str,
        _input: serde_json::Value,
        _cancel: CancellationToken,
    ) -> Result<rustain::domain::models::ToolResult, ToolError> {
        unimplemented!()
    }
}

#[tokio::test]
async fn test_subagent_approval_source_shape() {
    // Verify that permission_chain::check_with_source receives and acts on
    // ApprovalSource::ForegroundSubagent correctly (foundation for Story 10.7 wiring).
    let security = Arc::new(DummySecurity) as Arc<dyn SecurityPort>;
    let tools = Arc::new(DummyTools) as Arc<dyn ToolSetPort>;

    let source = ApprovalSource::ForegroundSubagent {
        conversation_id: "child-conv-1".into(),
        parent_tool_call_id: "tc-parent-1".into(),
        subagent_type: "code-reviewer".into(),
    };

    let decision = permission_chain::check_with_source(
        security.as_ref(),
        "Bash",
        &serde_json::json!({"command": "echo test"}),
        None,
        None,
        tools.as_ref(),
        Some(&source),
    )
    .await;

    // In Normal mode with Elevated risk (Bash), the decision should be Prompt,
    // which internally routes through the approval runtime with the correct source.
    assert!(
        matches!(
            decision,
            permission_chain::PermissionDecision::Prompt { .. }
        ),
        "Expected Prompt decision for Bash in Normal mode with ForegroundSubagent source, got {:?}",
        decision
    );
}

#[tokio::test]
async fn test_in_process_subagent_runner_launch_returns_handle() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = Arc::new(rustain::adapters::noop::NoOpProvider)
        as Arc<dyn rustain::domain::ports::StreamingProvider>;
    let storage = Arc::new(rustain::adapters::filesystem::FileSystemStorage::new(
        tmp.path().to_path_buf(),
    )) as Arc<dyn rustain::domain::ports::StoragePort>;
    let security = Arc::new(rustain::adapters::security_adapter::SecurityAdapter::new(
        tmp.path().to_path_buf(),
    )) as Arc<dyn rustain::domain::ports::SecurityPort>;
    let sandbox = Arc::new(arc_swap::ArcSwap::from_pointee(Arc::new(
        rustain::adapters::sandbox::NoOpSandbox,
    )
        as Arc<dyn rustain::domain::ports::SandboxManager>));
    let tools = Arc::new(rustain::adapters::toolset_adapter::ToolSetAdapter::new(
        tmp.path().to_path_buf(),
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
    let (event_bus, _rx) = rustain::infrastructure::runtime::event_bus::EventBus::new(1024);
    let registry = Arc::new(rustain::infrastructure::subagent::NodeTree::new());
    let parent_sandbox = Arc::new(tokio::sync::RwLock::new(SandboxPolicy::Permissive));
    let spool = Arc::new(
        rustain::infrastructure::subagent::SubagentSpool::new(tmp.path().join("spool"))
            .await
            .unwrap(),
    );
    let root_authority =
        rustain::domain::models::CapabilityToken::r1_root(rustain::domain::models::AgentId::root());
    let authority_ledger = Arc::new(
        rustain::domain::services::authority_ledger::AuthorityLedger::new(root_authority.clone()),
    );
    let authority =
        Arc::new(rustain::adapters::authority::InProcessAuthorityProvider::new(authority_ledger))
            as Arc<dyn rustain::domain::ports::AuthorityProvider>;

    let runner = rustain::adapters::subagent::InProcessSubagentRunner::new(
        provider,
        storage,
        security,
        tools,
        approval,
        scheduler,
        Arc::new(event_bus),
        registry,
        parent_sandbox,
        spool,
        authority,
        root_authority,
    );

    let spec = AgentLaunchSpec {
        prompt: String::from("hello"),
        effective_model: String::from("noop"),
        tier: ModelTier::CheapAgentic,
        tools_allow: ToolPolicy::InheritFromParent,
        parent_ctx_tokens: 0,
        sandbox_override: None,
        parent_trace: None,
        isolated: false,
    };

    let cancel = CancellationToken::new();
    let handle = runner.launch(spec, cancel.clone()).await.unwrap();
    assert!(!handle.task_id.is_empty());
    assert_eq!(handle.subagent_type, "in-process");
    handle.cancel.cancel();
}
