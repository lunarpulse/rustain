use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::models::subagent_status::SubagentStatus;

/// Lifecycle state of a node (subagent or peer) in the A2A execution graph.
///
/// `#[non_exhaustive]` lets Epic 14 (A2A peer agents) and later stories add
/// states such as `AwaitingApproval` or `RemotePeer` without breaking
/// downstream match sites — the same strategy used by [`SubagentStatus`].
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    Created,
    Running,
    Waiting,
    Suspended,
    Completed,
    Failed,
    Cancelled,
}

/// Errors raised by illegal [`NodeState`] transitions.
#[non_exhaustive]
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeStateError {
    /// The `from -> to` pair is absent from [`NodeState::TRANSITIONS`].
    #[error("invalid transition from {from:?} to {to:?}")]
    InvalidTransition { from: NodeState, to: NodeState },
}

/// Static transition table: every legal `(from, to)` edge.
///
/// Kept as a `const` slice so the legal graph is *data*, not control flow —
/// adding a transition is a one-line edit here, not a new arm in
/// [`NodeState::can_transition_to`]. Terminal states (`Completed`, `Failed`,
/// `Cancelled`) deliberately have no outgoing edges.
const TRANSITIONS: &[(NodeState, NodeState)] = &[
    // Created ->
    (NodeState::Created, NodeState::Running),
    (NodeState::Created, NodeState::Cancelled),
    // Running ->
    (NodeState::Running, NodeState::Waiting),
    (NodeState::Running, NodeState::Suspended),
    (NodeState::Running, NodeState::Completed),
    (NodeState::Running, NodeState::Failed),
    (NodeState::Running, NodeState::Cancelled),
    // Waiting ->
    (NodeState::Waiting, NodeState::Running),
    (NodeState::Waiting, NodeState::Cancelled),
    // Suspended ->
    (NodeState::Suspended, NodeState::Running),
    (NodeState::Suspended, NodeState::Cancelled),
    // Completed / Failed / Cancelled -> (none, terminal)
];

impl NodeState {
    /// All known variants in canonical (declaration) order.
    ///
    /// Pinned by `NodeState::ALL.len() == 7` in the test suite so that adding a
    /// variant without updating dependents is caught loudly.
    pub const ALL: &[NodeState] = &[
        NodeState::Created,
        NodeState::Running,
        NodeState::Waiting,
        NodeState::Suspended,
        NodeState::Completed,
        NodeState::Failed,
        NodeState::Cancelled,
    ];

    /// Returns `true` for the terminal states (`Completed`, `Failed`, `Cancelled`).
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(
            self,
            NodeState::Completed | NodeState::Failed | NodeState::Cancelled
        )
    }

    /// Returns `true` if the `self -> target` edge exists in [`TRANSITIONS`].
    ///
    /// Table-driven: a linear scan over a static `const` slice — no runtime
    /// map, no allocation. The table is small (11 edges) so the scan beats a
    /// `HashMap` for every realistic call pattern.
    #[must_use]
    pub fn can_transition_to(&self, target: NodeState) -> bool {
        TRANSITIONS
            .iter()
            .any(|&(from, to)| from == *self && to == target)
    }

    /// Performs a legal transition in place, or returns the rejected pair.
    ///
    /// Validation runs via [`NodeState::can_transition_to`] *before* `self` is
    /// touched, so on `Err` the state is guaranteed unchanged.
    pub fn transition_or_err(&mut self, target: NodeState) -> Result<(), NodeStateError> {
        if self.can_transition_to(target) {
            *self = target;
            Ok(())
        } else {
            Err(NodeStateError::InvalidTransition {
                from: *self,
                to: target,
            })
        }
    }
}

/// Boundary conversion from the legacy subagent status vocabulary onto the
/// richer [`NodeState`] lifecycle.
///
/// Both running tiers (`RunningFg`, `RunningBg`) collapse to [`NodeState::Running`];
/// `Killed` maps to [`NodeState::Cancelled`]. The wildcard arm future-proofs
/// against new `#[non_exhaustive]` variants on [`SubagentStatus`] (e.g.
/// `AwaitingApproval`, `Lost`, `RemotePeer`) by parking them in the pre-flight
/// [`NodeState::Created`] bucket until they are explicitly mapped.
#[allow(unreachable_patterns)] // `_` guards future SubagentStatus variants
impl From<SubagentStatus> for NodeState {
    fn from(status: SubagentStatus) -> Self {
        match status {
            SubagentStatus::Idle => NodeState::Created,
            SubagentStatus::RunningFg => NodeState::Running,
            SubagentStatus::RunningBg => NodeState::Running,
            SubagentStatus::Completed => NodeState::Completed,
            SubagentStatus::Failed => NodeState::Failed,
            SubagentStatus::Killed => NodeState::Cancelled,
            _ => NodeState::Created,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::subagent_status::SubagentStatus;

    /// Ground-truth transition matrix.
    ///
    /// `EXPECTED[from][to]` is `true` iff `from.can_transition_to(to)` must
    /// hold. Row/column indices follow [`NodeState::ALL`] declaration order:
    /// `0=Created, 1=Running, 2=Waiting, 3=Suspended, 4=Completed, 5=Failed,
    /// 6=Cancelled`.
    const EXPECTED: [[bool; 7]; 7] = [
        //Created Running  Waiting Suspended Completed Failed  Cancelled
        [false, true, false, false, false, false, true], // Created
        [false, false, true, true, true, true, true],    // Running
        [false, true, false, false, false, false, true], // Waiting
        [false, true, false, false, false, false, true], // Suspended
        [false, false, false, false, false, false, false], // Completed
        [false, false, false, false, false, false, false], // Failed
        [false, false, false, false, false, false, false], // Cancelled
    ];

    #[test]
    fn variant_count_is_pinned_at_seven() {
        assert_eq!(NodeState::ALL.len(), 7);
    }

    #[test]
    fn all_const_matches_declaration_order() {
        assert_eq!(NodeState::ALL[0], NodeState::Created);
        assert_eq!(NodeState::ALL[1], NodeState::Running);
        assert_eq!(NodeState::ALL[2], NodeState::Waiting);
        assert_eq!(NodeState::ALL[3], NodeState::Suspended);
        assert_eq!(NodeState::ALL[4], NodeState::Completed);
        assert_eq!(NodeState::ALL[5], NodeState::Failed);
        assert_eq!(NodeState::ALL[6], NodeState::Cancelled);
    }

    #[test]
    fn full_transition_matrix_is_49_assertions() {
        for (i, &from) in NodeState::ALL.iter().enumerate() {
            for (j, &to) in NodeState::ALL.iter().enumerate() {
                assert_eq!(
                    from.can_transition_to(to),
                    EXPECTED[i][j],
                    "can_transition_to({from:?} -> {to:?}): expected {}, got {}",
                    EXPECTED[i][j],
                    !EXPECTED[i][j],
                );
            }
        }
    }

    #[test]
    fn is_terminal_correct_for_every_variant() {
        let expected_terminal = [
            false, // Created
            false, // Running
            false, // Waiting
            false, // Suspended
            true,  // Completed
            true,  // Failed
            true,  // Cancelled
        ];
        for (i, &state) in NodeState::ALL.iter().enumerate() {
            assert_eq!(
                state.is_terminal(),
                expected_terminal[i],
                "is_terminal({state:?}): expected {}, got {}",
                expected_terminal[i],
                !expected_terminal[i],
            );
        }
    }

    #[test]
    fn transition_or_err_positive_control_created_to_running() {
        let mut state = NodeState::Created;
        assert_eq!(state.transition_or_err(NodeState::Running), Ok(()));
        assert_eq!(state, NodeState::Running, "state must advance on success");
    }

    #[test]
    fn transition_or_err_negative_control_leaves_state_unchanged() {
        let mut state = NodeState::Completed;
        let err = state
            .transition_or_err(NodeState::Running)
            .expect_err("terminal -> Running must be rejected");
        assert_eq!(
            err,
            NodeStateError::InvalidTransition {
                from: NodeState::Completed,
                to: NodeState::Running,
            }
        );
        assert_eq!(
            state,
            NodeState::Completed,
            "self must be untouched on the error path"
        );
    }

    #[test]
    fn from_subagent_status_maps_every_variant() {
        assert_eq!(NodeState::from(SubagentStatus::Idle), NodeState::Created);
        assert_eq!(
            NodeState::from(SubagentStatus::RunningFg),
            NodeState::Running
        );
        assert_eq!(
            NodeState::from(SubagentStatus::RunningBg),
            NodeState::Running
        );
        assert_eq!(
            NodeState::from(SubagentStatus::Completed),
            NodeState::Completed
        );
        assert_eq!(NodeState::from(SubagentStatus::Failed), NodeState::Failed);
        assert_eq!(
            NodeState::from(SubagentStatus::Killed),
            NodeState::Cancelled
        );
    }
}
