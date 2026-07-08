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
//!
//! **Story 14.3a (Preflight Party-Mode #2):** `run_wave` added as the retained-
//! handle entry — returns `Arc<dyn WaveHandle>` so the TUI can drill/diverge/
//! rerun/cancel through a domain trait, never the infra `ForkJoinRun`. The old
//! `run_fork_join` is kept as a convenience (delegates `run_wave(..).snapshot().
//! outcome`). `RerunOutcome` moved to `wave_handle.rs` typed off the trait.

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::domain::models::orchestration::{ForkJoinOutcome, OrchestrationError};
use crate::domain::ports::wave_handle::{RerunOutcome, WaveHandle};

/// The per-wave input the coordinator hands the orchestrator. Carried as a
/// concrete struct (not on the trait method signature) so R2 can extend it
/// additively without churning every implementor.
///
/// `coordinator` is the AgentId of the parent agent that owns the wave; the
/// orchestrator mints delegated child tokens from the coordinator's authority.
#[derive(Clone, Debug)]
pub struct ForkJoinRequest {
    /// The owning coordinator agent (mint point for child tokens).
    pub coordinator: crate::domain::models::AgentId,
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
    /// the grounded synthesis floor. **Convenience method** — delegates to
    /// `run_wave` and projects `.snapshot().outcome`. Kept for callers that
    /// don't need the retained handle.
    async fn run_fork_join(
        &self,
        request: ForkJoinRequest,
    ) -> Result<ForkJoinOutcome, OrchestrationError>;

    /// Fan out one level of children and return the retained wave handle.
    /// The caller passes a `CancellationToken` that becomes the wave's cancel
    /// root — `WaveHandle::cancel()` fires it (AC8). The token tree propagates
    /// to every child via `dispatch_launch`'s `wave_cancel.child_token()`.
    async fn run_wave(
        &self,
        request: ForkJoinRequest,
        cancel: CancellationToken,
    ) -> Result<Arc<dyn WaveHandle>, OrchestrationError>;

    /// Re-fork exactly one spoke through the sealed chokepoint (DD6/DD-B5/DD-B6).
    /// Added here (not just on the concrete executor) so a RemoteSubagentRunner
    /// can rerun — no LSP trap.
    ///
    /// The executor retains the current wave internally; this method operates on
    /// that retained state (R1: single-wave, DN-1 in-flight guard). The cancel
    /// token should be a `child_token()` of the wave root (F3 resolution).
    async fn rerun_spoke(
        &self,
        slot: usize,
        cancel: CancellationToken,
    ) -> Result<RerunOutcome, OrchestrationError>;
}
