//! Tests for SecurityAdapter and PermissionChain.

use rustain::adapters::noop::NoOpSecurity;
use rustain::adapters::security_adapter::SecurityAdapter;
use rustain::domain::models::PermissionMode;
use rustain::domain::ports::SecurityPort;
use rustain::domain::services::permission_chain::{self, PermissionDecision};
use tokio::sync::mpsc;

fn make_test_adapter() -> SecurityAdapter {
    let (tx, _rx) = mpsc::unbounded_channel();
    let adapter = SecurityAdapter::new(std::env::current_dir().unwrap(), tx);
    // Set to Yolo for backward-compat with existing tests that expect auto-allow
    adapter.set_mode(PermissionMode::Yolo);
    adapter
}

#[tokio::test]
async fn test_safe_bash_command_allowed() {
    let security = NoOpSecurity;
    let result = permission_chain::check(
        &security,
        "Bash",
        &serde_json::json!({"command": "echo hello"}),
    )
    .await;
    assert_eq!(result, PermissionDecision::Allow);
}

#[tokio::test]
async fn test_read_allowed_with_noop() {
    let security = NoOpSecurity;
    let result = permission_chain::check(
        &security,
        "Read",
        &serde_json::json!({"file_path": "./src/main.rs"}),
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
    )
    .await;
    assert!(matches!(result, PermissionDecision::Deny(_)));
}

#[tokio::test]
async fn test_blocklist_catches_fork_bomb() {
    use rustain::domain::ports::SecurityPort;
    let adapter = make_test_adapter();
    assert!(adapter.check_blocklist(":(){ :|:& };:").is_err());
}

#[tokio::test]
async fn test_multiple_tool_calls_sequential() {
    let adapter = make_test_adapter();
    // First tool call allowed
    let r1 = permission_chain::check(
        &adapter,
        "Bash",
        &serde_json::json!({"command": "echo hello"}),
    )
    .await;
    assert_eq!(r1, PermissionDecision::Allow);

    // Second tool call blocked
    let r2 = permission_chain::check(
        &adapter,
        "Bash",
        &serde_json::json!({"command": "rm -rf /"}),
    )
    .await;
    assert!(matches!(r2, PermissionDecision::Deny(_)));
}

// Conformance: YOLO mode + blocked command → still blocked
#[tokio::test]
async fn test_yolo_mode_still_blocks_dangerous() {
    use rustain::domain::ports::SecurityPort;
    let adapter = make_test_adapter();
    adapter.set_mode(PermissionMode::Yolo);
    let result = permission_chain::check(
        &adapter,
        "Bash",
        &serde_json::json!({"command": "rm -rf /"}),
    )
    .await;
    assert!(matches!(result, PermissionDecision::Deny(_)));
}

// Conformance: Normal mode + AlwaysAllow match → Allow (no prompt)
#[tokio::test]
async fn test_normal_mode_always_allow_match() {
    use rustain::domain::models::PermissionRule;
    let (tx, _rx) = mpsc::unbounded_channel();
    let adapter = SecurityAdapter::new(std::env::current_dir().unwrap(), tx);
    // Default is Normal mode
    {
        let mut rules = adapter.allowed_rules.write().await;
        rules.push(PermissionRule {
            tool_name: "Bash".to_string(),
            pattern: Some("cargo test".to_string()),
        });
    }

    let result = permission_chain::check(
        &adapter,
        "Bash",
        &serde_json::json!({"command": "cargo test"}),
    )
    .await;
    assert_eq!(result, PermissionDecision::AlwaysAllow);
}
