use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Ephemeral in-process scratch workspace handle.
///
/// The live temporary directory is intentionally not serializable and never
/// persisted. R2 merge-back consumes the durable [`UnifiedDiff`], not this
/// handle.
pub struct IsolationHandle {
    temp_dir: Option<tempfile::TempDir>,
    path: PathBuf,
    canonical_root: PathBuf,
    backend: ProvisioningTier,
    created_at_ms: u64,
    stopped: bool,
}

impl std::fmt::Debug for IsolationHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IsolationHandle")
            .field("path", &self.path)
            .field("canonical_root", &self.canonical_root)
            .field("backend", &self.backend)
            .field("created_at_ms", &self.created_at_ms)
            .field("stopped", &self.stopped)
            .finish_non_exhaustive()
    }
}

impl IsolationHandle {
    pub fn new(
        temp_dir: tempfile::TempDir,
        backend: ProvisioningTier,
        created_at_ms: u64,
    ) -> Result<Self, IsolationError> {
        let path = temp_dir.path().to_path_buf();
        let canonical_root =
            path.canonicalize()
                .map_err(|source| IsolationError::CanonicalizeRoot {
                    path: path.clone(),
                    source,
                })?;
        Ok(Self {
            temp_dir: Some(temp_dir),
            path,
            canonical_root,
            backend,
            created_at_ms,
            stopped: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }

    pub fn backend(&self) -> ProvisioningTier {
        self.backend
    }

    pub fn created_at_ms(&self) -> u64 {
        self.created_at_ms
    }

    pub fn mark_stopped(mut self) {
        self.stopped = true;
        let _ = self.temp_dir.take();
    }
}

impl Drop for IsolationHandle {
    fn drop(&mut self) {
        if !self.stopped && self.temp_dir.is_some() {
            tracing::warn!(path = %self.path.display(), "isolation handle dropped without explicit stop; tempdir Drop will clean scratch dir");
        }
    }
}

/// R1 provisioning tier seam. Only ScratchCopy is implemented in 14.5; R2 may
/// add reflink/overlayfs backends behind the same provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvisioningTier {
    ScratchCopy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnifiedDiff {
    pub backend: ProvisioningTier,
    pub diff: String,
}

impl UnifiedDiff {
    pub fn new(backend: ProvisioningTier, diff: String) -> Self {
        Self { backend, diff }
    }

    pub fn is_empty(&self) -> bool {
        self.diff.is_empty()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IsolationError {
    #[error("isolation backend unavailable: {backend}: {reason}")]
    BackendUnavailable {
        backend: &'static str,
        reason: String,
    },
    #[error("isolation failed closed: {reason}")]
    FailClosed { reason: String },
    #[error("failed to canonicalize isolation root {path}: {source}")]
    CanonicalizeRoot {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("scratch copy failed from {from} to {to}: {source}")]
    CopyFailed {
        from: PathBuf,
        to: PathBuf,
        source: std::io::Error,
    },
    #[error("git command `{cmd}` failed in {cwd}: {stderr}")]
    GitFailed {
        cmd: &'static str,
        cwd: PathBuf,
        stderr: String,
    },
    #[error("scratch teardown refused: {reason}")]
    TeardownRefused { reason: String },
}

impl From<IsolationError> for crate::domain::models::subagent_error::SubagentError {
    fn from(value: IsolationError) -> Self {
        crate::domain::models::subagent_error::SubagentError::Internal(value.to_string())
    }
}
