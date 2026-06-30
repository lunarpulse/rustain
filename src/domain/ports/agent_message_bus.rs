use async_trait::async_trait;
use thiserror::Error;

use crate::domain::models::{
    AgentId, DeliveryDisposition, DeliveryOutcome, MessageHeader, OwnershipKind, RefuseReason,
    relationship_disposition,
};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DeliveryError {
    #[error("recipient not found: {0:?}")]
    NotFound(AgentId),
    #[error("remote recipient unsupported in R1: {0:?}")]
    RemoteUnsupported(AgentId),
    #[error("recipient cannot receive messages in current state: {0:?}")]
    Refused(RefuseReason),
    #[error("recipient inbox is full: {0:?}")]
    Full(AgentId),
    #[error("recipient channel is closed: {0:?}")]
    Closed(AgentId),
}

#[async_trait]
pub trait AgentMessageBus: Send + Sync {
    async fn deliver(
        &self,
        to: &AgentId,
        env: crate::domain::models::Envelope<crate::domain::models::AgentMessage>,
    ) -> Result<DeliveryOutcome, DeliveryError>;
}

pub trait DeliveryPolicy: Send + Sync {
    fn decide(
        &self,
        header: &MessageHeader,
        recipient_ownership: OwnershipKind,
    ) -> DeliveryDisposition;
}

#[derive(Clone, Debug, Default)]
pub struct RelationshipDeliveryPolicy;

impl DeliveryPolicy for RelationshipDeliveryPolicy {
    fn decide(
        &self,
        _header: &MessageHeader,
        recipient_ownership: OwnershipKind,
    ) -> DeliveryDisposition {
        relationship_disposition(recipient_ownership)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{CorrelationId, MessageKind};

    fn header() -> MessageHeader {
        MessageHeader {
            sender: AgentId("parent".into()),
            recipient: AgentId("child".into()),
            correlation_id: CorrelationId::new("c"),
            kind: MessageKind::PeerMessage,
            sequence: None,
        }
    }

    #[test]
    fn ac4_relationship_policy_routes_owned_and_peer_through_same_seam() {
        let policy = RelationshipDeliveryPolicy;
        assert_eq!(
            policy.decide(&header(), OwnershipKind::Owned),
            DeliveryDisposition::MustReport
        );
        assert_eq!(
            policy.decide(&header(), OwnershipKind::Peer),
            DeliveryDisposition::MayRefuse
        );
    }
}
