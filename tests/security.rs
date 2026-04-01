//! Tests for SecurityAdapter and PermissionChain.

use rustain::adapters::noop::NoOpSecurity;
use rustain::adapters::security_adapter::SecurityAdapter;
use rustain::domain::services::permission_chain::{self, PermissionDecision};

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
    let adapter = SecurityAdapter::new(std::env::current_dir().unwrap());
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
    let adapter = SecurityAdapter::new(std::env::current_dir().unwrap());
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
    let adapter = SecurityAdapter::new(std::env::current_dir().unwrap());
    assert!(adapter.check_blocklist(":(){ :|:& };:").is_err());
}

#[tokio::test]
async fn test_multiple_tool_calls_sequential() {
    let adapter = SecurityAdapter::new(std::env::current_dir().unwrap());
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
