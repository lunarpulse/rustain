//! Pure review gate for write-capable patch artifacts.
//!
//! The gate has no I/O. It refuses `PermissionMode::Plan` (read-only) so a
//! reviewed patch can never mutate the One-Ring while the session is in plan
//! mode — merge-back is a write that bypasses the tool scheduler, and plan
//! mode's read-only invariant wins. Other tool-execution permission modes do
//! not weaken artifact review.

use crate::domain::models::{
    ArtifactKind, ArtifactRef, OwnershipKind, PermissionMode, ProvenanceTag, ReviewStatus,
    ReviewVerdict,
};

/// Explicit pre-authorization configured by the workspace owner.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MergeBackPolicy {
    pub auto_approve_user_originated: bool,
}

#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyDecision {
    Apply,
    Refuse,
}

/// Decide whether a patch artifact may mutate the One-Ring workspace.
///
/// The decision is pure. `PermissionMode::Plan` is refused unconditionally:
/// plan mode is read-only, and merge-back writes the One-Ring directly via
/// `git apply`, bypassing the tool scheduler. A configured policy may
/// pre-authorize only user-originated patches. Self-originated or peer-owned
/// patches always require a distinct explicit reviewer. Empty provenance is
/// refused (fail-closed) — a real producing node always tags its delta.
pub fn may_apply_patch(
    artifact: &ArtifactRef,
    ownership: OwnershipKind,
    permission_mode: PermissionMode,
    policy: &MergeBackPolicy,
) -> ApplyDecision {
    if permission_mode == PermissionMode::Plan {
        return ApplyDecision::Refuse;
    }
    if artifact.kind != ArtifactKind::Patch || matches!(ownership, OwnershipKind::Peer) {
        return ApplyDecision::Refuse;
    }
    // Fail-closed: a real producing node always stamps provenance. An
    // untagged patch cannot be auto-approved as user-originated.
    if artifact.provenance.is_empty() {
        return ApplyDecision::Refuse;
    }

    let self_originated = artifact
        .provenance
        .iter()
        .any(|tag| matches!(tag, ProvenanceTag::SelfOriginated));
    match &artifact.review {
        Some(ReviewStatus::Reviewed { reviewer, verdict }) => {
            if *verdict == ReviewVerdict::Approved
                && (!self_originated || reviewer != &artifact.producer)
            {
                ApplyDecision::Apply
            } else {
                ApplyDecision::Refuse
            }
        }
        Some(ReviewStatus::Pending) | None
            if policy.auto_approve_user_originated && !self_originated =>
        {
            ApplyDecision::Apply
        }
        Some(ReviewStatus::Pending) | None => ApplyDecision::Refuse,
    }
}
