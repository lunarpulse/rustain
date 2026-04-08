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
    Invalidated {
        previous_id: Option<SessionId>,
        needs_history_rebuild: bool,
    },
}

/// Owned, mutable state machine for session lifecycle management.
/// Single-threaded — not behind `Arc`. Lives in the event loop.
#[derive(Debug)]
pub struct SessionManager {
    state: SessionState,
}

impl SessionManager {
    pub fn new(state: SessionState) -> Self {
        Self { state }
    }

    /// Check if the current state requires history rebuild (session expiry).
    pub fn needs_history_rebuild(&self) -> bool {
        matches!(
            self.state,
            SessionState::Invalidated {
                needs_history_rebuild: true,
                ..
            }
        )
    }

    /// Transition to Invalidated state (on session expiry / provider error).
    pub fn mark_invalidated(&mut self, previous_id: Option<SessionId>) {
        self.state = SessionState::Invalidated {
            previous_id,
            needs_history_rebuild: true,
        };
    }

    /// Transition to Active state (after successful rebuild or new session).
    pub fn mark_active(&mut self, id: SessionId) {
        self.state = SessionState::Active { id };
    }

    /// Get the current session state.
    pub fn state(&self) -> &SessionState {
        &self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_manager_empty_to_active() {
        let mut mgr = SessionManager::new(SessionState::Empty);
        assert!(!mgr.needs_history_rebuild());

        mgr.mark_active("sess-1".to_string());
        assert_eq!(
            *mgr.state(),
            SessionState::Active {
                id: "sess-1".to_string()
            }
        );
    }

    #[test]
    fn test_session_manager_active_to_invalidated() {
        let mut mgr = SessionManager::new(SessionState::Active {
            id: "sess-1".to_string(),
        });
        assert!(!mgr.needs_history_rebuild());

        mgr.mark_invalidated(Some("sess-1".to_string()));
        assert!(mgr.needs_history_rebuild());
        assert_eq!(
            *mgr.state(),
            SessionState::Invalidated {
                previous_id: Some("sess-1".to_string()),
                needs_history_rebuild: true,
            }
        );
    }

    #[test]
    fn test_session_manager_invalidated_to_active() {
        let mut mgr = SessionManager::new(SessionState::Invalidated {
            previous_id: Some("sess-1".to_string()),
            needs_history_rebuild: true,
        });
        assert!(mgr.needs_history_rebuild());

        mgr.mark_active("sess-2".to_string());
        assert!(!mgr.needs_history_rebuild());
        assert_eq!(
            *mgr.state(),
            SessionState::Active {
                id: "sess-2".to_string()
            }
        );
    }
}
