//! Approval domain types — `ApprovalScope`, `ApprovalOutcome`.
//!
//! See ADR-06-01 for the canonical pub/sub design.

use serde::{Deserialize, Serialize};

/// Scope of an approval decision for persistence and fast-path matching.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ApprovalScope {
    /// tool_name (e.g., "Bash")
    Tool(String),
    /// mcp_server_id (e.g., "github-mcp")
    Server(String),
    /// glob pattern (e.g., "{workspace}/src/**")
    PathPrefix(String),
}

/// User's decision on a permission request.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ApprovalOutcome {
    /// Allow this call only.
    Once,
    /// Allow this tool for the rest of the session.
    AlwaysTool { tool_name: String },
    /// Allow all tools from this MCP server for the rest of the session.
    AlwaysServer { server_id: String },
    /// Allow this scope and persist it across restarts.
    AlwaysAndSave { scope: ApprovalScope },
    /// Deny this call, with optional feedback text for the LLM.
    Reject { feedback: Option<String> },
    /// Cancelled (e.g., parent turn aborted).
    Cancel,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_scope_roundtrip() {
        for scope in [
            ApprovalScope::Tool("Bash".into()),
            ApprovalScope::Server("github-mcp".into()),
            ApprovalScope::PathPrefix("src/**".into()),
        ] {
            let json = serde_json::to_string(&scope).unwrap();
            let back: ApprovalScope = serde_json::from_str(&json).unwrap();
            assert_eq!(scope, back);
        }
    }

    #[test]
    fn approval_outcome_roundtrip() {
        for outcome in [
            ApprovalOutcome::Once,
            ApprovalOutcome::AlwaysTool { tool_name: "Read".into() },
            ApprovalOutcome::AlwaysServer { server_id: "github-mcp".into() },
            ApprovalOutcome::AlwaysAndSave { scope: ApprovalScope::Tool("Bash".into()) },
            ApprovalOutcome::Reject { feedback: Some("don't delete".into()) },
            ApprovalOutcome::Reject { feedback: None },
            ApprovalOutcome::Cancel,
        ] {
            let json = serde_json::to_string(&outcome).unwrap();
            let back: ApprovalOutcome = serde_json::from_str(&json).unwrap();
            assert_eq!(outcome, back);
        }
    }
}
