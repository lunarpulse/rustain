//! Performance test: per-call scheduler overhead.
//!
//! Run: `cargo test --test scheduler_overhead -- --ignored`

use std::sync::Arc;
use std::time::Duration;

use rustain::domain::errors::{PermissionError, ToolError};
use rustain::domain::models::tool_call::{ApprovalSource, ToolCallRequest};
use rustain::domain::models::{
    ApprovalDecision, FileOperation, PathAccessType, PermissionMode, ToolDefinition, ToolResult,
};
use rustain::domain::ports::{SecurityPort, ToolSetPort};
use rustain::domain::services::tool_scheduler::ToolScheduler;
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

struct NoOpSecurity;

#[async_trait]
impl SecurityPort for NoOpSecurity {
    fn check_blocklist(&self, _command: &str) -> Result<(), PermissionError> {
        Ok(())
    }
    fn check_workspace_access(
        &self,
        _path: &std::path::Path,
        _op: FileOperation,
    ) -> Result<PathAccessType, PermissionError> {
        Ok(PathAccessType::Workspace)
    }
    async fn request_permission(
        &self,
        _tool_name: &str,
        _tool_input: &serde_json::Value,
    ) -> Result<ApprovalDecision, PermissionError> {
        Ok(ApprovalDecision::Allow)
    }
    fn current_mode(&self) -> PermissionMode {
        PermissionMode::Yolo
    }
    fn set_mode(&self, _mode: PermissionMode) {}
}

struct NoOpToolSet;

#[async_trait]
impl ToolSetPort for NoOpToolSet {
    fn available_tools(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "NoOp".to_string(),
            description: "noop".to_string(),
            input_schema: serde_json::json!({}),
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
            content: "ok".to_string(),
            is_error: false,
        })
    }
}

#[tokio::test]
#[ignore = "performance test — run manually with `cargo test --test scheduler_overhead -- --ignored`"]
async fn scheduler_overhead_p99() {
    let security: Arc<dyn SecurityPort> = Arc::new(NoOpSecurity);
    let tools: Arc<dyn ToolSetPort> = Arc::new(NoOpToolSet);
    let sched = ToolScheduler::new(security, tools, 1024);

    let batch_size = 1;
    let iterations = 10_000;
    let mut latencies = Vec::with_capacity(iterations);

    for _ in 0..iterations {
        let req = ToolCallRequest {
            id: "tc-1".into(),
            tool_name: "NoOp".into(),
            input: serde_json::json!({}),
        };
        let start = std::time::Instant::now();
        let _ = sched
            .clone()
            .schedule(
                ApprovalSource::ForegroundTurn {
                    conversation_id: "c1".into(),
                },
                vec![req; batch_size],
                CancellationToken::new(),
                None,
            )
            .await;
        latencies.push(start.elapsed());
    }

    latencies.sort();
    let p99_idx = (latencies.len() as f64 * 0.99) as usize;
    let p99 = latencies[p99_idx];

    assert!(
        p99 < Duration::from_micros(100),
        "P99 latency {:?} exceeds 100 µs threshold",
        p99
    );
}
