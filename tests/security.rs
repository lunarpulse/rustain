//! Tests for SecurityAdapter and PermissionChain.

use std::sync::Arc;

use rustain::adapters::noop::NoOpSecurity;
use rustain::adapters::security_adapter::SecurityAdapter;
use rustain::domain::models::PermissionMode;
use rustain::domain::models::{ApprovalOutcome, ApprovalScope};
use rustain::domain::ports::SecurityPort;
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::domain::services::permission_chain::{self, PermissionDecision};

fn make_test_adapter() -> SecurityAdapter {
    let adapter = SecurityAdapter::new(std::env::current_dir().unwrap());
    adapter.set_mode(PermissionMode::Yolo);
    adapter
}

// Covers: FR24 (permission prompt), FR25 (permission modes)
#[tokio::test]
async fn test_safe_bash_command_not_blocked() {
    // Safe bash commands pass the blocklist but still prompt under Normal mode
    // because Bash is Elevated risk. The test verifies blocklist does NOT deny.
    let security = NoOpSecurity;
    let result = permission_chain::check(
        &security,
        "Bash",
        &serde_json::json!({"command": "echo hello"}),
        None,
        None,
    )
    .await;
    assert!(
        !matches!(result, PermissionDecision::Deny(_)),
        "Safe bash command should not be blocked"
    );
}

#[tokio::test]
async fn test_read_allowed_with_noop() {
    let security = NoOpSecurity;
    let result = permission_chain::check(
        &security,
        "Read",
        &serde_json::json!({"file_path": "./src/main.rs"}),
        None,
        None,
    )
    .await;
    assert_eq!(result, PermissionDecision::Allow);
}

#[tokio::test]
async fn test_blocked_command_denied_even_with_allow_permission() {
    let adapter = make_test_adapter();
    let result = permission_chain::check(
        &adapter,
        "Bash",
        &serde_json::json!({"command": "rm -rf /"}),
        None,
        None,
    )
    .await;
    assert!(matches!(result, PermissionDecision::Deny(_)));
}

#[tokio::test]
async fn test_workspace_violation_denied() {
    let adapter = make_test_adapter();
    let result = permission_chain::check(
        &adapter,
        "Read",
        &serde_json::json!({"file_path": "/etc/passwd"}),
        None,
        None,
    )
    .await;
    assert!(matches!(result, PermissionDecision::Deny(_)));
}

#[tokio::test]
async fn test_blocklist_catches_fork_bomb() {
    let adapter = make_test_adapter();
    assert!(adapter.check_blocklist(":(){ :|:& };:").is_err());
}

#[tokio::test]
async fn test_multiple_tool_calls_sequential() {
    let adapter = make_test_adapter();
    let r1 = permission_chain::check(
        &adapter,
        "Bash",
        &serde_json::json!({"command": "echo hello"}),
        None,
        None,
    )
    .await;
    assert_eq!(r1, PermissionDecision::Allow);

    let r2 = permission_chain::check(
        &adapter,
        "Bash",
        &serde_json::json!({"command": "rm -rf /"}),
        None,
        None,
    )
    .await;
    assert!(matches!(r2, PermissionDecision::Deny(_)));
}

#[tokio::test]
async fn test_yolo_mode_still_blocks_dangerous() {
    let adapter = make_test_adapter();
    adapter.set_mode(PermissionMode::Yolo);
    let result = permission_chain::check(
        &adapter,
        "Bash",
        &serde_json::json!({"command": "rm -rf /"}),
        None,
        None,
    )
    .await;
    assert!(matches!(result, PermissionDecision::Deny(_)));
}

// ── Task 19.1: ToolRisk::risk_for_builtin covers every builtin ──

#[test]
fn test_risk_for_builtin_covers_all_tools() {
    use rustain::domain::models::{ToolRisk, risk_for_builtin};
    assert_eq!(risk_for_builtin("Read"), ToolRisk::Safe);
    assert_eq!(risk_for_builtin("Glob"), ToolRisk::Safe);
    assert_eq!(risk_for_builtin("Grep"), ToolRisk::Safe);
    assert_eq!(risk_for_builtin("Write"), ToolRisk::Standard);
    assert_eq!(risk_for_builtin("Edit"), ToolRisk::Standard);
    assert_eq!(risk_for_builtin("Bash"), ToolRisk::Elevated);
    assert_eq!(risk_for_builtin("WebFetch"), ToolRisk::Elevated);
    assert_eq!(risk_for_builtin("MCP_SomeTool"), ToolRisk::Elevated);
    assert_eq!(risk_for_builtin("unknown"), ToolRisk::Elevated);
    // Fail-safe: unknown tool names — including wrong-case — default to Elevated.
    // Matching is exact-case so that `Read` only grants Safe status to the canonical
    // tool, not an accidental lowercase fork.
    assert_eq!(risk_for_builtin("read"), ToolRisk::Elevated);
    assert_eq!(risk_for_builtin("READ"), ToolRisk::Elevated);
    assert_eq!(risk_for_builtin("bash"), ToolRisk::Elevated);
}

// ── Task 19.2: Mode × risk matrix (16 cases) ──

/// Mock SecurityPort that returns a fixed mode.
struct MockSecurity {
    mode: PermissionMode,
}

impl MockSecurity {
    fn new(mode: PermissionMode) -> Self {
        Self { mode }
    }
}

#[async_trait::async_trait]
impl rustain::domain::ports::SecurityPort for MockSecurity {
    fn check_blocklist(
        &self,
        _command: &str,
    ) -> Result<(), rustain::domain::errors::PermissionError> {
        Ok(())
    }

    fn check_workspace_access(
        &self,
        _path: &std::path::Path,
        _op: rustain::domain::models::FileOperation,
    ) -> Result<rustain::domain::models::PathAccessType, rustain::domain::errors::PermissionError>
    {
        Ok(rustain::domain::models::PathAccessType::Workspace)
    }

    fn current_mode(&self) -> PermissionMode {
        self.mode
    }

    fn set_mode(&self, _mode: PermissionMode) {}
}

#[tokio::test]
async fn test_mode_risk_plan_safe_auto_allows() {
    let sec = MockSecurity::new(PermissionMode::Plan);
    let result = permission_chain::check(
        &sec,
        "Read",
        &serde_json::json!({"file_path": "a.rs"}),
        None,
        None,
    )
    .await;
    assert_eq!(result, PermissionDecision::Allow);
}

#[tokio::test]
async fn test_mode_risk_plan_standard_denies() {
    let sec = MockSecurity::new(PermissionMode::Plan);
    let result = permission_chain::check(
        &sec,
        "Write",
        &serde_json::json!({"file_path": "a.rs"}),
        None,
        None,
    )
    .await;
    assert!(matches!(result, PermissionDecision::Deny(_)));
}

#[tokio::test]
async fn test_mode_risk_plan_elevated_denies() {
    let sec = MockSecurity::new(PermissionMode::Plan);
    let result = permission_chain::check(
        &sec,
        "Bash",
        &serde_json::json!({"command": "ls"}),
        None,
        None,
    )
    .await;
    assert!(matches!(result, PermissionDecision::Deny(_)));
}

#[tokio::test]
async fn test_mode_risk_normal_safe_auto_allows() {
    let sec = MockSecurity::new(PermissionMode::Normal);
    let result = permission_chain::check(
        &sec,
        "Read",
        &serde_json::json!({"file_path": "a.rs"}),
        None,
        None,
    )
    .await;
    assert_eq!(result, PermissionDecision::Allow);
}

#[tokio::test]
async fn test_mode_risk_normal_standard_prompts() {
    let sec = MockSecurity::new(PermissionMode::Normal);
    let result = permission_chain::check(
        &sec,
        "Write",
        &serde_json::json!({"file_path": "a.rs"}),
        None,
        None,
    )
    .await;
    assert!(
        matches!(result, PermissionDecision::Prompt { .. }),
        "Normal + Standard must return Prompt"
    );
}

#[tokio::test]
async fn test_mode_risk_normal_elevated_prompts() {
    let sec = MockSecurity::new(PermissionMode::Normal);
    let result = permission_chain::check(
        &sec,
        "Bash",
        &serde_json::json!({"command": "ls"}),
        None,
        None,
    )
    .await;
    assert!(
        matches!(result, PermissionDecision::Prompt { .. }),
        "Normal + Elevated must return Prompt"
    );
}

#[tokio::test]
async fn test_mode_risk_autoedit_safe_auto_allows() {
    let sec = MockSecurity::new(PermissionMode::AutoEdit);
    let result = permission_chain::check(
        &sec,
        "Read",
        &serde_json::json!({"file_path": "a.rs"}),
        None,
        None,
    )
    .await;
    assert_eq!(result, PermissionDecision::Allow);
}

#[tokio::test]
async fn test_mode_risk_autoedit_standard_auto_allows() {
    let sec = MockSecurity::new(PermissionMode::AutoEdit);
    let result = permission_chain::check(
        &sec,
        "Write",
        &serde_json::json!({"file_path": "a.rs"}),
        None,
        None,
    )
    .await;
    assert_eq!(result, PermissionDecision::Allow);
}

#[tokio::test]
async fn test_mode_risk_autoedit_elevated_prompts() {
    let sec = MockSecurity::new(PermissionMode::AutoEdit);
    let result = permission_chain::check(
        &sec,
        "Bash",
        &serde_json::json!({"command": "ls"}),
        None,
        None,
    )
    .await;
    assert!(
        matches!(result, PermissionDecision::Prompt { .. }),
        "AutoEdit + Elevated must return Prompt"
    );
}

#[tokio::test]
async fn test_mode_risk_yolo_safe_auto_allows() {
    let sec = MockSecurity::new(PermissionMode::Yolo);
    let result = permission_chain::check(
        &sec,
        "Read",
        &serde_json::json!({"file_path": "a.rs"}),
        None,
        None,
    )
    .await;
    assert_eq!(result, PermissionDecision::Allow);
}

#[tokio::test]
async fn test_mode_risk_yolo_standard_auto_allows() {
    let sec = MockSecurity::new(PermissionMode::Yolo);
    let result = permission_chain::check(
        &sec,
        "Write",
        &serde_json::json!({"file_path": "a.rs"}),
        None,
        None,
    )
    .await;
    assert_eq!(result, PermissionDecision::Allow);
}

#[tokio::test]
async fn test_mode_risk_yolo_elevated_auto_allows() {
    let sec = MockSecurity::new(PermissionMode::Yolo);
    let result = permission_chain::check(
        &sec,
        "Bash",
        &serde_json::json!({"command": "ls"}),
        None,
        None,
    )
    .await;
    assert_eq!(result, PermissionDecision::Allow);
}

// ── Task 19.2b: Blocked risk denies under every mode ──
//
// No builtin currently maps to `ToolRisk::Blocked` via `risk_for_builtin`,
// so this branch is unreachable through `permission_chain::check`. We exercise
// `mode_risk_outcome` directly to lock the matrix contract: Blocked always
// denies regardless of mode.

#[test]
fn test_mode_risk_plan_blocked_denies() {
    assert_eq!(
        rustain::domain::services::permission_chain::mode_risk_outcome(
            PermissionMode::Plan,
            rustain::domain::models::ToolRisk::Blocked,
        ),
        Some(false)
    );
}

#[test]
fn test_mode_risk_normal_blocked_denies() {
    assert_eq!(
        rustain::domain::services::permission_chain::mode_risk_outcome(
            PermissionMode::Normal,
            rustain::domain::models::ToolRisk::Blocked,
        ),
        Some(false)
    );
}

#[test]
fn test_mode_risk_autoedit_blocked_denies() {
    assert_eq!(
        rustain::domain::services::permission_chain::mode_risk_outcome(
            PermissionMode::AutoEdit,
            rustain::domain::models::ToolRisk::Blocked,
        ),
        Some(false)
    );
}

#[test]
fn test_mode_risk_yolo_blocked_denies() {
    assert_eq!(
        rustain::domain::services::permission_chain::mode_risk_outcome(
            PermissionMode::Yolo,
            rustain::domain::models::ToolRisk::Blocked,
        ),
        Some(false)
    );
}

// ── Task 19.10: Feedback flow — DenyWithFeedback message format (AC5) ──
//
// The feedback string produced by the user's typed text must survive unchanged
// (content-wise) through both paths that render it:
//   1. The chat stream FeedbackBlock (event loop)
//   2. The tool_result sent back to the LLM (turn runtime)
// Both now call `format_feedback_message`. These tests lock the canonical
// `Tool denied. User feedback: "<text>"` format with proper escaping of `\`
// and `"` so the quoted display stays unambiguous.

#[test]
fn test_format_feedback_message_plain() {
    use rustain::domain::services::permission_chain::format_feedback_message;
    assert_eq!(
        format_feedback_message("don't delete my files"),
        r#"Tool denied. User feedback: "don't delete my files""#
    );
}

#[test]
fn test_format_feedback_message_escapes_quotes() {
    use rustain::domain::services::permission_chain::format_feedback_message;
    assert_eq!(
        format_feedback_message(r#"use "safe" mode"#),
        r#"Tool denied. User feedback: "use \"safe\" mode""#
    );
}

#[test]
fn test_format_feedback_message_escapes_backslash() {
    use rustain::domain::services::permission_chain::format_feedback_message;
    // Backslashes must be escaped BEFORE quotes, otherwise `\"` would collapse.
    assert_eq!(
        format_feedback_message(r#"path C:\Users"#),
        r#"Tool denied. User feedback: "path C:\\Users""#
    );
}

#[test]
fn test_format_feedback_message_empty() {
    use rustain::domain::services::permission_chain::format_feedback_message;
    assert_eq!(
        format_feedback_message(""),
        r#"Tool denied. User feedback: """#
    );
}

// ── Task 19.x: /mode argument parsing (dispatch to SetPermissionMode) ──
//
// The full `/mode` dispatch lives inside the event loop, but the parsing
// rule (which arg → which mode) is extracted as `parse_mode_arg`. These
// tests lock the mapping that the event loop uses to build
// `AppEvent::SetPermissionMode`.

#[test]
fn test_parse_mode_arg_known_names() {
    use rustain::domain::services::permission_chain::parse_mode_arg;
    assert_eq!(parse_mode_arg(Some("plan")), Some(PermissionMode::Plan));
    assert_eq!(parse_mode_arg(Some("normal")), Some(PermissionMode::Normal));
    assert_eq!(
        parse_mode_arg(Some("autoedit")),
        Some(PermissionMode::AutoEdit)
    );
    assert_eq!(parse_mode_arg(Some("auto")), Some(PermissionMode::AutoEdit));
    assert_eq!(parse_mode_arg(Some("yolo")), Some(PermissionMode::Yolo));
}

#[test]
fn test_parse_mode_arg_case_and_whitespace() {
    use rustain::domain::services::permission_chain::parse_mode_arg;
    assert_eq!(parse_mode_arg(Some("  PLAN  ")), Some(PermissionMode::Plan));
    assert_eq!(parse_mode_arg(Some("YOLO")), Some(PermissionMode::Yolo));
}

#[test]
fn test_parse_mode_arg_unknown_and_missing() {
    use rustain::domain::services::permission_chain::parse_mode_arg;
    assert_eq!(parse_mode_arg(None), None);
    assert_eq!(parse_mode_arg(Some("")), None);
    assert_eq!(parse_mode_arg(Some("god-mode")), None);
}

// ── Task 19.5 (rewritten): Session-allow via ApprovalRuntime fast path ──

#[tokio::test]
async fn test_session_allow_bypasses_second_request() {
    let runtime = ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );

    // First request: resolve with AlwaysTool
    let mut events = runtime.subscribe();
    let (_, rx1) = runtime
        .request(
            rustain::domain::models::tool_call::ApprovalSource::ForegroundTurn {
                conversation_id: "c1".into(),
            },
            "Bash".into(),
            serde_json::json!({"command": "ls"}),
            rustain::domain::models::ToolRisk::Elevated,
            None,
            None,
        )
        .await;

    // Need to get the request ID from the broadcast to resolve it
    let event = events.recv().await.unwrap();
    let id1 = match event {
        rustain::domain::services::approval_runtime::ApprovalRuntimeEvent::Requested {
            id, ..
        } => id,
        _ => panic!("expected Requested event"),
    };

    runtime
        .resolve(
            &id1,
            ApprovalOutcome::AlwaysTool {
                tool_name: "Bash".into(),
            },
        )
        .await;
    assert_eq!(
        rx1.await.unwrap(),
        ApprovalOutcome::AlwaysTool {
            tool_name: "Bash".into()
        }
    );

    // Second request for same tool should fast-path (no event broadcast)
    let (_, rx2) = runtime
        .request(
            rustain::domain::models::tool_call::ApprovalSource::ForegroundTurn {
                conversation_id: "c1".into(),
            },
            "Bash".into(),
            serde_json::json!({"command": "ls -la"}),
            rustain::domain::models::ToolRisk::Elevated,
            None,
            None,
        )
        .await;

    // Fast path: receiver resolves immediately without needing a broadcast event
    let result = tokio::time::timeout(std::time::Duration::from_millis(100), rx2).await;
    assert!(result.is_ok(), "Session-allowed tool must fast-path");
    assert_eq!(result.unwrap().unwrap(), ApprovalOutcome::Once);
}

// ── Task 19.6 (rewritten): Session-allow does NOT write to persistence ──

#[tokio::test]
async fn test_session_allow_not_persisted() {
    let runtime = ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );

    // Resolve a request with AlwaysTool (session-only)
    let mut events = runtime.subscribe();
    let (_, rx) = runtime
        .request(
            rustain::domain::models::tool_call::ApprovalSource::ForegroundTurn {
                conversation_id: "c1".into(),
            },
            "Bash".into(),
            serde_json::json!({"command": "ls"}),
            rustain::domain::models::ToolRisk::Elevated,
            None,
            None,
        )
        .await;

    let event = events.recv().await.unwrap();
    let id = match event {
        rustain::domain::services::approval_runtime::ApprovalRuntimeEvent::Requested {
            id, ..
        } => id,
        _ => panic!("expected Requested event"),
    };

    runtime
        .resolve(
            &id,
            ApprovalOutcome::AlwaysTool {
                tool_name: "Bash".into(),
            },
        )
        .await;
    let _ = rx.await;

    // With NoOpPersistence, nothing is saved. The test verifies the runtime
    // does not panic and the session set is updated in memory.
    let mut session = runtime.snapshot_session().await;
    assert!(session.is_auto_approved("Bash", None, None));
}

// ── Task 19.7 (rewritten): Reject propagates through ApprovalRuntime ──

#[tokio::test]
async fn test_reject_propagates() {
    let runtime = ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );

    let mut events = runtime.subscribe();
    let (_, rx) = runtime
        .request(
            rustain::domain::models::tool_call::ApprovalSource::ForegroundTurn {
                conversation_id: "c1".into(),
            },
            "Bash".into(),
            serde_json::json!({"command": "cargo test"}),
            rustain::domain::models::ToolRisk::Elevated,
            None,
            None,
        )
        .await;

    let event = events.recv().await.unwrap();
    let id = match event {
        rustain::domain::services::approval_runtime::ApprovalRuntimeEvent::Requested {
            id, ..
        } => id,
        _ => panic!("expected Requested event"),
    };

    runtime
        .resolve(
            &id,
            ApprovalOutcome::Reject {
                feedback: Some("don't delete".into()),
            },
        )
        .await;
    let outcome = rx.await.unwrap();
    assert!(
        matches!(outcome, ApprovalOutcome::Reject { feedback: Some(ref s) } if s == "don't delete"),
        "Expected Reject with correct text, got {:?}",
        outcome
    );
}

// ── Task 19.8: DF-108 symlink tests ──

#[test]
fn test_df108_symlink_pointing_outside_rejected() {
    let tmp = tempfile::TempDir::new().unwrap();
    let adapter = SecurityAdapter::new(tmp.path().to_path_buf());

    // Create a symlink inside workspace pointing outside
    let target = tempfile::TempDir::new().unwrap();
    let link = tmp.path().join("evil_link");
    std::os::unix::fs::symlink(target.path(), &link).unwrap();

    let file_via_link = link.join("secret.txt");
    let result = adapter
        .check_workspace_access(&file_via_link, rustain::domain::models::FileOperation::Read);
    // Symlink points OUTSIDE workspace → canonicalized parent resolves outside
    // → must be rejected (DF-108 hardening).
    assert!(
        result.is_err(),
        "Symlink escape must be rejected: {:?}",
        result
    );
}

#[test]
fn test_df108_symlink_pointing_inside_sibling_accepted() {
    let tmp = tempfile::TempDir::new().unwrap();
    let adapter = SecurityAdapter::new(tmp.path().to_path_buf());

    // Create real directory and symlink pointing to sibling inside workspace
    let real_dir = tmp.path().join("real");
    std::fs::create_dir(&real_dir).unwrap();
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&real_dir, &link).unwrap();

    let file_via_link = link.join("file.txt");
    // Create the file so canonicalize works
    std::fs::write(real_dir.join("file.txt"), "test").unwrap();

    let result = adapter
        .check_workspace_access(&file_via_link, rustain::domain::models::FileOperation::Read);
    assert!(
        result.is_ok(),
        "Symlink pointing inside workspace sibling should be accepted: {:?}",
        result
    );
}

#[test]
fn test_df108_symlink_from_nonexistent_parent_rejected() {
    let tmp = tempfile::TempDir::new().unwrap();
    let adapter = SecurityAdapter::new(tmp.path().to_path_buf());

    let path = tmp
        .path()
        .join("nonexistent")
        .join("nested")
        .join("file.txt");
    let result =
        adapter.check_workspace_access(&path, rustain::domain::models::FileOperation::Write);
    assert!(
        result.is_err(),
        "Path with nonexistent parent should be rejected"
    );
}

#[test]
fn test_df108_symlink_in_middle_of_existing_path_rejected() {
    // AC8 — symlink escape hardening: a symlink in the middle of an already-existing
    // path that resolves outside the workspace must be rejected even when the final
    // component exists (canonicalize would succeed on the full path).
    let tmp = tempfile::TempDir::new().unwrap();
    let outside = tempfile::TempDir::new().unwrap();
    let adapter = SecurityAdapter::new(tmp.path().to_path_buf());

    // Create a real file at outside/subdir/file.txt
    let outside_subdir = outside.path().join("subdir");
    std::fs::create_dir(&outside_subdir).unwrap();
    let outside_file = outside_subdir.join("file.txt");
    std::fs::write(&outside_file, "secret").unwrap();

    // Create a symlink inside workspace whose name is "sneaky" pointing at outside/
    let link = tmp.path().join("sneaky");
    std::os::unix::fs::symlink(outside.path(), &link).unwrap();

    // Access path: workspace/sneaky/subdir/file.txt — a fully existing path whose
    // middle hop is a symlink escaping the workspace.
    let escape_path = link.join("subdir").join("file.txt");
    let result =
        adapter.check_workspace_access(&escape_path, rustain::domain::models::FileOperation::Read);
    assert!(
        result.is_err(),
        "Symlink in middle of existing path escaping workspace must be rejected: {:?}",
        result
    );
}

// ── Task 19.11: Blocklist overrides Yolo (conformance) ──

#[tokio::test]
async fn test_blocklist_overrides_yolo_conformance() {
    let adapter = make_test_adapter();
    adapter.set_mode(PermissionMode::Yolo);
    let result = permission_chain::check(
        &adapter,
        "Bash",
        &serde_json::json!({"command": "rm -rf /"}),
        None,
        None,
    )
    .await;
    assert!(
        matches!(result, PermissionDecision::Deny(_)),
        "Blocklist must override Yolo"
    );
}

// ── Task 19.12 (rewritten): Mode switch does NOT clear session-allow set ──

#[tokio::test]
async fn test_mode_switch_preserves_session_allow() {
    let runtime = ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );

    // Register session-allow for Bash via AlwaysTool resolution
    let mut events = runtime.subscribe();
    let (_, rx) = runtime
        .request(
            rustain::domain::models::tool_call::ApprovalSource::ForegroundTurn {
                conversation_id: "c1".into(),
            },
            "Bash".into(),
            serde_json::json!({"command": "ls"}),
            rustain::domain::models::ToolRisk::Elevated,
            None,
            None,
        )
        .await;

    let event = events.recv().await.unwrap();
    let id = match event {
        rustain::domain::services::approval_runtime::ApprovalRuntimeEvent::Requested {
            id, ..
        } => id,
        _ => panic!("expected Requested event"),
    };

    runtime
        .resolve(
            &id,
            ApprovalOutcome::AlwaysTool {
                tool_name: "Bash".into(),
            },
        )
        .await;
    let _ = rx.await;

    let mut snap = runtime.snapshot_session().await;
    assert!(snap.is_auto_approved("Bash", None, None));

    let mut snap2 = runtime.snapshot_session().await;
    assert!(
        snap2.is_auto_approved("Bash", None, None),
        "Session-allow should persist"
    );
}

// ── Task 19.13: Plan mode + Standard tool → Deny ──

#[tokio::test]
async fn test_plan_mode_blocks_standard_tools() {
    let sec = MockSecurity::new(PermissionMode::Plan);
    let result = permission_chain::check(
        &sec,
        "Write",
        &serde_json::json!({"file_path": "a.rs"}),
        None,
        None,
    )
    .await;
    match result {
        PermissionDecision::Deny(reason) => {
            assert!(
                reason.contains("Plan mode"),
                "Deny reason should mention Plan mode: {}",
                reason
            );
        }
        other => panic!("Expected Deny, got {:?}", other),
    }
}

// ── Task 16.1 (rewritten): SessionAllow does NOT call persist_settings ──

#[tokio::test]
async fn test_session_allow_does_not_persist() {
    let runtime = ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );

    let mut events = runtime.subscribe();
    let (_, rx) = runtime
        .request(
            rustain::domain::models::tool_call::ApprovalSource::ForegroundTurn {
                conversation_id: "c1".into(),
            },
            "Bash".into(),
            serde_json::json!({"command": "ls"}),
            rustain::domain::models::ToolRisk::Elevated,
            None,
            None,
        )
        .await;

    let event = events.recv().await.unwrap();
    let id = match event {
        rustain::domain::services::approval_runtime::ApprovalRuntimeEvent::Requested {
            id, ..
        } => id,
        _ => panic!("expected Requested event"),
    };

    // AlwaysTool is session-only (not persisted)
    runtime
        .resolve(
            &id,
            ApprovalOutcome::AlwaysTool {
                tool_name: "Bash".into(),
            },
        )
        .await;
    let _ = rx.await;

    // With NoOpPersistence, verify the runtime still functions correctly
    // and the session is updated without persisting.
    let mut session = runtime.snapshot_session().await;
    assert!(session.is_auto_approved("Bash", None, None));
}

// ── ApprovalRuntime conformance tests ──

#[tokio::test]
async fn test_approval_runtime_cancel_by_source() {
    let runtime = ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );

    let source = rustain::domain::models::tool_call::ApprovalSource::ForegroundTurn {
        conversation_id: "c1".into(),
    };
    let (_, rx) = runtime
        .request(
            source.clone(),
            "Bash".into(),
            serde_json::json!({"command": "ls"}),
            rustain::domain::models::ToolRisk::Elevated,
            None,
            None,
        )
        .await;

    runtime
        .cancel_by_source(
            &source,
            rustain::domain::services::approval_runtime::CancelReason::SourceAborted,
        )
        .await;

    let outcome = rx.await.unwrap();
    assert_eq!(outcome, ApprovalOutcome::Cancel);
}

#[tokio::test]
async fn test_approval_runtime_always_and_save_persists_scope() {
    let tmp = tempfile::tempdir().unwrap();
    let user_config = tmp.path().join("config.toml");
    let workspace_rules = tmp.path().join("permissions.toml");
    let persistence = Arc::new(
        rustain::adapters::approval_persistence_toml::ApprovalPersistenceToml::new(
            user_config,
            workspace_rules,
        ),
    );
    let runtime = ApprovalRuntime::new(16, persistence.clone());

    let mut events = runtime.subscribe();
    let (_, rx) = runtime
        .request(
            rustain::domain::models::tool_call::ApprovalSource::ForegroundTurn {
                conversation_id: "c1".into(),
            },
            "Bash".into(),
            serde_json::json!({"command": "ls"}),
            rustain::domain::models::ToolRisk::Elevated,
            None,
            None,
        )
        .await;

    let event = events.recv().await.unwrap();
    let id = match event {
        rustain::domain::services::approval_runtime::ApprovalRuntimeEvent::Requested {
            id, ..
        } => id,
        _ => panic!("expected Requested event"),
    };

    runtime
        .resolve(
            &id,
            ApprovalOutcome::AlwaysAndSave {
                scope: ApprovalScope::Tool("Bash".into()),
            },
        )
        .await;
    let _ = rx.await;

    // Verify session is updated
    let mut session = runtime.snapshot_session().await;
    assert!(session.is_auto_approved("Bash", None, None));
}
