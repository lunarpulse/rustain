//! Story 17.1a — Task 4 / AC2 context: RAP `TaskState` lifecycle FSM.
//!
//! `RapTaskState` currently exists only as a flat `#[non_exhaustive]` enum
//! (`R1_STATES` + `as_str`). Task 4 requires it to **mirror `NodeState`** — a
//! table-driven state machine with terminal-state semantics and a legal-edge
//! table that 17.4b's `A2aCompat` translates against. That machinery
//! (`is_terminal`, `can_transition_to`, `transition_or_err`, an error type) is
//! **not yet implemented**, so these tests fail to compile until it lands —
//! genuine red-first.
//!
//! Expected API (mirror `domain::models::node_state::NodeState` exactly):
//! - `RapTaskState::is_terminal(&self) -> bool`
//! - `RapTaskState::can_transition_to(&self, target: RapTaskState) -> bool`
//! - `RapTaskState::transition_or_err(&mut self, target) -> Result<(), RapTaskStateError>`
//!
//! Invariants pinned (each unambiguous from A2A semantics / the NodeState
//! precedent — the full edge table is Main's to design; these only fix it):
//! - The 4 terminal states (`Completed`/`Failed`/`Canceled`/`Rejected`) reject
//!   ALL outgoing transitions (mirrors NodeState's documented invariant).
//! - `Submitted -> Working` is the canonical legal start edge.

use rustain::domain::models::RapTaskState;

const TERMINAL: [RapTaskState; 4] = [
    RapTaskState::Completed,
    RapTaskState::Failed,
    RapTaskState::Canceled,
    RapTaskState::Rejected,
];

const NON_TERMINAL: [RapTaskState; 4] = [
    RapTaskState::Submitted,
    RapTaskState::Working,
    RapTaskState::InputRequired,
    RapTaskState::AuthRequired,
];

#[test]
fn is_terminal_flags_exactly_the_four_terminal_states() {
    for state in NON_TERMINAL {
        assert!(
            !state.is_terminal(),
            "{state:?} must NOT be terminal — it has outgoing edges in the A2A lifecycle"
        );
    }
    for state in TERMINAL {
        assert!(
            state.is_terminal(),
            "{state:?} MUST be terminal — a finished/rejected task never resumes"
        );
    }
}

#[test]
fn terminal_states_admit_no_outgoing_transitions() {
    // Mirrors NodeState: terminal states have no edges in the transition table.
    // Every terminal -> X must be rejected, for every possible target. A
    // terminal state that can resume is a correctness bug (a Completed task
    // flipping back to Working would lie about finality to 17.4b's translator).
    let all_known = [
        RapTaskState::Submitted,
        RapTaskState::Working,
        RapTaskState::InputRequired,
        RapTaskState::AuthRequired,
        RapTaskState::Completed,
        RapTaskState::Failed,
        RapTaskState::Canceled,
        RapTaskState::Rejected,
    ];
    for from in TERMINAL {
        for target in all_known {
            assert!(
                !from.can_transition_to(target),
                "terminal {from:?} must not transition to {target:?}"
            );
        }
    }
}

#[test]
fn transition_or_err_rejects_illegal_edge_without_mutating() {
    // A rejected transition leaves the state untouched (mirrors NodeState's
    // validate-before-mutate contract). `Completed -> Working` is illegal
    // because Completed is terminal — a finished task never resumes. This is
    // the load-bearing terminal invariant, not a contested edge-design call.
    let mut state = RapTaskState::Completed;
    let result = state.transition_or_err(RapTaskState::Working);
    assert!(
        result.is_err(),
        "Completed -> Working must be rejected (terminal)"
    );
    assert_eq!(
        state,
        RapTaskState::Completed,
        "rejected transition must not mutate state"
    );
}

#[test]
fn transition_or_err_applies_legal_edge() {
    // The canonical happy-path start: Submitted -> Working.
    let mut state = RapTaskState::Submitted;
    state
        .transition_or_err(RapTaskState::Working)
        .expect("Submitted -> Working is the canonical legal start edge");
    assert_eq!(state, RapTaskState::Working);
}
