//! PermissionChain — domain service that orchestrates permission checks.
//! Pure orchestration: calls port traits, no I/O itself.

use crate::domain::models::{ApprovalDecision, FileOperation};
use crate::domain::ports::SecurityPort;

/// Result of a permission chain check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    AlwaysAllow,
    Deny(String),
    Cancel,
}

/// Check permission for a tool call through the full chain.
///
/// Steps:
/// 1. Tool restriction check (agent allowed_tools — pass-through for now)
/// 2. Blocklist check (Bash tool only)
/// 3. Workspace restriction (file tools — Read/Write)
/// 4. request_permission dispatch
pub async fn check(
    security: &dyn SecurityPort,
    tool_name: &str,
    input: &serde_json::Value,
) -> PermissionDecision {
    // Step 1: Tool restriction (no subagents yet — pass-through)

    // Step 2: Blocklist check (Bash tool only)
    if tool_name == "Bash" || tool_name == "bash" {
        match input.get("command").and_then(|v| v.as_str()) {
            Some(command) => {
                if let Err(e) = security.check_blocklist(command) {
                    return PermissionDecision::Deny(e.to_string());
                }
            }
            None => {
                return PermissionDecision::Deny(
                    "Bash tool missing required 'command' field".to_string(),
                );
            }
        }
    }

    // Step 3: Workspace restriction (file tools)
    match tool_name {
        "Read" | "read" => match input.get("file_path").and_then(|v| v.as_str()) {
            Some(path) => {
                if let Err(e) =
                    security.check_workspace_access(std::path::Path::new(path), FileOperation::Read)
                {
                    return PermissionDecision::Deny(e.to_string());
                }
            }
            None => {
                return PermissionDecision::Deny(
                    "Read tool missing required 'file_path' field".to_string(),
                );
            }
        },
        "Write" | "write" => match input.get("file_path").and_then(|v| v.as_str()) {
            Some(path) => {
                if let Err(e) = security
                    .check_workspace_access(std::path::Path::new(path), FileOperation::Write)
                {
                    return PermissionDecision::Deny(e.to_string());
                }
            }
            None => {
                return PermissionDecision::Deny(
                    "Write tool missing required 'file_path' field".to_string(),
                );
            }
        },
        _ => {}
    }

    // Step 4: Request permission
    match security.request_permission(tool_name, input).await {
        Ok(ApprovalDecision::Allow) => PermissionDecision::Allow,
        Ok(ApprovalDecision::AlwaysAllow) => PermissionDecision::AlwaysAllow,
        Ok(ApprovalDecision::Deny) => {
            PermissionDecision::Deny("Permission denied by user".to_string())
        }
        Ok(ApprovalDecision::Cancel) => PermissionDecision::Cancel,
        Err(e) => PermissionDecision::Deny(e.to_string()),
    }
}

// Tests moved to tests/security.rs to satisfy domain purity conformance test.
