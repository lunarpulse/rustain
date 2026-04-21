#![allow(dead_code)]
use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::domain::errors::PermissionError;
use crate::domain::models::{ApprovalDecision, FileOperation, PathAccessType, PermissionMode};

/// Security enforcement: blocklist, workspace boundaries, permission prompts.
///
/// Claudian equivalent: `src/core/permissions/permissionManager.ts`
#[async_trait]
pub trait SecurityPort: Send + Sync {
    /// Check if a command is blocked by the blocklist.
    fn check_blocklist(&self, command: &str) -> Result<(), PermissionError>;

    /// Check if a file path is within allowed workspace boundaries.
    fn check_workspace_access(
        &self,
        path: &Path,
        op: FileOperation,
    ) -> Result<PathAccessType, PermissionError>;

    /// Request permission for a tool call. Adapter reads its own mode internally.
    /// YOLO: auto-approve. Normal: send AppEvent + await oneshot. Plan: check plan status.
    async fn request_permission(
        &self,
        tool_name: &str,
        tool_input: &serde_json::Value,
    ) -> Result<ApprovalDecision, PermissionError>;

    /// Get the current permission mode.
    fn current_mode(&self) -> PermissionMode;

    /// Update the permission mode (interior mutability).
    fn set_mode(&self, mode: PermissionMode);

    /// Register a skill directory as a readable path for the current session (Story 5-2 AC7).
    /// Called on `activate`, paired with `remove_active_skill_dir` on deactivation.
    /// Default impl is a no-op for adapters that don't enforce workspace boundaries.
    fn add_active_skill_dir(&self, _dir: PathBuf) {}

    /// Remove a skill directory from the per-session readable-paths set (Story 5-2 AC7).
    fn remove_active_skill_dir(&self, _dir: &Path) {}

    // v0.5+: fn check_tool_restriction(&self, tool_name: &str, agent_tools: Option<&[String]>) -> Result<(), PermissionError> { Ok(()) }
    // v0.5+: fn add_allowed_rule(&self, rule: PermissionRule);
}
