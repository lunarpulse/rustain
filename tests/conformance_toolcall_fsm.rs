#![allow(dead_code)] // AI-12.1: test fixture scaffolding
//! Conformance tests for `ToolCall` 7-variant FSM and `ToolScheduler` pipeline.
//!
//! Source of truth:
//! - `_bmad-output/planning-artifacts/architecture/adr/ADR-06-02-toolcall-enum-fsm.md`
//! - `_bmad-output/implementation-artifacts/6-0b-toolscheduler-toolcall-fsm.md`

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rustain::domain::errors::{PermissionError, ToolError};
use rustain::domain::models::tool_call::{
    ApprovalSource, RequestId, ToolCall, ToolCallRequest, ToolCallResult, ToolCallTransition,
};
use rustain::domain::models::{
    FileOperation, PathAccessType, PermissionMode, ToolDefinition, ToolResult,
};
use rustain::domain::ports::{SecurityPort, ToolSetPort};
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::domain::services::tool_scheduler::ToolScheduler;
use tokio_util::sync::CancellationToken;

// ── AC1: Enum shape + serde round-trip ──────────────────────────────────────

#[test]
fn ac1_enum_shape_and_serde_round_trip() {
    let req = ToolCallRequest {
        id: "tc-1".into(),
        tool_name: "Read".into(),
        input: serde_json::json!({"file_path": "/tmp/x"}),
    };
    let variants = vec![
        ToolCall::Validating {
            id: "a".into(),
            request: req.clone(),
            started_at: 1714000000,
        },
        ToolCall::Scheduled {
            id: "a".into(),
            request: req.clone(),
        },
        ToolCall::AwaitingApproval {
            id: "a".into(),
            request: req.clone(),
            approval_id: RequestId("req-1".into()),
        },
        ToolCall::Executing {
            id: "a".into(),
            request: req.clone(),
            started_at: 1714000000,
        },
        ToolCall::Success {
            id: "a".into(),
            request: req.clone(),
            result: ToolCallResult {
                output: "ok".into(),
                is_error: false,
                duration_ms: 42,
            },
        },
        ToolCall::Error {
            id: "a".into(),
            request: req.clone(),
            error: "boom".into(),
        },
        ToolCall::Cancelled {
            id: "a".into(),
            request: req.clone(),
            reason: "user-cancel".into(),
        },
    ];
    assert_eq!(variants.len(), 7);
    for call in variants {
        let json = serde_json::to_string(&call).unwrap();
        let back: ToolCall = serde_json::from_str(&json).unwrap();
        assert_eq!(call, back, "round-trip failed for {:?}", call);
    }
}

// ── AC2: Scheduler API surface ──────────────────────────────────────────────

#[tokio::test]
async fn ac2_scheduler_api_surface() {
    let security: Arc<dyn SecurityPort> = Arc::new(YoloSecurity);
    let tools: Arc<dyn ToolSetPort> = Arc::new(SleepToolSet {
        delay_ms: 0,
        parallel_safe: true,
    });
    let approval_runtime = ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let sched = ToolScheduler::new(security, tools, approval_runtime, 16);
    let mut rx = sched.subscribe();

    let req = ToolCallRequest {
        id: "tc-1".into(),
        tool_name: "Sleep".into(),
        input: serde_json::json!({}),
    };
    let result = sched
        .schedule(
            ApprovalSource::ForegroundTurn {
                conversation_id: "c1".into(),
            },
            vec![req],
            CancellationToken::new(),
            None,
        )
        .await;
    assert_eq!(result.len(), 1);
    assert!(matches!(result[0], ToolCall::Success { .. }));

    // Verify broadcast receiver saw transitions
    let mut seen = vec![];
    while let Ok(t) = rx.try_recv() {
        seen.push(t.call);
    }
    assert!(
        matches!(
            &seen[..],
            [
                ToolCall::Validating { .. },
                ToolCall::Scheduled { .. },
                ToolCall::Executing { .. },
                ToolCall::Success { .. }
            ]
        ),
        "unexpected transitions: {:?}",
        seen
    );
}

// ── AC3: Legal transitions ──────────────────────────────────────────────────

#[tokio::test]
async fn ac3_transition_happy_path() {
    let (sched, mut rx) = make_test_scheduler(PermissionMode::Yolo, 0);
    let result = run_one(&sched, "Sleep", serde_json::json!({})).await;
    assert!(matches!(result, ToolCall::Success { .. }));
    assert_sequence(
        &mut rx,
        &["validating", "scheduled", "executing", "success"],
    );
}

#[tokio::test]
async fn ac3_transition_with_approval() {
    // Normal mode + Elevated risk (Sleep is unknown => Elevated => Prompt)
    let security: Arc<dyn SecurityPort> = Arc::new(MockSecurity {
        mode: PermissionMode::Normal,
    });
    let tools: Arc<dyn ToolSetPort> = Arc::new(SleepToolSet {
        delay_ms: 0,
        parallel_safe: true,
    });
    let approval_runtime = ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let sched = ToolScheduler::new(security, tools, approval_runtime.clone(), 16);
    let mut rx = sched.subscribe();

    // Subscribe to approval events BEFORE spawning run_one
    let mut events = approval_runtime.subscribe();
    let sched2 = sched.clone();
    let handle =
        tokio::spawn(async move { run_one(&sched2, "Sleep", serde_json::json!({})).await });

    let event = events.recv().await.unwrap();
    let id = match event {
        rustain::domain::services::approval_runtime::ApprovalRuntimeEvent::Requested {
            id, ..
        } => id,
        _ => panic!("expected Requested event"),
    };
    approval_runtime
        .resolve(&id, rustain::domain::models::ApprovalOutcome::Once)
        .await;

    let result = handle.await.unwrap();
    assert!(matches!(result, ToolCall::Success { .. }));
    assert_sequence(
        &mut rx,
        &[
            "validating",
            "scheduled",
            "awaiting_approval",
            "executing",
            "success",
        ],
    );
}

#[tokio::test]
async fn ac3_transition_invalid_input_fails_fast() {
    let tools: Arc<dyn ToolSetPort> = Arc::new(ValidatingToolSet);
    let security: Arc<dyn SecurityPort> = Arc::new(MockSecurity {
        mode: PermissionMode::Yolo,
    });
    let approval_runtime = ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let sched = ToolScheduler::new(security, tools, approval_runtime, 16);
    let mut rx = sched.subscribe();
    let result = run_one(&sched, "Sleep", serde_json::json!({})).await;
    assert!(matches!(result, ToolCall::Error { .. }));
    assert_sequence(&mut rx, &["validating", "error"]);
}

#[tokio::test]
async fn ac3_transition_policy_denial() {
    // Plan mode blocks Standard tools (mode_risk_outcome returns Some(false))
    let (sched, mut rx) = make_test_scheduler(PermissionMode::Plan, 0);
    let result = run_one(&sched, "Sleep", serde_json::json!({})).await;
    assert!(matches!(result, ToolCall::Error { .. }));
    assert_sequence(&mut rx, &["validating", "scheduled", "error"]);
}

#[tokio::test]
async fn ac3_transition_cancel_during_execute() {
    let started = Arc::new(tokio::sync::Notify::new());
    let tools: Arc<dyn ToolSetPort> = Arc::new(NotifyingSleepToolSet {
        started: started.clone(),
    });
    let security: Arc<dyn SecurityPort> = Arc::new(MockSecurity {
        mode: PermissionMode::Yolo,
    });
    let approval_runtime = ApprovalRuntime::new(
        64,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let sched = ToolScheduler::new(security, tools, approval_runtime, 64);
    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    let req = ToolCallRequest {
        id: "tc-1".into(),
        tool_name: "Sleep".into(),
        input: serde_json::json!({}),
    };
    let sched2 = sched.clone();
    let handle = tokio::spawn(async move {
        sched2
            .schedule(
                ApprovalSource::ForegroundTurn {
                    conversation_id: "c1".into(),
                },
                vec![req],
                cancel,
                None,
            )
            .await
    });
    started.notified().await;
    cancel2.cancel();
    let mut results = handle.await.unwrap();
    let result = results.pop().unwrap();
    assert!(
        matches!(result, ToolCall::Cancelled { ref reason, .. } if reason == "cancelled-during-execute")
    );
}

#[tokio::test]
async fn ac3_transition_cancel_during_approval() {
    let security: Arc<dyn SecurityPort> = Arc::new(DelaySecurity {
        delay_ms: 10_000,
        mode: PermissionMode::Normal,
    });
    let tools: Arc<dyn ToolSetPort> = Arc::new(SleepToolSet {
        delay_ms: 0,
        parallel_safe: true,
    });
    let approval_runtime = ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let sched = ToolScheduler::new(security, tools, approval_runtime, 16);
    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel2.cancel();
    });
    let req = ToolCallRequest {
        id: "tc-1".into(),
        tool_name: "Sleep".into(),
        input: serde_json::json!({}),
    };
    let mut results = sched
        .schedule(
            ApprovalSource::ForegroundTurn {
                conversation_id: "c1".into(),
            },
            vec![req],
            cancel,
            None,
        )
        .await;
    let result = results.pop().unwrap();
    assert!(
        matches!(result, ToolCall::Cancelled { ref reason, .. } if reason == "cancelled-during-approval")
    );
}

#[tokio::test]
async fn ac3_transition_user_rejection_with_feedback() {
    let security: Arc<dyn SecurityPort> = Arc::new(MockSecurity {
        mode: PermissionMode::Normal,
    });
    let tools: Arc<dyn ToolSetPort> = Arc::new(SleepToolSet {
        delay_ms: 0,
        parallel_safe: true,
    });
    let approval_runtime = ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let sched = ToolScheduler::new(security, tools, approval_runtime.clone(), 16);
    let mut rx = sched.subscribe();

    // Subscribe to approval events BEFORE spawning run_one
    let mut events = approval_runtime.subscribe();
    let sched2 = sched.clone();
    let handle =
        tokio::spawn(async move { run_one(&sched2, "Sleep", serde_json::json!({})).await });

    let event = events.recv().await.unwrap();
    let id = match event {
        rustain::domain::services::approval_runtime::ApprovalRuntimeEvent::Requested {
            id, ..
        } => id,
        _ => panic!("expected Requested event"),
    };
    approval_runtime
        .resolve(
            &id,
            rustain::domain::models::ApprovalOutcome::Reject {
                feedback: Some("nope".into()),
            },
        )
        .await;

    let result = handle.await.unwrap();
    assert!(matches!(result, ToolCall::Error { .. }));
    if let ToolCall::Error { error, .. } = &result {
        assert!(
            error.contains("User feedback") || error.contains("nope"),
            "expected feedback in error payload, got: {}",
            error
        );
    }
    assert_sequence(
        &mut rx,
        &["validating", "scheduled", "awaiting_approval", "error"],
    );
}

// ── AC4: Parallelism ────────────────────────────────────────────────────────

#[tokio::test]
async fn ac4_parallel_batch_all_safe() {
    let (sched, _rx) = make_test_scheduler(PermissionMode::Yolo, 50);
    let batch: Vec<ToolCallRequest> = (0..3)
        .map(|i| ToolCallRequest {
            id: format!("tc-{}", i),
            tool_name: "Sleep".into(),
            input: serde_json::json!({}),
        })
        .collect();
    let start = std::time::Instant::now();
    let results = sched
        .schedule(
            ApprovalSource::ForegroundTurn {
                conversation_id: "c1".into(),
            },
            batch,
            CancellationToken::new(),
            None,
        )
        .await;
    let elapsed = start.elapsed();
    assert_eq!(results.len(), 3);
    assert!(
        elapsed < Duration::from_millis(100),
        "parallel batch took {:?}, expected < 100 ms",
        elapsed
    );
}

#[tokio::test]
async fn ac4_sequential_when_any_unsafe() {
    let (_sched, _rx) = make_test_scheduler(PermissionMode::Yolo, 50);
    // Override tools with parallel_safe=false
    let tools: Arc<dyn ToolSetPort> = Arc::new(SleepToolSet {
        delay_ms: 50,
        parallel_safe: false,
    });
    let security: Arc<dyn SecurityPort> = Arc::new(MockSecurity {
        mode: PermissionMode::Yolo,
    });
    let approval_runtime = ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let sched = ToolScheduler::new(security, tools, approval_runtime, 16);
    let batch: Vec<ToolCallRequest> = (0..3)
        .map(|i| ToolCallRequest {
            id: format!("tc-{}", i),
            tool_name: "Sleep".into(),
            input: serde_json::json!({}),
        })
        .collect();
    let start = std::time::Instant::now();
    let results = sched
        .schedule(
            ApprovalSource::ForegroundTurn {
                conversation_id: "c1".into(),
            },
            batch,
            CancellationToken::new(),
            None,
        )
        .await;
    let elapsed = start.elapsed();
    assert_eq!(results.len(), 3);
    assert!(
        elapsed >= Duration::from_millis(130),
        "sequential batch took {:?}, expected >= 130 ms",
        elapsed
    );
}

#[test]
fn ac4_builtin_parallel_safe_flags() {
    use rustain::adapters::toolset_adapter::ToolSetAdapter;
    use rustain::domain::ports::StoragePort;
    let tmp = tempfile::tempdir().unwrap();
    let storage: Arc<dyn StoragePort> = Arc::new(
        rustain::adapters::filesystem::FileSystemStorage::new(tmp.path().to_path_buf()),
    );
    let adapter = ToolSetAdapter::new(
        tmp.path().to_path_buf(),
        storage,
        Arc::new(arc_swap::ArcSwap::from_pointee(
            Arc::new(rustain::adapters::sandbox::NoOpSandbox)
                as Arc<dyn rustain::domain::ports::SandboxManager>,
        )),
        Arc::new(tokio::sync::RwLock::new(
            rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        )),
    );
    let defs = adapter.available_tools();
    let map: std::collections::HashMap<String, bool> = defs
        .iter()
        .map(|d| (d.name.clone(), d.parallel_safe))
        .collect();
    assert_eq!(map.get("Read"), Some(&true));
    assert_eq!(map.get("Bash"), Some(&false));
    assert_eq!(map.get("Write"), Some(&false));
    assert_eq!(map.get("activate_skill"), Some(&true));
}

// ── AC6: Policy integration ─────────────────────────────────────────────────

#[tokio::test]
async fn ac6_policy_allow_proceeds() {
    let (sched, mut rx) = make_test_scheduler(PermissionMode::Yolo, 0);
    let result = run_one(&sched, "Sleep", serde_json::json!({})).await;
    assert!(matches!(result, ToolCall::Success { .. }));
    assert_sequence(
        &mut rx,
        &["validating", "scheduled", "executing", "success"],
    );
}

#[tokio::test]
async fn ac6_policy_deny_emits_error() {
    let (sched, mut rx) = make_test_scheduler(PermissionMode::Plan, 0);
    let result = run_one(&sched, "Sleep", serde_json::json!({})).await;
    assert!(matches!(result, ToolCall::Error { .. }));
    assert_sequence(&mut rx, &["validating", "scheduled", "error"]);
}

#[tokio::test]
async fn ac6_policy_ask_routes_to_approval_runtime() {
    let security: Arc<dyn SecurityPort> = Arc::new(MockSecurity {
        mode: PermissionMode::Normal,
    });
    let tools: Arc<dyn ToolSetPort> = Arc::new(SleepToolSet {
        delay_ms: 0,
        parallel_safe: true,
    });
    let approval_runtime = ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let sched = ToolScheduler::new(security, tools, approval_runtime.clone(), 16);
    let mut rx = sched.subscribe();

    // Subscribe to approval events BEFORE spawning run_one
    let mut events = approval_runtime.subscribe();
    let sched2 = sched.clone();
    let handle =
        tokio::spawn(async move { run_one(&sched2, "Sleep", serde_json::json!({})).await });

    let event = events.recv().await.unwrap();
    let id = match event {
        rustain::domain::services::approval_runtime::ApprovalRuntimeEvent::Requested {
            id, ..
        } => id,
        _ => panic!("expected Requested event"),
    };
    approval_runtime
        .resolve(&id, rustain::domain::models::ApprovalOutcome::Once)
        .await;

    let result = handle.await.unwrap();
    assert!(matches!(result, ToolCall::Success { .. }));
    assert_sequence(
        &mut rx,
        &[
            "validating",
            "scheduled",
            "awaiting_approval",
            "executing",
            "success",
        ],
    );
}

// ── AC7: turn.rs migration cleanliness ──────────────────────────────────────

#[test]
fn ac7_turn_rs_delegates_to_scheduler() {
    let turn_rs = std::fs::read_to_string("src/infrastructure/runtime/turn.rs").unwrap();
    // permission_chain::check should not appear directly in turn.rs anymore
    assert!(
        !turn_rs.contains("permission_chain::check"),
        "turn.rs still contains direct permission_chain::check call"
    );
    // tools.execute should not appear directly in turn.rs anymore
    assert!(
        !turn_rs.contains("tools.execute"),
        "turn.rs still contains direct tools.execute call"
    );
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn make_test_scheduler(
    mode: PermissionMode,
    delay_ms: u64,
) -> (
    Arc<ToolScheduler>,
    tokio::sync::broadcast::Receiver<ToolCallTransition>,
) {
    let security: Arc<dyn SecurityPort> = Arc::new(MockSecurity { mode });
    let tools: Arc<dyn ToolSetPort> = Arc::new(SleepToolSet {
        delay_ms,
        parallel_safe: true,
    });
    let approval_runtime = ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let sched = ToolScheduler::new(security, tools, approval_runtime, 16);
    let rx = sched.subscribe();
    (sched, rx)
}

async fn run_one(
    sched: &Arc<ToolScheduler>,
    tool_name: &str,
    input: serde_json::Value,
) -> ToolCall {
    let req = ToolCallRequest {
        id: "tc-1".into(),
        tool_name: tool_name.into(),
        input,
    };
    let mut results = sched
        .clone()
        .schedule(
            ApprovalSource::ForegroundTurn {
                conversation_id: "c1".into(),
            },
            vec![req],
            CancellationToken::new(),
            None,
        )
        .await;
    results.pop().unwrap()
}

fn assert_sequence(
    rx: &mut tokio::sync::broadcast::Receiver<ToolCallTransition>,
    expected: &[&str],
) {
    let mut seen = vec![];
    while let Ok(t) = rx.try_recv() {
        let status = match t.call {
            ToolCall::Validating { .. } => "validating",
            ToolCall::Scheduled { .. } => "scheduled",
            ToolCall::AwaitingApproval { .. } => "awaiting_approval",
            ToolCall::Executing { .. } => "executing",
            ToolCall::Success { .. } => "success",
            ToolCall::Error { .. } => "error",
            ToolCall::Cancelled { .. } => "cancelled",
        };
        seen.push(status);
    }
    assert_eq!(
        seen, expected,
        "transition sequence mismatch: got {:?}, expected {:?}",
        seen, expected
    );
}

struct MockSecurity {
    mode: PermissionMode,
}

#[async_trait]
impl SecurityPort for MockSecurity {
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
        self.mode
    }
    fn set_mode(&self, _mode: PermissionMode) {}
}

struct DelaySecurity {
    delay_ms: u64,
    mode: PermissionMode,
}

struct DenyFeedbackSecurity;

#[async_trait]
impl SecurityPort for DenyFeedbackSecurity {
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
        PermissionMode::Normal
    }
    fn set_mode(&self, _mode: PermissionMode) {}
}

#[async_trait]
impl SecurityPort for DelaySecurity {
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
        self.mode
    }
    fn set_mode(&self, _mode: PermissionMode) {}
}

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

struct SleepToolSet {
    delay_ms: u64,
    parallel_safe: bool,
}

#[async_trait]
impl ToolSetPort for SleepToolSet {
    fn available_tools(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "Sleep".to_string(),
            description: "sleep".to_string(),
            input_schema: serde_json::json!({}),
            parallel_safe: self.parallel_safe,
        }]
    }
    async fn execute(
        &self,
        _tool_name: &str,
        _input: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        if self.delay_ms > 0 {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(self.delay_ms)) => {},
                _ = cancel.cancelled() => return Err(ToolError::Cancelled),
            }
        }
        Ok(ToolResult {
            tool_use_id: String::new(),
            content: "done".to_string(),
            is_error: false,
        })
    }
}

struct NotifyingSleepToolSet {
    started: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl ToolSetPort for NotifyingSleepToolSet {
    fn available_tools(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "Sleep".to_string(),
            description: "sleep".to_string(),
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
        self.started.notify_one();
        // Never returns — cancellation is tested at the ToolScheduler level,
        // not inside the tool implementation.
        tokio::time::sleep(Duration::from_secs(3600)).await;
        Ok(ToolResult {
            tool_use_id: String::new(),
            content: "done".to_string(),
            is_error: false,
        })
    }
}

struct ValidatingToolSet;

#[async_trait]
impl ToolSetPort for ValidatingToolSet {
    fn available_tools(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "Sleep".to_string(),
            description: "sleep".to_string(),
            input_schema: serde_json::json!({}),
            parallel_safe: true,
        }]
    }
    fn validate_input(
        &self,
        _tool_name: &str,
        _input: &serde_json::Value,
    ) -> Result<(), ToolError> {
        Err(ToolError::ExecutionFailed("invalid input".into()))
    }
    async fn execute(
        &self,
        _tool_name: &str,
        _input: serde_json::Value,
        _cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        unreachable!()
    }
}
