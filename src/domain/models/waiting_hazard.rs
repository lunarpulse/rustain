//! Pure `Waiting → Hazard ▲` escalation policy (AC5).
//!
//! The hazard is a *derived marker*, never a new `NodeState` variant or FSM
//! edge. Dwell is measured in persisted wall-clock milliseconds so a node that
//! sat in `Waiting` across a restart keeps accumulating dwell — a monotonic
//! `Instant` would reset to zero and never escalate.

use crate::domain::models::agent_node::NodeCheckpoint;
use crate::domain::models::node_state::NodeState;

/// Default dwell ceiling before a waiting node escalates to a hazard.
pub const WAITING_HAZARD_THRESHOLD_MS: i64 = 60_000;

/// Derived hazard marker. Carries the observed dwell; it does not mutate the
/// node or its lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaitingHazard {
    pub dwell_ms: i64,
    pub threshold_ms: i64,
}

/// Return a hazard marker when a `Waiting` node has dwelled at least
/// `threshold_ms` measured from its persisted `waiting_since` wall clock.
///
/// R-2 (17.5b): the reason is consulted, not merely stored. A `Waiting` node
/// carrying a non-escalating reason (today only `BudgetPaused`, a deliberate
/// recoverable pause) does NOT raise a hazard. This is the load-bearing read
/// that makes `WaitReason::escalates()` and Task 3's stamp seam meaningful;
/// without it, adding `AwaitingHumanInput` to `escalates()` has zero effect.
#[must_use]
pub fn waiting_hazard(
    checkpoint: &NodeCheckpoint,
    now_ms: i64,
    threshold_ms: i64,
) -> Option<WaitingHazard> {
    if checkpoint.state != NodeState::Waiting {
        return None;
    }
    // A stamped reason that does not escalate short-circuits — the only path
    // to `Waiting` with a non-escalating reason today is `BudgetPaused`. An
    // unstamped node (`None`, the historical default) still escalates.
    match checkpoint.wait_reason {
        Some(reason) if !reason.escalates() => return None,
        _ => {}
    }
    let waiting_since = checkpoint.waiting_since?;
    let dwell_ms = now_ms.saturating_sub(waiting_since);
    (dwell_ms >= threshold_ms).then_some(WaitingHazard {
        dwell_ms,
        threshold_ms,
    })
}
