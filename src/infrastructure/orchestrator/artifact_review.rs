//! Composition-root adapter binding the artifact surfaces to the durable
//! merge-back service (Story 18.3a-c, AC5).
//!
//! [`PatchMergeBack::review`] shipped in 17.3b with **zero production
//! callers**: it validates the stored artifact, appends
//! `RoomEvent::PatchReviewed` durable-first, and returns the reviewed handle —
//! complete, tested, and unreachable. This type is the composition-root half
//! of its front door. The TUI half is
//! `infrastructure::runtime::artifact_bridge`, which reaches it only through
//! [`PatchReviewRecorder`].
//!
//! ⛔ **It records; it never applies.** `PatchMergeBack::apply` is deliberately
//! not exposed here (ruling A1): the operator-triggered apply front door, the
//! durable applied-state event (`DF-17-3b-1`, P1) and the cross-process apply
//! lock (`DF-17-3b-2`) all belong to `18-3a-d`.

use std::sync::Arc;

use async_trait::async_trait;

use crate::domain::models::{AgentId, ArtifactId, ArtifactRef, ReviewVerdict};
use crate::domain::ports::{ArtifactStore, PatchReviewError, PatchReviewRecorder};
use crate::domain::services::patch_review::MergeBackPolicy;
use crate::infrastructure::orchestrator::PatchMergeBack;

/// The journal-backed [`PatchReviewRecorder`].
///
/// Holds the **same** `Arc`s the fork-join executor was composed with — cloned
/// at the root before the executor takes ownership — so the surface and the
/// apply path can never be looking at two different stores, two different
/// journals, or two different policies.
pub struct JournalPatchReview {
    merge_back: Arc<PatchMergeBack>,
    store: Arc<dyn ArtifactStore>,
    /// The value handed to `ForkJoinExecutor::with_merge_back_policy`, carried
    /// rather than re-read. `DF-18-3a-MERGEBACK-POLICY-VISIBILITY` requires the
    /// operator-facing annotation to be *"sourced from the same resolver the
    /// apply path uses"*; a second read of `startup.rs` is precisely the drift
    /// that requirement forbids.
    policy: MergeBackPolicy,
}

impl JournalPatchReview {
    #[must_use]
    pub fn new(
        merge_back: Arc<PatchMergeBack>,
        store: Arc<dyn ArtifactStore>,
        policy: MergeBackPolicy,
    ) -> Self {
        Self {
            merge_back,
            store,
            policy,
        }
    }
}

#[async_trait]
impl PatchReviewRecorder for JournalPatchReview {
    async fn record_verdict(
        &self,
        artifact: ArtifactRef,
        reviewer: AgentId,
        verdict: ReviewVerdict,
    ) -> Result<ArtifactRef, PatchReviewError> {
        self.merge_back
            .review(artifact, reviewer, verdict)
            .await
            .map_err(|error| PatchReviewError::Refused(error.to_string()))
    }

    async fn body(&self, id: &ArtifactId) -> Result<Vec<u8>, PatchReviewError> {
        self.store
            .get(id)
            .await
            .map_err(|error| PatchReviewError::Refused(error.to_string()))
    }

    fn effective_policy(&self) -> MergeBackPolicy {
        self.policy
    }
}
