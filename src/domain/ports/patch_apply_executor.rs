//! Domain seam for the operator-triggered patch-apply front door.
//!
//! The TUI and merge-back service are adapters on opposite sides of the domain boundary. The
//! composition root binds this port once; callers resolve an `ArtifactRef` from the room
//! projection before invoking it.

use async_trait::async_trait;

use crate::domain::models::{AgentId, ArtifactRef, OwnershipKind, PermissionMode};
use crate::domain::services::patch_review::MergeBackPolicy;

/// Operator-facing classification of a merge-back refusal or failure.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PatchApplyPortError {
    #[error("another process is applying a patch to this workspace")]
    WorkspaceBusy,
    #[error(
        "a prior apply has no recorded outcome; inspect the working tree and record what you \
         found with `/artifact resolve <id> present|absent`"
    )]
    ApplyIndeterminate,
    #[error(
        "the apply mutated the workspace but its outcome could not be recorded; it is now \
         indeterminate — inspect the working tree and record what you found with \
         `/artifact resolve <id> present|absent`: {0}"
    )]
    ApplyUnresolved(String),
    #[error("git apply rejected the patch without mutating the workspace: {0}")]
    Conflict(String),
    #[error("patch apply failed: {0}")]
    Failed(String),
}

/// Apply one projection-resolved patch through the durable merge-back chokepoint.
#[async_trait]
pub trait PatchApplyExecutor: Send + Sync {
    async fn apply_patch(
        &self,
        artifact: ArtifactRef,
        ownership: OwnershipKind,
        permission_mode: PermissionMode,
        policy: MergeBackPolicy,
        applier: Option<AgentId>,
    ) -> Result<(), PatchApplyPortError>;
}
