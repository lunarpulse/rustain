use serde::{Deserialize, Serialize};

use crate::domain::models::{AgentId, NodeState, OwnershipKind};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CorrelationId(pub String);

impl CorrelationId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageKind {
    PeerMessage,
    OwnerReport,
    Refusal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageHeader {
    pub sender: AgentId,
    pub recipient: AgentId,
    pub correlation_id: CorrelationId,
    pub kind: MessageKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub header: MessageHeader,
    pub body: T,
}

impl<T> Envelope<T> {
    pub(crate) fn new(header: MessageHeader, body: T) -> Self {
        Self { header, body }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMessage {
    pub content: String,
}

impl AgentMessage {
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
        }
    }
}

#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryMode {
    Aside,
    Wake,
    Queue,
    Refuse,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentDelivery {
    pub envelope: Envelope<AgentMessage>,
    pub mode: DeliveryMode,
}

impl AgentDelivery {
    pub(crate) fn new(envelope: Envelope<AgentMessage>, mode: DeliveryMode) -> Self {
        Self { envelope, mode }
    }
}

#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefuseReason {
    Capacity,
    TerminalState,
    Policy,
}

#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryOutcome {
    Delivered,
    Queued,
    Refused { reason: RefuseReason },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryDisposition {
    MustReport,
    MayRefuse,
}

pub fn delivery_decision(recipient_status: NodeState) -> DeliveryMode {
    match recipient_status {
        NodeState::Running => DeliveryMode::Aside,
        NodeState::Created | NodeState::Waiting => DeliveryMode::Wake,
        NodeState::Suspended => DeliveryMode::Queue,
        NodeState::Completed | NodeState::Failed | NodeState::Cancelled => DeliveryMode::Refuse,
    }
}

pub fn relationship_disposition(ownership: OwnershipKind) -> DeliveryDisposition {
    match ownership {
        OwnershipKind::Self_(_) | OwnershipKind::Owned => DeliveryDisposition::MustReport,
        OwnershipKind::Peer => DeliveryDisposition::MayRefuse,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> AgentId {
        AgentId(s.to_string())
    }

    #[test]
    fn ac1_delivery_decision_maps_statuses_with_positive_controls() {
        assert_eq!(delivery_decision(NodeState::Running), DeliveryMode::Aside);
        assert_eq!(delivery_decision(NodeState::Waiting), DeliveryMode::Wake);
        assert_eq!(delivery_decision(NodeState::Created), DeliveryMode::Wake);
        assert_eq!(delivery_decision(NodeState::Suspended), DeliveryMode::Queue);
        assert_eq!(
            delivery_decision(NodeState::Completed),
            DeliveryMode::Refuse
        );
    }

    #[test]
    fn ac4_relationship_disposition_owned_and_peer_are_distinct() {
        assert_eq!(
            relationship_disposition(OwnershipKind::Owned),
            DeliveryDisposition::MustReport
        );
        assert_eq!(
            relationship_disposition(OwnershipKind::self_root()),
            DeliveryDisposition::MustReport
        );
        assert_eq!(
            relationship_disposition(OwnershipKind::Peer),
            DeliveryDisposition::MayRefuse
        );
    }

    #[test]
    fn ac5_header_round_trips_with_sequence_none() {
        let header = MessageHeader {
            sender: id("parent"),
            recipient: id("child"),
            correlation_id: CorrelationId::new("c-1"),
            kind: MessageKind::PeerMessage,
            sequence: None,
        };
        let env = Envelope::new(header, AgentMessage::new("hello"));
        let json = serde_json::to_string(&env).unwrap();
        assert!(!json.contains("sequence"));
        let round: Envelope<AgentMessage> = serde_json::from_str(&json).unwrap();
        assert_eq!(round.header.correlation_id, CorrelationId::new("c-1"));
        assert_eq!(round.header.sequence, None);
    }
}
