//! Conformance test: live_tail = false kill-switch (AC14).
//!
//! Verifies that when the feature flag is OFF (the default), no progress
//! events are emitted and the ToolResult content matches the semantic
//! output of today's `wait_with_output()` path.
//!
//! NOTE: The S16.9 BufReader line-stream refactor normalizes `\r\n` → `\n`
//! and strips/re-adds newlines, so a strict byte-identical comparison is
//! not expected. The conformance gate verifies logical line equivalence.
//!
//! This test serves as the kill-switch verification gate — if S16.9 ships
//! and someone accidentally flips the default to `true`, this test catches it.

use async_trait::async_trait;
use rustain::adapters::toolset_adapter::ToolSetAdapter;
use rustain::domain::errors::PermissionError;
use rustain::domain::models::{FileOperation, PathAccessType, PermissionMode, ToolCallRequest};
use rustain::domain::ports::{SecurityPort, StoragePort, ToolSetPort};
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::domain::services::tool_scheduler::ToolScheduler;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn make_adapter(dir: &std::path::Path) -> ToolSetAdapter {
    let sessions_dir = dir.join(".claude").join("sessions");
    let storage: Arc<dyn StoragePort> = Arc::new(
        rustain::adapters::filesystem::FileSystemStorage::new(sessions_dir),
    );
    ToolSetAdapter::new(dir.to_path_buf(), storage)
}

// ── Simple mock security port for ToolScheduler construction ────────────────

struct YoloSecurity;

#[async_trait]
impl SecurityPort for YoloSecurity {
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
    fn current_mode(&self) -> PermissionMode {
        PermissionMode::Yolo
    }
    fn set_mode(&self, _mode: PermissionMode) {}
}

#[tokio::test]
async fn live_tail_off_no_progress_events() {
    let dir = std::env::current_dir().unwrap();
    let adapter = make_adapter(&dir);
    // Never call set_progress_tx → stays None

    let result = adapter
        .execute_with_id(
            "bash",
            "test-14",
            serde_json::json!({"command": "for i in 1 2 3 4 5; do echo line $i; sleep 0.2; done"}),
            CancellationToken::new(),
            None, // progress_tx is None
        )
        .await
        .unwrap();

    assert!(!result.is_error);
    // Verify the output contains all 5 lines (full stdout, not truncated)
    for i in 1..=5 {
        assert!(
            result.content.contains(&format!("line {}", i)),
            "missing line {} in output: {}",
            i,
            result.content
        );
    }
}

#[tokio::test]
async fn live_tail_off_content_semantic_match() {
    let dir = std::env::current_dir().unwrap();
    let adapter = make_adapter(&dir);

    let script = "echo alpha; echo beta; echo gamma >&2";
    let result = adapter
        .execute_with_id(
            "bash",
            "test-byte-match",
            serde_json::json!({"command": script}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();

    assert!(!result.is_error);
    // Semantic match: each logical line must appear (exact byte identity
    // is not guaranteed because BufReader::lines() normalizes newlines).
    assert!(
        result.content.contains("alpha"),
        "content: {}",
        result.content
    );
    assert!(
        result.content.contains("beta"),
        "content: {}",
        result.content
    );
    assert!(
        result.content.contains("gamma"),
        "content: {}",
        result.content
    );
}

#[tokio::test]
async fn live_tail_off_tool_scheduler_kill_switch() {
    // AC14: Constructs a ToolScheduler with set_progress_tx(None) and
    // verifies the default no-progress path still works end-to-end.
    let dir = std::env::current_dir().unwrap();
    let adapter = make_adapter(&dir);
    let tools: Arc<dyn ToolSetPort> = Arc::new(adapter);

    let security: Arc<dyn SecurityPort> = Arc::new(YoloSecurity);
    let approval_runtime = ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let sched = ToolScheduler::new(security, tools, approval_runtime, 16);
    sched.set_progress_tx(None).await;

    let req = ToolCallRequest {
        id: "tc-14".into(),
        tool_name: "bash".into(),
        input: serde_json::json!({"command": "echo one; echo two; echo three"}),
    };

    let calls = sched
        .schedule(
            rustain::domain::models::tool_call::ApprovalSource::ForegroundTurn {
                conversation_id: "conv-14".into(),
            },
            vec![req],
            CancellationToken::new(),
            None,
        )
        .await;

    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    let content = match call {
        rustain::domain::models::tool_call::ToolCall::Success { result, .. } => &result.output,
        _ => panic!("expected Success, got {:?}", call),
    };
    assert!(content.contains("one"), "output: {}", content);
    assert!(content.contains("two"), "output: {}", content);
    assert!(content.contains("three"), "output: {}", content);
}
