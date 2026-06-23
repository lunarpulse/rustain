//! Fork-join orchestrator port — the public coarse entry the coordinator's turn
//! loop calls (Story 14.3, FR129 R1, DD1).
//!
//! Per the party-mode resolution (2026-06-21, authoritative):
//! - **`run_fork_join`** is the public entry: fans out one level via the sealed
//!   `dispatch` chokepoint, collects AgentId-keyed results, and returns the
//!   grounded synthesis floor. It calls `dispatch` N times and MUST NOT spawn
//!   directly.
//! - **`dispatch`** is the single, internal, sealed spawn chokepoint. It is
//!   deliberately NOT on this trait: exposing the single-child primitive would
//!   leak it into the public surface and create an R2 LSP trap (a `run_graph`
//!   sibling is additive; a trait method is not). `dispatch` lives as a
//!   `pub(crate)` method on the concrete executor. Concrete-first, no premature
//!   trait (Rule-of-Three; 12.2a precedent).
//!
//! The port exists so the composition root binds a concrete executor behind
//! `Arc<dyn Orchestrator>` (ADR-06-09 shared-port binding) and so R2 can swap
//! the implementation. Synthesis stays INSIDE the orchestrator (DD1): the
//! coordinator schedules no synthesizer node (AC1).

use async_trait::async_trait;

use crate::domain::models::AgentId;
use crate::domain::models::orchestration::{ForkJoinOutcome, OrchestrationError};

/// The per-wave input the coordinator hands the orchestrator. Carried as a
/// concrete struct (not on the trait method signature) so R2 can extend it
/// additively without churning every implementor.
///
/// `coordinator` is the AgentId of the parent agent that owns the wave; the
/// orchestrator mints delegated child tokens from the coordinator's authority.
#[derive(Clone, Debug)]
pub struct ForkJoinRequest {
    /// The owning coordinator agent (mint point for child tokens).
    pub coordinator: AgentId,
    /// One [`SpokeSpec`] per child. The executor asserts each is single-level
    /// (`waits_for` empty) and bounds the fan-out at the static spawn cap.
    pub spokes: Vec<crate::domain::models::orchestration::SpokeSpec>,
    /// Wave-completion policy (`All` in R1; `Quorum` reserved & inert — AC9).
    pub wait_policy: crate::domain::models::orchestration::WaitPolicy,
    /// Concurrency bound (semaphore permits). Capped to `spokes.len()` and the
    /// static spawn cap; externalized as config (AC4).
    pub concurrency: usize,
}

/// Fork-join orchestration port. The executor impl drives children through the
/// `SubagentRunner` port (AC8), never a concrete runner, so R2's
/// `RemoteSubagentRunner` is a transparent composition-root swap (ADR-10-1).
#[async_trait]
pub trait Orchestrator: Send + Sync {
    /// Fan out one level of children, collect AgentId-keyed results, and return
    /// the grounded synthesis floor. The coordinator's turn loop consumes the
    /// [`ForkJoinOutcome`] (it may enrich via its own `run_turn`, but the
    /// orchestrator schedules NO synthesizer node — AC1).
    async fn run_fork_join(
        &self,
        request: ForkJoinRequest,
    ) -> Result<ForkJoinOutcome, OrchestrationError>;

    /// Re-fork exactly one spoke through the sealed chokepoint (DD6/DD-B5/DD-B6).
    /// Added here (not just on the concrete executor) so a RemoteSubagentRunner
    /// can rerun — no LSP trap. Borrows the prior [`ForkJoinRun`] (compiler-
    /// enforced non-destructiveness per DD-B6): a cancel/fail leaves the prior
    /// untouched; only a terminal-SUCCESS produces a new [`ForkJoinRun`].
    async fn rerun_spoke(
        &self,
        prev: &crate::infrastructure::orchestrator::ForkJoinRun,
        slot: usize,
    ) -> Result<RerunOutcome, OrchestrationError>;
}

/// Result of a single-spoke rerun (DD-B6).
pub enum RerunOutcome {
    /// The target spoke succeeded; here is the new ForkJoinRun (deep-clone CoW
    /// of the store, fresh AgentId at a stable slot — DD-B4/DD-B5).
    Replaced(crate::infrastructure::orchestrator::ForkJoinRun),
    /// The target spoke failed or was cancelled; the prior is untouched.
    Reverted,
}
