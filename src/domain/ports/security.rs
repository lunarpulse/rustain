#![allow(dead_code)]
use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::domain::errors::PermissionError;
use crate::domain::models::{FileOperation, PathAccessType, PermissionMode};

/// Security enforcement: blocklist, workspace boundaries, permission mode.
///
/// Permission request flow lives in `ApprovalRuntime` (ADR-06-01).
/// This trait only covers blocklist, workspace, and mode queries.
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

    /// Get the current permission mode.
    fn current_mode(&self) -> PermissionMode;

    /// Update the permission mode (interior mutability).
    fn set_mode(&self, mode: PermissionMode);

    /// Register a skill directory as a readable path for the current session (Story 5-2 AC7).
    fn add_active_skill_dir(&self, _dir: PathBuf) {}

    /// Remove a skill directory from the per-session readable-paths set (Story 5-2 AC7).
    fn remove_active_skill_dir(&self, _dir: &Path) {}
}
