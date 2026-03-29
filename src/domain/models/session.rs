#![allow(dead_code)]
/// Type alias for session identifiers.
pub type SessionId = String;

/// Session lifecycle state machine.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SessionState {
    #[default]
    Empty,
    Active {
        id: SessionId,
    },
    Confirmed {
        id: SessionId,
    },
    Invalidated {
        previous_id: Option<SessionId>,
        needs_history_rebuild: bool,
    },
}
