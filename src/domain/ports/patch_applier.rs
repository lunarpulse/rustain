//! Git-over-workspace adapter seam. The domain/use-case layer decides
//! *whether* to apply (the pure `may_apply_patch` gate); this port is the
//! boundary behind which every `git` interaction with the One-Ring workspace
//! happens — the mutation itself and the workspace facts the orchestrator must
//! not shell out for on its own. The concrete git shell-out lives in
//! `adapters/`, composed at the startup root — never inlined in orchestration.
//!
//! ⚑ **The description above is a 2026-08-07 correction (Story 18.3a-d,
//! ruling P10).** From 17.3b until then this doc claimed the port "owns *how*
//! the One-Ring workspace is mutated", which was never true of its only
//! implementation: `GitPatchApplier` is a `git` shell-out, so the port has
//! always been the git-over-workspace seam. [`PatchApplier::revision`] is in
//! concern under the corrected description; it is not a widening of a
//! mutation-only port, and `ADR-11-3` rule 3 does not fire.

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

    /// Best-effort identifier of the workspace's current revision, read
    /// immediately before a mutation so the durable apply record carries a
    /// preimage witness (Story 18.3a-d, `DF-17-3b-1`).
    ///
    /// 🔴 **`None` is a first-class honest value and must never fail an
    /// apply.** `git rev-parse HEAD` legitimately fails on a repository with
    /// no commits, and `git apply` works outside a repository entirely, so a
    /// non-git or pre-first-commit workspace is a supported case rather than
    /// an error. Callers record the value and carry on; an audit field must
    /// never become load bearing for control flow.
    async fn revision(&self, workspace: &std::path::Path) -> Option<String>;
}
