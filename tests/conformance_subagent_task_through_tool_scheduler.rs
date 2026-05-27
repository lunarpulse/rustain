use std::sync::Arc;

use async_trait::async_trait;
use rustain::domain::errors::{PermissionError, ToolError};
use rustain::domain::models::tool_call::{ApprovalSource, ToolCallRequest};
use rustain::domain::models::{PermissionMode, SandboxPolicy, ToolDefinition, ToolResult};
use rustain::domain::ports::{SecurityPort, ToolSetPort};
use rustain::domain::services::tool_scheduler::ToolScheduler;
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

// Stub toolset that includes a "task" tool representing subagent dispatch
struct TaskToolSet;

#[async_trait]
impl ToolSetPort for TaskToolSet {
    fn available_tools(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "task".to_string(),
            description: "Dispatch a subagent task".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": { "type": "string" }
                },
                "required": ["prompt"]
            }),
            parallel_safe: false,
        }]
    }
    async fn execute(
        &self,
        _tool_name: &str,
        _input: serde_json::Value,
        _cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult {
            tool_use_id: String::new(),
            content: "subagent completed".to_string(),
            is_error: false,
        })
    }
}

#[tokio::test]
async fn test_subagent_task_traverses_scheduler_fsm() {
    let security = Arc::new(DummySecurity) as Arc<dyn SecurityPort>;
    let tools = Arc::new(TaskToolSet) as Arc<dyn ToolSetPort>;
    let approval = rustain::domain::services::approval_runtime::ApprovalRuntime::new(
        1024,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let scheduler = ToolScheduler::new(security, tools, approval, 1024);

    let source = ApprovalSource::ForegroundTurn {
        conversation_id: "conv-1".into(),
    };
    let req = ToolCallRequest {
        id: "tc-1".into(),
        tool_name: "task".into(),
        input: serde_json::json!({"prompt": "hello"}),
    };

    let cancel = CancellationToken::new();
    let calls = scheduler.schedule(source, vec![req], cancel, None).await;

    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert!(
        matches!(
            call,
            rustain::domain::models::tool_call::ToolCall::Success { .. }
        ),
        "Expected Success terminal state for task tool dispatch, got {:?}",
        call
    );
}
