use serde::{Deserialize, Serialize};

use crate::domain::models::{AgentId, CorrelationId, NodeState, RefuseReason};

#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubagentEvent {
    MessageDelivered {
        correlation_id: CorrelationId,
    },
    /// Story 14-4a (CS-4) — reshaped to carry `RefuseReason` directly.
    /// `Capacity` is admission-sync-only (documented; receipts never carry it
    /// because capacity never passes admission). `Policy` = consent-refusal.
    /// `TerminalState` = terminal-drain settlement.
    MessageRefused {
        correlation_id: CorrelationId,
        reason: RefuseReason,
    },
    StateChanged {
        state: NodeState,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentEnvelope {
    pub parent_tool_call_id: String,
    pub agent_id: AgentId,
    pub kind: crate::domain::models::MessageKind,
    pub event: SubagentEvent,
}

impl SubagentEnvelope {
    pub fn new(
        parent_tool_call_id: impl Into<String>,
        agent_id: AgentId,
        kind: crate::domain::models::MessageKind,
        event: SubagentEvent,
    ) -> Self {
        Self {
            parent_tool_call_id: parent_tool_call_id.into(),
            agent_id,
            kind,
            event,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{MessageKind, RefuseReason};

    #[test]
    fn subagent_event_variants_are_constructed_and_read() {
        let correlation_id = CorrelationId::new("corr");
        let delivered = SubagentEvent::MessageDelivered {
            correlation_id: correlation_id.clone(),
        };
        let refused = SubagentEvent::MessageRefused {
            correlation_id: correlation_id.clone(),
            reason: RefuseReason::Capacity,
        };
        let state = SubagentEvent::StateChanged {
            state: NodeState::Running,
        };
        assert!(matches!(delivered, SubagentEvent::MessageDelivered { .. }));
        assert!(matches!(refused, SubagentEvent::MessageRefused { .. }));
        assert_eq!(
            state,
            SubagentEvent::StateChanged {
                state: NodeState::Running
            }
        );
    }

    #[test]
    fn envelope_keeps_parent_tool_call_and_agent_identity() {
        let env = SubagentEnvelope::new(
            "tc-1",
            AgentId("child".into()),
            MessageKind::PeerMessage,
            SubagentEvent::StateChanged {
                state: NodeState::Waiting,
            },
        );
        assert_eq!(env.parent_tool_call_id, "tc-1");
        assert_eq!(env.agent_id, AgentId("child".into()));
    }
}
