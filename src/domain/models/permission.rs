#![allow(dead_code)]
use serde::{Deserialize, Serialize};

/// Permission mode governing tool execution approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[repr(u8)]
pub enum PermissionMode {
    /// Every tool call requires explicit approval (except Safe tools).
    Normal = 0,
    /// All tool calls auto-approved (dangerous) — blocklist still enforced.
    Yolo = 1,
    /// Plan mode: read-only tools auto-allowed; Standard/Elevated blocked without prompt.
    Plan = 2,
    /// Read + Write/Edit auto-allowed; Elevated tools still prompt.
    AutoEdit = 3,
}

/// Risk category for tool classification (AC1, AC7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolRisk {
    /// Cannot modify state — Read, Glob, Grep.
    Safe,
    /// Modify files in workspace — Write, Edit.
    Standard,
    /// Arbitrary execution / network egress — Bash, WebFetch.
    Elevated,
    /// Dangerous command patterns matched by blocklist (always overrides mode).
    Blocked,
}

/// Map a built-in tool name to its risk category (AC1).
/// Unknown tools (including MCP/skill tools sharing names with builtins in different case)
/// default to `Elevated` (fail-safe) — matching is exact on the canonical capitalized builtin IDs.
pub fn risk_for_builtin(tool_name: &str) -> ToolRisk {
    match tool_name {
        "Read" | "Glob" | "Grep" | "activate_skill" | "exit_plan_mode" | "propose_plan" => ToolRisk::Safe,
        "Write" | "Edit" => ToolRisk::Standard,
        "Bash" | "WebFetch" => ToolRisk::Elevated,
        _ => ToolRisk::Elevated,
    }
}

/// Type of filesystem operation being requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOperation {
    Read,
    Write,
}

/// Classification of a file path relative to workspace boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathAccessType {
    Workspace,
    External,
    Export,
}

/// Outcome of the PlanApprovalCard widget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanApprovalOutcome {
    ApproveNormal,
    ApproveAutoEdit,
    Reject,
    Revise,
}
