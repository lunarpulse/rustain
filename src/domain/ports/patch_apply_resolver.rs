//! Domain seam for the operator-triggered indeterminate-apply resolution verb
//! (Story 18.3a-f — FR160(d)).
//!
//! # A sibling seam, never a widening
//!
//! `ADR-11-3` rule 3: *"A new capability is a sibling seam, never a widening of
//! an existing port."* [`crate::domain::ports::PatchApplyExecutor`] applies a
//! patch; this port records what a human saw. ⛔ They are not two modes of one
//! verb: the executor's whole contract is that it mutates the workspace, and
//! this one's whole contract is that it **does not**. The in-tree precedent is
//! `infrastructure::orchestrator::artifact_review`, which says the same thing
//! from the other side — *"It records; it never applies."*
//!
//! # What crosses this seam is a report, not an observation
//!
//! The value is a [`OperatorApplyFinding`], deliberately not an
//! `ApplyOutcome`: the system never learns what `git apply` did, it learns what
//! a human reports, and those are two different facts that stay in two
//! different types all the way to the row. ⛔ Nothing behind this port probes
//! the working tree — AD-12 (`ARCHITECTURE-SPINE.md:159`) and
//! `ADR-18-3a-d-01` D2 forbid the probe by name. The operator probes; the
//! product records.

use async_trait::async_trait;

use crate::domain::models::{AgentId, ArtifactRef, OperatorApplyFinding};

/// Operator-facing classification of a resolution refusal or failure.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PatchResolvePortError {
    #[error("another process is applying a patch to this workspace")]
    WorkspaceBusy,
    /// 🔴 The wedged-only gate (ruling A5). The fold is last-write-wins, so
    /// without this refusal an operator's report would override a real machine
    /// outcome — fail-open through the side door.
    #[error("this patch is not awaiting an operator report; its recorded apply state is {0}")]
    NotWedged(String),
    /// 🔴 The verb never records an unreadable finding (`Unknown` is
    /// deserialize-only forward-compat). See `MergeBackError::UnreadableFinding`.
    #[error("an operator report must be 'present' or 'absent'; 'unknown' cannot be recorded")]
    UnreadableFinding,
    #[error("the operator report could not be recorded: {0}")]
    RecordFailed(String),
}

/// Record one operator's report about an indeterminate apply.
///
/// The implementation takes the same two locks in the same order as the apply
/// path, re-reads the room projection **after** acquiring them, and refuses
/// unless the projected state is exactly `ApplyState::Indeterminate`
/// (`ADR-18-3a-d-01` D4: *"Acquire before the gates, and re-read the projection
/// after acquiring … ⛔ Never release and re-acquire around the checks."*).
#[async_trait]
pub trait PatchApplyResolver: Send + Sync {
    async fn record_operator_inspection(
        &self,
        artifact: ArtifactRef,
        finding: OperatorApplyFinding,
        inspector: AgentId,
    ) -> Result<(), PatchResolvePortError>;
}
