//! Write-capable patch-application port. The domain/use-case layer decides
//! *whether* to apply (the pure `may_apply_patch` gate); this port owns *how*
//! the One-Ring workspace is mutated. The concrete git shell-out lives in
//! `adapters/`, composed at the startup root — never inlined in orchestration.

use async_trait::async_trait;

/// Outcome of attempting to apply a patch body to a working tree.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum PatchApplyError {
    /// The patch is corrupt / unparseable — a hard error, never a review
    /// conflict. Equivalent to F12's "malformed" classification.
    #[error("patch body is malformed and could not be parsed")]
    Malformed,
    /// The patch is well-formed but does not apply cleanly against the current
    /// tree (merge conflict). Route to review, never silently overwrite.
    #[error("patch conflicts with the current working tree: {0}")]
    Conflict(String),
    /// The apply mechanism could not start or complete (git missing, I/O).
    #[error("patch apply mechanism failed: {0}")]
    Io(String),
}

/// Apply a serialized unified-diff patch body to `workspace` via `git apply`.
///
/// On success the working tree is mutated. On error the working tree is left
/// unchanged (git apply is atomic across hunks unless `--reject` is requested,
/// which this port never does).
#[async_trait]
pub trait PatchApplier: Send + Sync {
    async fn apply(&self, workspace: &std::path::Path, body: &[u8]) -> Result<(), PatchApplyError>;
}
