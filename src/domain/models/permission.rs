#![allow(dead_code)]
use serde::{Deserialize, Serialize};

/// Permission mode governing tool execution approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    /// Every tool call requires explicit approval.
    Normal,
    /// All tool calls auto-approved (dangerous).
    Yolo,
    // v0.5: Plan — tools approved per plan step
}

/// User's decision on a permission request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    Allow,
    AlwaysAllow,
    Deny,
    Cancel,
}

/// A rule granting automatic permission for a specific tool pattern.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRule {
    pub tool_name: String,
    pub pattern: Option<String>,
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
