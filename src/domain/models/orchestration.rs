//! Orchestration domain model — single-level fork-join (Story 14.3, FR129 R1).
//!
//! R1 ships a strict subset of the amendment's `DagWaveOrchestrator`: ONE level
//! of children fanned out via `dispatch`, collected AgentId-keyed, and a
//! coordinator-side grounded synthesis. DAG/wave scheduling is demoted to R2
//! (`epics.md:6128-6138`); the types here are shaped so R2's multi-wave
//! readiness predicate is purely additive.
//!
//! ## Party-mode resolutions (2026-06-21 — authoritative)
//!
//! - **DD2:** `AgentId` is used directly — no `type NodeId = AgentId` alias (a
//!   transparent alias adds a second name with zero type-safety). If R2 needs a
//!   distinct node identity it is an additive `struct NodeId(AgentId)` newtype.
//! - **DD2:** `waits_for: Vec<AgentId>` is retained as **inert data** on the
//!   spec — default-empty, asserted-empty by the executor, NEVER read for
//!   scheduling in R1. It satisfies both "no dependency-edge logic in a
//!   non-graph executor" and the R2-additivity keystone (the zero-sibling-
//!   transition discriminator, AC2).
//! - **DD3:** executor time fields are `u64` ms read through the injected
//!   `Clock` port; exactly one `i64→u64` cast lives at the clock read.
//! - **DD4:** result bodies live in an AgentId-keyed `ResultStore` side-table,
//!   NOT on `NodeHandle`. The public [`SpokeResult`] is a Result-shaped
//!   projection (synthesis sees failed/cancelled children too).
//!
//! This module is import-pure: it references only `crate::domain` types so the
//! hexagonal boundary (`conformance.rs::test_domain_no_adapter_or_infra_imports`)
//! holds.

use serde::{Deserialize, Serialize};

use crate::domain::models::{AgentId, Budget, ModelTier, ToolPolicy};

/// The static spawn-cap ceiling — an engine **invariant**, not the adaptive UI
/// gate (that is 14.3a). Fan-out **above** this is refused before any child is
/// dispatched (`attempted > cap`); fan-out exactly at the cap is permitted.
pub const FORK_JOIN_SPAWN_CAP: usize = 8;

/// Maximum grandchildren accepted by one declarative coordinator.
pub const MAX_NESTED_BREADTH: usize = FORK_JOIN_SPAWN_CAP;

/// Wave-completion policy. `All` is the only active variant in R1; `Quorum(n)`
/// is reserved + inert (R3 consensus, Story 18.6 activates it).
///
/// Mirrors the const-data style of `NodeState`: the legal graph is data, not
/// control flow. `is_active_in_r1` is the single predicate consumers branch on,
/// so activating `Quorum` later is a one-line edit here.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitPolicy {
    /// Wait for every dispatched spoke to reach a terminal state (R1 default).
    All,
    /// Wait for `n` terminal spokes (R3 consensus — INERT in R1).
    Quorum(u32),
}

impl Default for WaitPolicy {
    fn default() -> Self {
        Self::All
    }
}

impl WaitPolicy {
    /// `All` is the sole R1-active variant. `Quorum(n)` is pinned-unused: the
    /// executor asserts it is never supplied in R1 (AC9), so any future
    /// activation is a deliberate, reviewed change rather than silent drift.
    pub const fn is_active_in_r1(self) -> bool {
        matches!(self, Self::All)
    }

    /// R1 rejects `Quorum` at the boundary (AC9: reserved & inert). Returning
    /// `OrchestrationError::WaitPolicyUnsupported` keeps the gate in one place.
    pub const fn r1_unsupported_reason(self) -> Option<&'static str> {
        match self {
            Self::All => None,
            Self::Quorum(_) => Some("WaitPolicy::Quorum is reserved for R3 (Story 18.6)"),
        }
    }
}

/// Declarative execution role for one spoke.
///
/// R1 supports one nested coordinator layer. Grandchildren must be leaves;
/// deeper role composition is refused before any launch.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpokeRole {
    /// Execute the spoke once and collect its terminal result.
    Leaf,
    /// Retain this spoke's live host-bound handle and drive one child wave.
    Coordinator {
        grandchildren: Box<[SpokeSpec]>,
        concurrency: usize,
        wait_policy: WaitPolicy,
    },
}

impl Default for SpokeRole {
    fn default() -> Self {
        Self::Leaf
    }
}

/// A single child spoke in a fork-join wave. The declarative input the
/// coordinator hands to `run_fork_join`; the executor converts each spoke into
/// an `AgentLaunchSpec` + a delegated child token before dispatching.
///
/// `id` and `waits_for` form the active dependency DAG consumed by the
/// multi-wave scheduler.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpokeSpec {
    /// Stable logical node id used by `waits_for` dependency edges.
    pub id: AgentId,
    /// Short human label rendered in the coverage line / WaveStrip.
    pub label: String,
    /// Instruction text for the spoke (becomes the launch prompt).
    pub prompt: String,
    /// Resolved model after tier routing.
    pub effective_model: String,
    /// Cost/quality tier for model selection.
    pub tier: ModelTier,
    /// Tool policy for this spoke (narrowing-only inheritance).
    pub tools_allow: ToolPolicy,
    /// Stable logical ids of prerequisite spokes.
    pub waits_for: Vec<AgentId>,
    /// Declarative execution behavior. Defaults to a single leaf.
    pub role: SpokeRole,
}

/// The declarative fork-join request handed to `run_fork_join`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForkJoinSpec {
    /// The coordinator (parent) agent that owns this wave + synthesizes.
    pub coordinator: AgentId,
    /// One spoke per child. Bounded by [`FORK_JOIN_SPAWN_CAP`].
    pub spokes: Vec<SpokeSpec>,
    /// Wave-completion policy — `All` in R1.
    pub wait_policy: WaitPolicy,
    /// Concurrency bound (semaphore permits). Capped to `spokes.len()` and to
    /// [`FORK_JOIN_SPAWN_CAP`]; externalized as config (AC4).
    pub concurrency: usize,
}

/// Per-spoke terminal outcome. **Result-shaped** (DD4): synthesis sees failed
/// and cancelled spokes too, so the honest coverage line can say "2 failed".
///
/// This is the PUBLIC projection of the crate-private `NodeResult` the executor
/// mints at a child's terminal state. It carries no body — full payloads stay
/// in the `ResultStore` side-table, addressed by `AgentId` (AC5 symbolic
/// composition).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpokeResult {
    /// Spoke completed; `summary` is compact metadata (a relative-ranked
    /// salience noun-phrase), NOT the full payload body.
    Completed { summary: String },
    /// Spoke reached a non-terminal-error failure.
    Failed { reason: String },
    /// Spoke was cancelled (cascade / cancel-all / user).
    Cancelled,
    /// Spoke completed with no usable signal (empty body).
    Empty,
}

impl SpokeResult {
    /// `true` for outcomes that contributed a usable signal (the denominator
    /// of the coverage line). Failed/Cancelled/Empty do NOT count.
    pub const fn is_signal(&self) -> bool {
        matches!(self, Self::Completed { .. })
    }

    /// `true` for terminal outcomes that are NOT a clean completion — i.e. the
    /// "honest-empty" cohort the synthesis must never paper over (AC7).
    pub const fn is_degraded(&self) -> bool {
        matches!(self, Self::Failed { .. } | Self::Cancelled | Self::Empty)
    }
}

/// Honest coverage line for a synthesis (AC7). `completed` is the count of
/// `SpokeResult::Completed`; `degraded` aggregates Failed/Cancelled/Empty.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CoverageLine {
    /// Spokes that returned a usable signal.
    pub completed: usize,
    /// Spokes that failed.
    pub failed: usize,
    /// Spokes that were cancelled.
    pub cancelled: usize,
    /// Spokes that completed empty.
    pub empty: usize,
    /// Total spokes dispatched.
    pub total: usize,
}

impl CoverageLine {
    /// Build from a slice of spoke outcomes. The postcondition
    /// `completed + failed + cancelled + empty == total` is invariant by
    /// construction (every spoke is exactly one variant).
    pub fn from_results(results: &[SpokeResult]) -> Self {
        let mut line = Self {
            total: results.len(),
            ..Self::default()
        };
        for r in results {
            match r {
                SpokeResult::Completed { .. } => line.completed += 1,
                SpokeResult::Failed { .. } => line.failed += 1,
                SpokeResult::Cancelled => line.cancelled += 1,
                SpokeResult::Empty => line.empty += 1,
            }
        }
        line
    }

    /// `true` when zero spokes contributed signal — the synthesis MUST surface
    /// an explicit honest-empty state rather than confident noise (AC7).
    pub const fn is_honest_empty(&self) -> bool {
        self.completed == 0
    }

    /// Human-readable coverage line, e.g. `over 12 of 15 — 2 failed, 1 empty`.
    pub fn render(&self) -> String {
        if self.is_honest_empty() {
            return format!("no signal from {} spokes", self.total);
        }
        let mut parts = Vec::new();
        if self.failed > 0 {
            parts.push(format!("{} failed", self.failed));
        }
        if self.cancelled > 0 {
            parts.push(format!("{} cancelled", self.cancelled));
        }
        if self.empty > 0 {
            parts.push(format!("{} empty", self.empty));
        }
        if parts.is_empty() {
            format!("over {} of {}", self.completed, self.total)
        } else {
            format!(
                "over {} of {} — {}",
                self.completed,
                self.total,
                parts.join(", ")
            )
        }
    }
}

/// A per-spoke citation grounding one synthesis claim in a concrete handle
/// (AC7). `agent_id` addresses the full payload in the `ResultStore`
/// side-table; the synthesis never inlines the body (AC5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpokeCitation {
    pub agent_id: AgentId,
    pub label: String,
    pub summary: String,
}

/// The deterministic, grounded synthesis floor (AC7 — the day-one floor; the
/// LLM-enriched synthesis is the coordinator's own turn, NOT scheduled here).
///
/// `build` requires a per-spoke citation and enforces
/// `coverage.len() == completed.len()` (no orphan claims). An all-degraded wave
/// produces an explicit `honest_empty` synthesis rather than confident noise.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct SynthesisView {
    /// One-line grounded summary. Empty (with `honest_empty = true`) when no
    /// spoke contributed signal.
    pub summary: String,
    /// Per-completed-spoke citations. Length == `coverage.completed`.
    pub citations: Vec<SpokeCitation>,
    /// Roll-up of every spoke's terminal outcome.
    pub coverage: CoverageLine,
    /// `true` when zero spokes contributed signal (AC7 honest-empty).
    pub honest_empty: bool,
}

impl SynthesisView {
    /// Build the grounded floor from per-spoke citations + the full outcome
    /// roll-up. POSTCONDITION: `citations.len() == coverage.completed` (every
    /// completed spoke is cited exactly once — no orphan claims, AC7).
    pub fn build(citations: Vec<SpokeCitation>, coverage: CoverageLine) -> Self {
        debug_assert_eq!(
            citations.len(),
            coverage.completed,
            "synthesis postcondition: one citation per completed spoke"
        );
        let honest_empty = coverage.is_honest_empty();
        let summary = if honest_empty {
            // Honest-empty: never confident noise when all spokes degraded. The
            // coverage line below carries the counts; this summary is the
            // single statement of the "no signal" intent (no duplication).
            "no spoke contributed usable signal.".to_string()
        } else {
            format!(
                "Synthesized {} — {}.",
                citations
                    .iter()
                    .map(|c| c.label.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                coverage.render()
            )
        };
        Self {
            summary,
            citations,
            coverage,
            honest_empty,
        }
    }
}

/// The outcome of a fork-join wave: AgentId-keyed per-spoke results + the
/// grounded synthesis floor. The coordinator's turn loop consumes this (it may
/// run an enrichment turn via `run_turn`, but the executor schedules NO
/// synthesizer node — AC1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForkJoinOutcome {
    /// AgentId-keyed terminal outcomes, in dispatch order. Never a flattened
    /// blob (AC2): R2's readiness predicate composes over this shape.
    pub spokes: Vec<(AgentId, SpokeResult)>,
    /// The grounded synthesis floor (AC7).
    pub synthesis: SynthesisView,
}

/// Opaque drill identifier (cheap — validates the id without resolving the body).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DrillId(pub(crate) AgentId);

/// **RELOCATED** to `domain/ports/wave_handle.rs` per Preflight Party-Mode #2
/// (F1+F2+F5). Re-exported here for backward compatibility.
pub use crate::domain::ports::wave_handle::DrillBody;

/// A typed reason a waiting/in-flight spoke may carry (AC10). Lives in executor
/// side-state — NOT a payload on `NodeState::Waiting`, which would break the
/// 14.1 FSM const-table + serde + field pins. Escalation to a hazard after a
/// `Clock`-driven threshold reads this side-state.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitReason {
    /// Spoke awaiting a terminal state under `WaitPolicy::All`.
    AwaitingSpoke,
    /// Wave auto-paused at the budget ceiling (reserve untouched, AC10).
    BudgetPaused,
    /// Downstream node parked until durable upstream artifact handles land.
    AwaitingUpstreamArtifact,
}

impl WaitReason {
    /// `true` for reasons that should escalate to a hazard after the threshold.
    /// `BudgetPaused` does NOT escalate (it is a deliberate, recoverable pause).
    pub const fn escalates(&self) -> bool {
        matches!(self, Self::AwaitingSpoke | Self::AwaitingUpstreamArtifact)
    }
}

/// Errors raised by the fork-join executor. `#[non_exhaustive]` so R2
/// (multi-wave, cycle detection, per-wave semaphores) can extend it without
/// breaking exhaustive matches.
#[non_exhaustive]
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum OrchestrationError {
    /// Dependency names a spoke outside this fork-join request.
    #[error("spoke {spoke} waits for unknown dependency {dependency}")]
    MissingDependency { spoke: AgentId, dependency: AgentId },
    /// Logical spoke ids must be unique inside one request.
    #[error("duplicate spoke id {0}")]
    DuplicateSpoke(AgentId),
    /// The waits-for graph is cyclic and therefore cannot dispatch.
    #[error("fork-join dependency cycle detected")]
    DependencyCycle,
    /// Fan-out above the static spawn cap (AC4 invariant). Exactly-at-cap is
    /// permitted (`attempted > cap`); the doc on [`FORK_JOIN_SPAWN_CAP`] is the
    /// authority.
    #[error("fan-out {attempted} exceeds spawn cap {cap}")]
    SpawnCapExceeded { cap: usize, attempted: usize },
    /// A declarative coordinator exceeded the nested fan-out bound.
    #[error("nested fan-out {attempted} exceeds nested breadth cap {cap}")]
    NestedBreadthExceeded { cap: usize, attempted: usize },
    /// R1 permits root → coordinator → leaf only.
    #[error("nested coordinator grandchildren must all be leaves")]
    NestedDepthUnsupported,
    /// `WaitPolicy::Quorum` supplied in R1 (AC9 — reserved & inert).
    #[error("{0}")]
    WaitPolicyUnsupported(&'static str),
    /// The child token lacks `Spawn` or is revoked/expired — the spawn gate
    /// refused (Murat vacuity-closer: validates the CHILD token, not the
    /// coordinator's). No node created, no ledger debit.
    #[error("spawn refused by authority gate: {0}")]
    SpawnRefused(String),
    /// The underlying runner rejected the launch.
    #[error("subagent runner error: {0}")]
    Runner(String),
    /// A spoke exceeded its waiting threshold and escalated to a hazard.
    #[error("spoke `{0}` stuck waiting: escalated to hazard after threshold")]
    StuckWaiting(String),
    /// AC10 budget-ceiling auto-pause. Typed (not generic [`Self::Internal`])
    /// so the caller / WaveStrip can surface `paused` — fan-out drew the
    /// synthesis reserve to its limit and is paused (never silent death; the
    /// reserve is untouched). R1 returns this from `run_fork_join`; the
    /// turn-loop integration (deferred) maps it to a recoverable paused state.
    #[error("budget ceiling reached: auto-paused (have {available:?}, need {needed:?})")]
    BudgetPaused { available: Budget, needed: Budget },
    /// A nested drive lost the in-memory owner handle required for delegation.
    #[error("host-bound coordinator handle unavailable for {0}")]
    HostBoundUnavailable(AgentId),
    /// Nested coordinator waves are intentionally one-shot in R1.
    #[error("nested coordinator spokes are not rerunnable")]
    NestedRerunUnsupported,
    /// Internal invariant violation.
    #[error("orchestration internal error: {0}")]
    Internal(String),
    /// Rerun target slot index out of bounds (DD-B5/DD-B6).
    #[error("rerun slot {0} out of bounds")]
    InvalidSlot(usize),
    /// Rerun target agent id not found in the spec map (DD-B5/DD-B6).
    #[error("rerun spec not found for agent {0:?}")]
    SpecNotFound(AgentId),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spoke(label: &str) -> SpokeSpec {
        SpokeSpec {
            id: AgentId::new(),
            label: label.into(),
            prompt: format!("explore {label}"),
            effective_model: "test-model".into(),
            tier: ModelTier::Flagship,
            tools_allow: ToolPolicy::InheritFromParent,
            waits_for: Vec::new(),
            role: SpokeRole::Leaf,
        }
    }

    #[test]
    fn wait_policy_all_is_default_and_active() {
        assert!(matches!(WaitPolicy::default(), WaitPolicy::All));
        assert!(WaitPolicy::All.is_active_in_r1());
        assert!(WaitPolicy::All.r1_unsupported_reason().is_none());
    }

    #[test]
    fn wait_policy_quorum_is_inert_in_r1() {
        assert!(!WaitPolicy::Quorum(2).is_active_in_r1());
        assert!(WaitPolicy::Quorum(2).r1_unsupported_reason().is_some());
    }

    #[test]
    fn spoke_waits_for_default_empty() {
        assert!(spoke("a").waits_for.is_empty());
    }

    #[test]
    fn coverage_counts_every_variant() {
        let results = vec![
            SpokeResult::Completed {
                summary: "a".into(),
            },
            SpokeResult::Completed {
                summary: "b".into(),
            },
            SpokeResult::Failed {
                reason: "boom".into(),
            },
            SpokeResult::Cancelled,
            SpokeResult::Empty,
        ];
        let line = CoverageLine::from_results(&results);
        assert_eq!(line.completed, 2);
        assert_eq!(line.failed, 1);
        assert_eq!(line.cancelled, 1);
        assert_eq!(line.empty, 1);
        assert_eq!(line.total, 5);
        // Postcondition: every spoke accounted for exactly once.
        assert_eq!(
            line.completed + line.failed + line.cancelled + line.empty,
            line.total
        );
        assert!(!line.is_honest_empty());
        assert!(line.render().contains("over 2 of 5"));
    }

    #[test]
    fn coverage_honest_empty_renders_no_signal() {
        let results = vec![SpokeResult::Cancelled, SpokeResult::Empty];
        let line = CoverageLine::from_results(&results);
        assert!(line.is_honest_empty());
        assert!(line.render().contains("no signal"));
    }

    #[test]
    fn synthesis_postcondition_one_citation_per_completed_spoke() {
        let results = vec![
            SpokeResult::Completed {
                summary: "a".into(),
            },
            SpokeResult::Failed { reason: "x".into() },
            SpokeResult::Completed {
                summary: "b".into(),
            },
        ];
        let coverage = CoverageLine::from_results(&results);
        let citations = vec![
            SpokeCitation {
                agent_id: AgentId::from_validated("a1"),
                label: "a".into(),
                summary: "a".into(),
            },
            SpokeCitation {
                agent_id: AgentId::from_validated("b1"),
                label: "b".into(),
                summary: "b".into(),
            },
        ];
        let view = SynthesisView::build(citations, coverage);
        assert_eq!(view.citations.len(), view.coverage.completed);
        assert!(!view.honest_empty);
        assert!(view.summary.contains("a") && view.summary.contains("b"));
    }

    #[test]
    fn synthesis_honest_empty_when_no_signal() {
        let results = vec![SpokeResult::Cancelled, SpokeResult::Empty];
        let coverage = CoverageLine::from_results(&results);
        let view = SynthesisView::build(Vec::new(), coverage);
        assert!(view.honest_empty);
        assert!(view.summary.contains("no spoke contributed"));
    }

    #[test]
    fn spoke_result_signal_and_degraded_partitions() {
        assert!(SpokeResult::Completed { summary: "".into() }.is_signal());
        assert!(!SpokeResult::Completed { summary: "".into() }.is_degraded());
        assert!(SpokeResult::Failed { reason: "".into() }.is_degraded());
        assert!(SpokeResult::Cancelled.is_degraded());
        assert!(SpokeResult::Empty.is_degraded());
    }

    #[test]
    fn fork_join_spawn_cap_below_node_tree_max_children() {
        // The wave must never exceed the node-tree's per-parent sibling ceiling.
        const {
            assert!(FORK_JOIN_SPAWN_CAP <= 10);
        }
        const {
            assert!(FORK_JOIN_SPAWN_CAP >= 1);
        }
    }

    #[test]
    fn wait_reason_escalation_predicate() {
        assert!(WaitReason::AwaitingSpoke.escalates());
        assert!(!WaitReason::BudgetPaused.escalates());
    }

    #[test]
    fn orchestration_error_is_non_exhaustive_friendly() {
        let error = OrchestrationError::DependencyCycle;
        assert!(error.to_string().contains("cycle"));
    }
}
