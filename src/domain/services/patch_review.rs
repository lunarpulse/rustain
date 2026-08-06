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

/// The reason-carrying outcome of the merge-back gate.
///
/// [`may_apply_patch`] is a **total projection of this value** and has no
/// branches of its own, so the operator-facing suffix and the apply path
/// cannot diverge (`DF-18-3a-MERGEBACK-POLICY-VISIBILITY`: the annotation must
/// be *"sourced from the same resolver the apply path uses"*). Two derivations
/// of one gate is `ADR-17-CC-01`'s rejected alternative rebuilt inside a
/// module.
///
/// ⚑ [`ApplyDecision`] is deliberately **not** widened (Story 18.3a-c ruling
/// A2). It is `Copy`, `#[non_exhaustive]`, not `Serialize`, compared by `!=` in
/// `PatchMergeBack::apply` and asserted six times in
/// `tests/conformance_cow_mergeback.rs`; widening breaks all seven. The
/// transitional pair is registered as `DF-18-3a-c-APPLYDECISION-RETIRE`.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatchDisposition {
    /// Approved, and **not** (self-originated *and* self-reviewed).
    ///
    /// 🔴 The guard is a **disjunction**: `!self_originated || reviewer !=
    /// producer`. A user-originated patch approved by its own producer applies,
    /// because `!self_originated` already satisfies the disjunct. An
    /// implementation that tests only `reviewer != producer` silently refuses
    /// those, on the live `/fanout` path.
    Applies,
    /// Not yet reviewed, but the owner pre-authorized user-originated
    /// merge-back and this patch is not self-originated. Production ships
    /// `auto_approve_user_originated: true`, so this is the **common** case: a
    /// surface rendering "pending ⇒ blocked" is lying under the shipped policy.
    AutoApplies,
    /// Plan mode is read-only and merge-back writes the One-Ring directly.
    RefusedPlanMode,
    /// The artifact is not a patch. Split from [`Self::RefusedPeerOwned`]
    /// (ruling A4): one `if` carried two causes, and the UX table's peer-owned
    /// row would render a **false reason** for a non-patch artifact.
    /// Unreachable through the patch-row front door — proven by a
    /// direct-domain unit test, per gate-review NON-BLOCKER 2.
    RefusedNotAPatch,
    RefusedPeerOwned,
    /// Fail-closed: a real producing node always stamps provenance.
    RefusedNoProvenance,
    /// Approved, but self-originated content approved by its own producer.
    RefusedSelfReview,
    RefusedChangesRequested,
    RefusedRejected,
    /// The ordinary awaiting-review state, and the fail-closed landing site for
    /// a review state or verdict this build cannot read. Carries **no** decision
    /// suffix on screen: the row's `<state>` field already says `pending`.
    AwaitingReview,
}

impl PatchDisposition {
    /// The apply gate's answer. [`may_apply_patch`] returns exactly this.
    ///
    /// Ruling P4: a **method**, not a free function beside the type. The
    /// subordination is then structural rather than a doc comment — an
    /// `ApplyDecision` cannot be computed without going through a disposition.
    #[must_use]
    pub fn decision(self) -> ApplyDecision {
        match self {
            Self::Applies | Self::AutoApplies => ApplyDecision::Apply,
            _ => ApplyDecision::Refuse,
        }
    }
}

/// Resolve **why** a patch artifact may or may not mutate the One-Ring
/// workspace.
///
/// The decision is pure. `PermissionMode::Plan` is refused unconditionally:
/// plan mode is read-only, and merge-back writes the One-Ring directly via
/// `git apply`, bypassing the tool scheduler. A configured policy may
/// pre-authorize only user-originated patches. Self-originated or peer-owned
/// patches always require a distinct explicit reviewer. Empty provenance is
/// refused (fail-closed) — a real producing node always tags its delta.
///
/// ⛔ The caller must pass an artifact resolved from the **journal
/// projection**, never a caller-supplied handle: `ArtifactRef.review` is public
/// and forgeable, which is why `PatchMergeBack::apply` re-derives it from
/// `project_room` before consulting this gate. Every surface inherits the same
/// rule.
#[must_use]
pub fn patch_disposition(
    artifact: &ArtifactRef,
    ownership: OwnershipKind,
    permission_mode: PermissionMode,
    policy: &MergeBackPolicy,
) -> PatchDisposition {
    if permission_mode == PermissionMode::Plan {
        return PatchDisposition::RefusedPlanMode;
    }
    // Ruling A4 — evaluated in this order so a non-patch never renders the
    // peer-owned reason. Both still project to `Refuse`, so behaviour is
    // unchanged.
    if artifact.kind != ArtifactKind::Patch {
        return PatchDisposition::RefusedNotAPatch;
    }
    if matches!(ownership, OwnershipKind::Peer) {
        return PatchDisposition::RefusedPeerOwned;
    }
    // Fail-closed: a real producing node always stamps provenance. An
    // untagged patch cannot be auto-approved as user-originated.
    if artifact.provenance.is_empty() {
        return PatchDisposition::RefusedNoProvenance;
    }

    let self_originated = artifact
        .provenance
        .iter()
        .any(|tag| matches!(tag, ProvenanceTag::SelfOriginated));
    match &artifact.review {
        Some(ReviewStatus::Reviewed { reviewer, verdict }) => match verdict {
            ReviewVerdict::Approved => {
                if !self_originated || reviewer != &artifact.producer {
                    PatchDisposition::Applies
                } else {
                    PatchDisposition::RefusedSelfReview
                }
            }
            ReviewVerdict::ChangesRequested => PatchDisposition::RefusedChangesRequested,
            ReviewVerdict::Rejected => PatchDisposition::RefusedRejected,
            // A verdict this build cannot read is not an approval. Fail closed
            // to the same landing site as an unreadable status.
            _ => PatchDisposition::AwaitingReview,
        },
        // An unreadable review status is never an approval (Story 18.3a-c AC1,
        // fourth mutant). It lands on the ordinary awaiting-review disposition
        // rather than minting an eleventh variant the suffix table has no row
        // for.
        Some(ReviewStatus::Unknown) => PatchDisposition::AwaitingReview,
        Some(ReviewStatus::Pending) | None
            if policy.auto_approve_user_originated && !self_originated =>
        {
            PatchDisposition::AutoApplies
        }
        Some(ReviewStatus::Pending) | None => PatchDisposition::AwaitingReview,
        // ⛔ No `_` arm. `ReviewStatus` is `#[non_exhaustive]`, which does not
        // apply in-crate, so this match is genuinely exhaustive and a future
        // variant forces a decision here at compile time rather than silently
        // inheriting a fallback.
    }
}

/// Decide whether a patch artifact may mutate the One-Ring workspace.
///
/// ⛔ **This function contains no condition of its own.** Its entire body is a
/// total projection of [`patch_disposition`], so the operator-facing reason and
/// the apply gate are structurally incapable of disagreeing. Adding a branch
/// here rebuilds the two-derivations defect ruling A2 exists to prevent.
pub fn may_apply_patch(
    artifact: &ArtifactRef,
    ownership: OwnershipKind,
    permission_mode: PermissionMode,
    policy: &MergeBackPolicy,
) -> ApplyDecision {
    patch_disposition(artifact, ownership, permission_mode, policy).decision()
}
