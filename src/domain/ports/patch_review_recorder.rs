//! `PatchReviewRecorder` — the narrow domain seam by which the TUI records an
//! operator patch-review verdict WITHOUT holding a concrete `PatchMergeBack`
//! (which lives in `infrastructure/orchestrator/`).
//!
//! Story 18.3a-c (AC5). `architecture.md:2087-2120` forbids
//! `adapters/* → infrastructure/*`, and `ADR-18-3-01` D3 is explicit: *when two
//! adapters must meet, they meet at a domain port*. The `/artifact` handler
//! parses; `infrastructure/runtime/artifact_bridge` performs the I/O through
//! this port; the concrete implementation is composed only at the composition
//! root (`startup.rs`), which owns the slot (`ADR-18-3-01` D4).
//!
//! The port carries exactly the three things the artifact surfaces call — no
//! dead methods:
//!
//! 1. [`PatchReviewRecorder::record_verdict`] — the verdict verb.
//! 2. [`PatchReviewRecorder::body`] — the `/artifact show <id>` drill-down.
//! 3. [`PatchReviewRecorder::effective_policy`] — the **same**
//!    [`MergeBackPolicy`] value the apply path was composed with, so the row's
//!    `(policy)` clause cannot drift from the gate it describes
//!    (`DF-18-3a-MERGEBACK-POLICY-VISIBILITY`).
//!
//! ⛔ **This port does not apply anything.** Recording a verdict is an
//! append to the room journal and nothing else; no `git apply`, no
//! working-tree mutation. The operator-triggered apply front door is
//! `18-3a-d` (`DF-18-3a-c-APPLY-FRONT-DOOR`).

use async_trait::async_trait;

use crate::domain::models::{AgentId, ArtifactId, ArtifactRef, ReviewVerdict};
use crate::domain::services::patch_review::MergeBackPolicy;

/// Failure surface for the verdict verb and the drill-down. Kept in `domain/`
/// so the trait carries no infrastructure error type.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PatchReviewError {
    /// The durable append, the store read, or the identity re-check refused.
    /// Carries an operator-facing message.
    #[error("{0}")]
    Refused(String),
}

/// Durable recording of an operator patch-review verdict across the domain
/// boundary.
#[async_trait]
pub trait PatchReviewRecorder: Send + Sync {
    /// Append `RoomEvent::PatchReviewed` for `artifact`, durable-first.
    ///
    /// ⛔ **`artifact` MUST already be resolved from the room projection.**
    /// The underlying service validates against the artifact *store* only, and
    /// the projection's fold is a **silent no-op** for an artifact it has never
    /// seen — so a raw-id caller can journal a verdict that then vanishes
    /// (ruling P3). Addressing is the projection's job, exactly as reading the
    /// review state is.
    async fn record_verdict(
        &self,
        artifact: ArtifactRef,
        reviewer: AgentId,
        verdict: ReviewVerdict,
    ) -> Result<ArtifactRef, PatchReviewError>;

    /// The stored body of an artifact, for the `/artifact show <id>`
    /// drill-down. Integrity is re-verified by the store on read.
    async fn body(&self, id: &ArtifactId) -> Result<Vec<u8>, PatchReviewError>;

    /// The merge-back policy **the apply path uses**.
    ///
    /// Composed from the single value handed to the fork-join executor at the
    /// root, never re-read from config: OPEN-DR-4's resolution is that the row
    /// suffix explains the policy, and an explainer sourced from a second read
    /// is the drift it exists to prevent.
    fn effective_policy(&self) -> MergeBackPolicy;
}
