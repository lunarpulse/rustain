//! Story 9.2/9.4/9.5 — end-to-end MCP routing through the ToolScheduler FSM.
//!
//! Existing conformance tests cover the gating helpers and the adapter projections
//! in isolation. These tests prove the **runtime pipeline** — a `ToolCallRequest`
//! named `mcp__<server>__<tool>` flows through:
//!
//!     ToolScheduler::schedule
//!       → permission_chain (PermissionMode + ToolRisk decision)
//!       → CompositeToolsetAdapter::execute
//!       → McpClientAdapter::call_tool (real stdio subprocess)
//!       → ToolCall::Success | Cancelled
//!
//! Risks closed:
//!   * R2  — tool_use routes through the scheduler to the MCP client
//!   * R3  — Plan-mode rejects elevated MCP at the FSM level (Cancelled, not gated by unit fn)
//!   * R12 — Workspace-access check is NOT invoked for MCP tools at runtime
//!   * R13 — Cancellation mid-execution lands the FSM in Cancelled with no orphan
//!
//! All tests require the `mcp` feature flag (the only feature where MCP code paths exist).

#![cfg(feature = "mcp")]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use rustain::adapters::composite_toolset_adapter::CompositeToolsetAdapter;
use rustain::adapters::mcp::client::McpClientAdapter;
use rustain::adapters::noop::{NoOpApprovalPersistence, NoOpToolSet};
use rustain::domain::errors::PermissionError;
use rustain::domain::events::AppEvent;
use rustain::domain::models::tool_call::{ApprovalSource, ToolCall, ToolCallRequest};
use rustain::domain::models::{
    FileOperation, McpConnectionState, McpServerSource, McpServerSpec, McpTransport,
    PathAccessType, PermissionMode,
};
use rustain::domain::ports::{SecurityPort, ToolSetPort};
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::domain::services::tool_scheduler::ToolScheduler;
use tokio_util::sync::CancellationToken;

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Locate the `fake-mcp-server` binary built by Cargo for these tests.
///
/// Matches the discovery logic in `conformance_mcp_lifecycle.rs` so both
/// suites consume the same fixture binary without duplicating env knobs.
fn fake_mcp_binary() -> PathBuf {
    let binary_name = if cfg!(target_os = "windows") {
        "fake-mcp-server.exe"
    } else {
        "fake-mcp-server"
    };
    let exe_dir = std::env::current_exe()
        .expect("current exe")
        .parent()
        .expect("parent")
        .to_path_buf();
    for candidate in [
        exe_dir.join(binary_name),
        exe_dir.parent().expect("deps parent").join(binary_name),
    ] {
        if candidate.exists() {
            return candidate;
        }
    }
    panic!(
        "fake-mcp-server binary not found near {} — run `cargo build --features mcp --bin fake-mcp-server`",
        exe_dir.display()
    );
}

fn fake_spec(id: &str, env: BTreeMap<String, String>) -> McpServerSpec {
    McpServerSpec {
        id: id.to_string(),
        transport: McpTransport::Stdio,
        command: Some(fake_mcp_binary().to_string_lossy().into_owned()),
        args: vec![],
        env,
        url: None,
        persistent: false,
        source: McpServerSource::Workspace,
    }
}

async fn wait_connected(client: &McpClientAdapter, timeout_ms: u64) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if matches!(
            client.state(),
            McpConnectionState::Connected { .. } | McpConnectionState::Degraded { .. }
        ) {
            return true;
        }
        if std::time::Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Build a `CompositeToolsetAdapter` over a single connected `fake-mcp-server`.
async fn build_composite(
    server_id: &str,
    env: BTreeMap<String, String>,
) -> Arc<CompositeToolsetAdapter> {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    let spec = fake_spec(server_id, env);
    let client = Arc::new(McpClientAdapter::new(spec.clone(), Some(tx.clone())));
    client.connect().await.expect("mcp connect");
    assert!(
        wait_connected(&client, 5000).await,
        "fake-mcp-server failed to connect in 5s"
    );
    let builtin: Arc<dyn ToolSetPort> = Arc::new(NoOpToolSet);
    Arc::new(CompositeToolsetAdapter::new(
        builtin,
        vec![client],
        vec![spec],
        true, // include_builtin (irrelevant for our mcp__-prefixed calls)
        None, // event_tx — not needed for runtime tests
        None, // skill_activator
    ))
}

/// `SecurityPort` spy: tracks every `check_workspace_access` invocation.
/// Used to prove R12 — MCP tools must NOT trigger workspace-path checks.
struct SpySecurity {
    mode: PermissionMode,
    workspace_checks: Arc<AtomicU32>,
    blocklist_checks: Arc<AtomicU32>,
}

impl SpySecurity {
    fn new(mode: PermissionMode) -> (Self, Arc<AtomicU32>, Arc<AtomicU32>) {
        let workspace = Arc::new(AtomicU32::new(0));
        let blocklist = Arc::new(AtomicU32::new(0));
        (
            Self {
                mode,
                workspace_checks: workspace.clone(),
                blocklist_checks: blocklist.clone(),
            },
            workspace,
            blocklist,
        )
    }
}

#[async_trait]
impl SecurityPort for SpySecurity {
    fn check_blocklist(&self, _command: &str) -> Result<(), PermissionError> {
        self.blocklist_checks.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn check_workspace_access(
        &self,
        _path: &std::path::Path,
        _op: FileOperation,
    ) -> Result<PathAccessType, PermissionError> {
        self.workspace_checks.fetch_add(1, Ordering::SeqCst);
        Ok(PathAccessType::Workspace)
    }
    fn current_mode(&self) -> PermissionMode {
        self.mode
    }
    fn set_mode(&self, _mode: PermissionMode) {}
}

fn make_scheduler(
    composite: Arc<CompositeToolsetAdapter>,
    mode: PermissionMode,
) -> (Arc<ToolScheduler>, Arc<AtomicU32>, Arc<AtomicU32>) {
    let (spy, workspace_checks, blocklist_checks) = SpySecurity::new(mode);
    let security: Arc<dyn SecurityPort> = Arc::new(spy);
    let tools: Arc<dyn ToolSetPort> = composite;
    let approval = ApprovalRuntime::new(16, Arc::new(NoOpApprovalPersistence));
    let sched = ToolScheduler::new(security, tools, approval, 16);
    (sched, workspace_checks, blocklist_checks)
}

async fn schedule_one(
    sched: Arc<ToolScheduler>,
    tool_name: &str,
    input: serde_json::Value,
    cancel: CancellationToken,
) -> ToolCall {
    let req = ToolCallRequest {
        id: "tc-mcp-1".into(),
        tool_name: tool_name.into(),
        input,
    };
    let mut results = sched
        .schedule(
            ApprovalSource::ForegroundTurn {
                conversation_id: "c-mcp".into(),
            },
            vec![req],
            cancel,
            None,
        )
        .await;
    results.pop().expect("one result")
}

// ── R2: tool_use routes through scheduler → MCP client → Success ────────────

/// Normal mode + read-only MCP tool (`add`) → the scheduler must end in Success
/// with the fake server's computed output.
#[tokio::test]
async fn r2_scheduler_routes_mcp_tool_use_to_client_and_returns_result() {
    let composite = build_composite("svr-r2", BTreeMap::new()).await;
    let (sched, ws_checks, _) = make_scheduler(composite, PermissionMode::Normal);

    let call = schedule_one(
        sched,
        "mcp__svr-r2__add",
        serde_json::json!({"a": 2, "b": 3}),
        CancellationToken::new(),
    )
    .await;

    match call {
        ToolCall::Success { result, .. } => {
            assert!(
                result.output.contains('5'),
                "expected fake-mcp-server to compute 2+3=5, got: {}",
                result.output
            );
            assert!(!result.is_error, "tool result should not be an error");
        }
        other => panic!("expected Success terminal state, got {:?}", other),
    }
    // R12-adjacent invariant: even the success path must not check workspace.
    assert_eq!(
        ws_checks.load(Ordering::SeqCst),
        0,
        "MCP success path must not invoke check_workspace_access"
    );
}

// ── R3: Plan-mode rejects elevated MCP at the FSM (not just gate fn) ────────

/// Plan mode + write-style MCP tool (`echo` has `readOnlyHint=false` per
/// fake-mcp-server) → the scheduler must transition to Cancelled (or never
/// reach Executing). This is the runtime proof — the unit-level gate fn is
/// already covered by `conformance_mcp_tool_invocation.rs`.
#[tokio::test]
async fn r3_plan_mode_denies_elevated_mcp_at_runtime() {
    let composite = build_composite("svr-r3", BTreeMap::new()).await;
    let (sched, _, _) = make_scheduler(composite, PermissionMode::Plan);

    let call = schedule_one(
        sched,
        "mcp__svr-r3__echo",
        serde_json::json!({"text": "should-be-denied"}),
        CancellationToken::new(),
    )
    .await;

    // The FSM may surface denial as Cancelled (typical plan-mode terminal)
    // OR as Error("denied") — both prove the gate fired at runtime.
    match &call {
        ToolCall::Cancelled { reason, .. } => {
            assert!(
                reason.to_lowercase().contains("plan")
                    || reason.to_lowercase().contains("deni")
                    || reason.to_lowercase().contains("permission"),
                "plan-mode denial reason should indicate plan/permission, got: {reason}"
            );
        }
        ToolCall::Error { error, .. } => {
            assert!(
                error.to_lowercase().contains("plan")
                    || error.to_lowercase().contains("deni")
                    || error.to_lowercase().contains("permission"),
                "plan-mode denial error should indicate plan/permission, got: {error}"
            );
        }
        ToolCall::Success { result, .. } => panic!(
            "plan-mode MUST NOT allow elevated MCP tool to succeed (output: {:?})",
            result.output
        ),
        other => panic!("expected Cancelled or Error in plan mode, got {:?}", other),
    }
}

// ── R12: Workspace-access check is NOT invoked for MCP tools ────────────────

/// Normal mode + Yolo would auto-allow; Normal requires no path check either
/// for the `mcp__` namespace per ADR-06-08. The spy proves the runtime path
/// never asks SecurityPort about a workspace path.
#[tokio::test]
async fn r12_workspace_check_not_invoked_for_mcp_tools_at_runtime() {
    let composite = build_composite("svr-r12", BTreeMap::new()).await;
    let (sched, ws_checks, _) = make_scheduler(composite, PermissionMode::Yolo);

    let call = schedule_one(
        sched,
        "mcp__svr-r12__echo",
        serde_json::json!({"text": "hi"}),
        CancellationToken::new(),
    )
    .await;

    // Yolo + read-only-ish tool should land Success.
    assert!(
        matches!(call, ToolCall::Success { .. }),
        "expected Success under Yolo for mcp__ tool, got {:?}",
        call
    );
    assert_eq!(
        ws_checks.load(Ordering::SeqCst),
        0,
        "MCP runtime path must not invoke check_workspace_access (got {} calls)",
        ws_checks.load(Ordering::SeqCst)
    );
}

// ── R13: Cancellation mid-execution lands FSM in Cancelled ──────────────────

/// Configure `FAKE_MCP_HANG_CALL_TOOL=1` so `tools/call` never responds, then
/// schedule + cancel from the outside. The FSM must terminate Cancelled —
/// proving the per-call cancellation token reaches the in-flight subprocess
/// transaction without orphaning the JSON-RPC request.
#[tokio::test]
async fn r13_cancellation_mid_call_terminates_fsm_in_cancelled() {
    let mut env = BTreeMap::new();
    env.insert("FAKE_MCP_HANG_CALL_TOOL".into(), "1".into());
    let composite = build_composite("svr-r13", env).await;
    let (sched, _, _) = make_scheduler(composite, PermissionMode::Yolo);

    let cancel = CancellationToken::new();
    let cancel_for_task = cancel.clone();

    // Fire the cancel from a separate task so the schedule() future
    // is mid-execution when it lands.
    let canceler = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel_for_task.cancel();
    });

    // Bound the whole schedule call — if we don't get any terminal state in
    // a reasonable window, the cancellation path is broken (not just slow).
    let call = tokio::time::timeout(
        Duration::from_secs(5),
        schedule_one(
            sched,
            "mcp__svr-r13__echo",
            serde_json::json!({"text": "hang"}),
            cancel,
        ),
    )
    .await
    .expect("scheduler did not honor cancellation within 5s — orphan risk");

    canceler.await.expect("canceler task panicked");

    assert!(
        matches!(call, ToolCall::Cancelled { .. } | ToolCall::Error { .. }),
        "expected Cancelled (or Error from cancellation propagation), got {:?}",
        call
    );
}
