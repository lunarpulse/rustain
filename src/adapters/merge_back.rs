//! Concrete `PatchApplier` backed by a `git apply` shell-out.
//!
//! The domain/use-case layer decides *whether* to apply via the pure
//! `may_apply_patch` gate; this adapter owns *how* the One-Ring workspace is
//! mutated. `git` is shell-out only (no `git2`/`gix`).

use std::path::Path;
use std::process::Stdio;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;

use crate::domain::ports::{PatchApplier, PatchApplyError};

/// Apply patches via `git apply --whitespace=nowarn -` reading the body from stdin.
#[derive(Debug, Default, Clone, Copy)]
pub struct GitPatchApplier;

#[async_trait]
impl PatchApplier for GitPatchApplier {
    async fn apply(&self, workspace: &Path, body: &[u8]) -> Result<(), PatchApplyError> {
        let mut child = tokio::process::Command::new("git")
            .args(["apply", "--whitespace=nowarn", "-"])
            .current_dir(workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| PatchApplyError::Io(source.to_string()))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| PatchApplyError::Io("git apply stdin unavailable".into()))?;
        stdin
            .write_all(body)
            .await
            .map_err(|source| PatchApplyError::Io(source.to_string()))?;
        // Drop stdin so git sees EOF and processes the patch.
        drop(stdin);
        let output = child
            .wait_with_output()
            .await
            .map_err(|source| PatchApplyError::Io(source.to_string()))?;
        if output.status.success() {
            return Ok(());
        }
        let diagnostic = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let lower = diagnostic.to_ascii_lowercase();
        // F12 — conflict ≠ corruption: a corrupt/unparseable patch is a hard
        // error; a well-formed patch that does not apply cleanly is a conflict.
        if lower.contains("corrupt patch")
            || lower.contains("unrecognized input")
            || lower.contains("patch fragment without header")
            || lower.contains("invalid path")
        {
            Err(PatchApplyError::Malformed)
        } else {
            Err(PatchApplyError::Conflict(diagnostic))
        }
    }
}
