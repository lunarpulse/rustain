//! Domain trait for a completed (or in-flight) fork-join wave handle.
//!
//! **Story 14.3a — Preflight Party-Mode #2 (F1+F2+F5 resolution).**
//!
//! The v0.4 plan hand-waved "retain the `ForkJoinRun` somewhere the event loop
//! reaches". There is no valid implementation of that under `AppState.orchestrator:
//! Arc<dyn Orchestrator>` — the trait `run_fork_join` returns only `ForkJoinOutcome`,
//! and the `ForkJoinRun` is built then discarded. This trait dissolves the problem:
//!
//! - `Orchestrator::run_wave(req, cancel) -> Arc<dyn WaveHandle>` replaces the
//!   raw `run_fork_join` as the domain entry for retained-handle fan-outs.
//! - `ForkJoinRun` stays `pub(crate)` infra + `impl WaveHandle` via zero-new-body
//!   delegation — the infra handle never leaks past the domain boundary.
//! - `TuiState.wave_run: Option<Arc<dyn WaveHandle>>` — a domain trait object,
//!   adapters→domain only; the adapters→infra crossing is gone.
//!
//! **ADR-14-3a-01** names Epic-17 `RemoteSubagentRunner` as the contractually-
//! committed second `impl WaveHandle` (overrides Rule-of-Three so a future
//! reviewer can't "simplify away" the single-impl trait).

use std::fmt::Debug;
use std::sync::Arc;

use crate::domain::models::AgentId;
use crate::domain::models::orchestration::{CoverageLine, ForkJoinOutcome, SpokeResult};

/// Sealed opaque newtype for drill body content (DD-B3/DD4).
///
/// **RELOCATED** from `domain/models/orchestration.rs` per Preflight #2 — still
/// domain, keeps DD4 sealed-opaque + the 3 compiler guards + `as_render_str` only.
///
/// NO `Deref`/`AsRef`/`Borrow`/`From`/`Into`/`Serialize` — exactly one accessor.
/// The handle leaks no `Arc<ResultStore>` / `&ResultStore` / `&NodeResult` / raw body.
pub struct DrillBody(pub(crate) String);

impl DrillBody {
    /// Render the drill body for display. Hidden to discourage casual use.
    #[doc(hidden)]
    pub fn as_render_str(&self) -> &str {
        &self.0
    }
}

/// A read-only snapshot of wave state. `Copy`/`Clone` counters the render path
/// reads without holding a reference to the run.
#[derive(Clone, Debug)]
pub struct WaveSnapshot {
    /// Total dispatched spoke count.
    pub spoke_count: usize,
    /// Spokes that have reached a terminal state.
    pub completed: usize,
    /// `Some(true)` when the synthesis surfaced an honest-empty floor.
    pub honest_empty: Option<bool>,
    /// `true` when cancel-all fired.
    pub cancelled: bool,
    /// The pure-domain outcome (spokes + synthesis).
    pub outcome: ForkJoinOutcome,
    /// Number of drill-body resolutions performed through this handle.
    pub resolve_count: usize,
    /// Dispatch-ordered slot AgentIds.
    pub slots: Vec<AgentId>,
    /// Per-slot re-run counts.
    pub rerun_counts: Vec<u8>,
}

/// Domain trait for a retained fork-join wave handle.
///
/// The render path, keybindings, and drill/diverge/rerun all consume this trait —
/// never the infra-side `ForkJoinRun` directly. `Send + Sync + 'static` so it
/// lives in `Arc<dyn WaveHandle>` on `TuiState`. `Debug` required by `AppEvent`'s
/// derive.
pub trait WaveHandle: Debug + Send + Sync + 'static {
    /// Read-only snapshot of the wave's current state.
    fn snapshot(&self) -> WaveSnapshot;

    /// Fire the wave-cancel token. Propagates to every child spoke + any
    /// in-flight rerun. Idempotent (calling twice is safe).
    fn cancel(&self);

    /// Lazy drill-on-open for slot `slot`. Returns `None` for out-of-range
    /// slots or stale drill-ids after a rerun replaced the slot.
    fn drill(&self, slot: usize) -> Option<DrillBody>;

    /// Cheap id lookup for slot `slot` — validates the slot belongs to this wave.
    /// Does NOT resolve the body (resolve_count unchanged).
    fn drill_id(&self, slot: usize) -> Option<crate::domain::models::orchestration::DrillId>;

    /// Read-only view of the dispatch-ordered slot AgentIds.
    fn slots(&self) -> Vec<AgentId>;

    /// Re-run count for a slot. 0 for out-of-range.
    fn rerun_count_for_slot(&self, slot: usize) -> u8;

    /// Number of drill-body resolutions performed through this handle.
    fn resolve_count(&self) -> usize;
}

/// Result of a single-spoke rerun (DD-B6).
///
/// **Moved here** from `node_orchestrator.rs` — now typed off the domain
/// `WaveHandle` trait, repaying the F2 domain→infra inversion
/// (`node_orchestrator.rs:67,76` used to name `ForkJoinRun`).
#[derive(Debug)]
pub enum RerunOutcome {
    /// The target spoke succeeded; here is the new wave handle (deep-clone CoW
    /// of the store, fresh AgentId at a stable slot — DD-B4/DD-B5).
    Replaced(Arc<dyn WaveHandle>),
    /// The target spoke failed, was cancelled, or hit the storm-cap;
    /// the prior is untouched. Carries the `slot` for lamp-clear (Sally AC11).
    Reverted { slot: usize },
}
