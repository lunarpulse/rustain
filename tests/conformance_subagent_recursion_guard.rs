use async_trait::async_trait;
use rustain::domain::errors::{PermissionError, ToolError};
use rustain::domain::models::tool_call::ApprovalSource;
use rustain::domain::models::{ActiveSkill, PermissionMode, SkillSource};
use rustain::domain::ports::{SecurityPort, ToolSetPort};
use rustain::domain::services::permission_chain;
use std::sync::Arc;
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
        PermissionMode::Yolo
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
async fn test_subagent_recursion_guard_denies_task_tool() {
    let security = Arc::new(DummySecurity) as Arc<dyn SecurityPort>;
    let tools = Arc::new(DummyTools) as Arc<dyn ToolSetPort>;

    let source = ApprovalSource::ForegroundSubagent {
        conversation_id: "conv-1".into(),
        parent_tool_call_id: "tc-1".into(),
        subagent_type: "explore".into(),
    };

    let active_skills = vec![ActiveSkill {
        name: "test-skill".into(),
        directory: std::path::PathBuf::from("/tmp/test"),
        allowed_tools: Some(vec!["Read".into(), "Write".into()]),
        body: String::new(),
        arguments: String::new(),
        activation_depth: 0,
        source: SkillSource::WorkspaceAgents,
    }];

    let decision = permission_chain::check_with_source(
        security.as_ref(),
        "task",
        &serde_json::json!({"prompt": "hello"}),
        Some(&active_skills),
        None,
        tools.as_ref(),
        Some(&source),
    )
    .await;

    match decision {
        permission_chain::PermissionDecision::Deny(reason) => {
            assert!(
                reason.contains("recursion guard"),
                "Expected recursion guard denial, got: {}",
                reason
            );
        }
        other => panic!("Expected Deny, got {:?}", other),
    }
}
