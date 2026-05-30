use std::sync::Arc;

use async_trait::async_trait;
use rustain::domain::errors::{PermissionError, ToolError};
use rustain::domain::models::tool_call::{ApprovalSource, ToolCallRequest};
use rustain::domain::models::{PermissionMode, ToolDefinition, ToolResult};
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

// Story 10.7 — real schema: { description, prompt, subagent_type?, task_id?, tier_hint? }
struct TaskToolSet;

#[async_trait]
impl ToolSetPort for TaskToolSet {
    fn available_tools(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "task".to_string(),
            description: "Dispatch an isolated subagent task and return the final result text (bounded 8 KB tail).".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "description": { "type": "string", "description": "Short description of the task" },
                    "prompt": { "type": "string", "description": "Full prompt to send to the subagent" },
                    "subagent_type": { "type": "string", "description": "Agent definition name (from .claude/agents/); omit for default worker" },
                    "task_id": { "type": "string", "description": "Optional session id for resuming a prior task" },
                    "tier_hint": { "type": "string", "description": "Optional model tier hint (e.g. 'cheap', 'flagship')" }
                },
                "required": ["description", "prompt"]
            }),
            parallel_safe: true,
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
        input: serde_json::json!({"description": "test", "prompt": "hello"}),
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

#[tokio::test]
async fn test_recursion_guard_denies_task_from_subagent() {
    let security = Arc::new(DummySecurity) as Arc<dyn SecurityPort>;
    let tools = Arc::new(TaskToolSet) as Arc<dyn ToolSetPort>;
    let approval = rustain::domain::services::approval_runtime::ApprovalRuntime::new(
        1024,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let scheduler = ToolScheduler::new(security, tools, approval, 1024);

    let source = ApprovalSource::ForegroundSubagent {
        conversation_id: "conv-1".into(),
        parent_tool_call_id: "tc-parent".into(),
        subagent_type: "worker".into(),
    };
    let req = ToolCallRequest {
        id: "tc-2".into(),
        tool_name: "task".into(),
        input: serde_json::json!({"description": "test", "prompt": "hello"}),
    };

    let cancel = CancellationToken::new();
    let calls = scheduler.schedule(source, vec![req], cancel, None).await;

    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert!(
        matches!(
            call,
            rustain::domain::models::tool_call::ToolCall::Error { .. }
        ),
        "Expected Error terminal state for task tool from subagent (recursion guard), got {:?}",
        call
    );
}

#[test]
fn test_no_new_app_event_variant_for_task() {
    // AC-10-7-8: assert no new Task* / Subagent*Dispatch* event variant was introduced.
    // The easiest way is to grep the events.rs source for forbidden patterns.
    let events_src = include_str!("../src/domain/events.rs");
    let forbidden = [
        "TaskSpawned",
        "TaskCompleted",
        "SubagentDispatch",
        "SubagentTool",
    ];
    for pat in &forbidden {
        assert!(
            !events_src.contains(pat),
            "events.rs must NOT contain a '{}' variant (ADR-10-2)",
            pat
        );
    }
}
