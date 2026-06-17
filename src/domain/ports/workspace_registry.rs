use std::path::{Path, PathBuf};

use async_trait::async_trait;
use thiserror::Error;

/// One registered workspace hint row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceEntry {
    /// Canonical absolute path to the workspace root.
    pub path: PathBuf,
    /// Last successful note timestamp (unix seconds).
    pub last_seen: i64,
}

#[derive(Debug, Error)]
pub enum WorkspaceRegistryError {
    #[error("I/O error: {0}")]
    Io(String),

    #[error("Parse error: {0}")]
    Parse(String),

    #[error("Lock error: {0}")]
    Locked(String),

    #[error("Serialization error: {0}")]
    Serialize(String),

    #[error("Unsupported schema version: {0}")]
    UnsupportedVersion(String),
}

/// Write-side workspace registry seam.
#[async_trait]
pub trait WorkspaceRegistrarPort: Send + Sync {
    /// Best-effort, idempotent upsert of a persisted workspace root.
    async fn note_workspace(&self, workspace: &Path) -> Result<(), WorkspaceRegistryError>;
}

/// Read-side workspace registry seam.
#[async_trait]
pub trait WorkspaceRegistryReaderPort: Send + Sync {
    /// Return live workspaces only (dead paths omitted), never mutating the registry.
    /// Missing/corrupt/newer-version files degrade to `Ok(vec![])`.
    async fn live_workspaces(&self) -> Result<Vec<WorkspaceEntry>, WorkspaceRegistryError>;
}
