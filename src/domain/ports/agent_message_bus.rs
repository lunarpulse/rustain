use async_trait::async_trait;
use std::sync::Arc;
use thiserror::Error;

use crate::domain::models::{
    AgentId, DeliveryDisposition, DeliveryOutcome, EffectivePolicy, MessageHeader, OwnershipKind,
    PeerId, RefuseReason, ResponseMode, relationship_disposition,
};
use crate::domain::services::team_policy::sender_policy_for;

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
/// Response automation selected independently from relationship consent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerResponsePolicy {
    pub mode: ResponseMode,
    pub auto_response: Option<String>,
}

impl Default for PeerResponsePolicy {
    fn default() -> Self {
        Self {
            mode: ResponseMode::NotifyAndWait,
            auto_response: None,
        }
    }
}

pub trait DeliveryPolicy: Send + Sync {
    fn decide(
        &self,
        header: &MessageHeader,
        recipient_ownership: OwnershipKind,
    ) -> DeliveryDisposition;

    fn response_policy(&self, header: &MessageHeader) -> PeerResponsePolicy {
        header
            .verified_peer_id
            .as_ref()
            .map_or_else(PeerResponsePolicy::default, |peer_id| {
                self.response_policy_for_peer(peer_id)
            })
    }

    fn response_policy_for_peer(&self, peer_id: &PeerId) -> PeerResponsePolicy {
        let _ = peer_id;
        PeerResponsePolicy::default()
    }
}

#[derive(Clone, Debug, Default)]
pub struct RelationshipDeliveryPolicy;

impl DeliveryPolicy for RelationshipDeliveryPolicy {
    fn decide(
        &self,
        header: &MessageHeader,
        recipient_ownership: OwnershipKind,
    ) -> DeliveryDisposition {
        let _ = header;
        relationship_disposition(recipient_ownership)
    }
}

/// Consent-only delivery policy backed by the startup-resolved workspace policy.
/// Response modes are queried separately through [`DeliveryPolicy::response_policy`]
/// and [`DeliveryPolicy::response_policy_for_peer`].
#[derive(Clone, Debug)]
pub struct EffectiveDeliveryPolicy {
    policy: Arc<EffectivePolicy>,
}

impl EffectiveDeliveryPolicy {
    #[must_use]
    pub fn new(policy: Arc<EffectivePolicy>) -> Self {
        Self { policy }
    }
}

impl DeliveryPolicy for EffectiveDeliveryPolicy {
    fn decide(
        &self,
        _header: &MessageHeader,
        recipient_ownership: OwnershipKind,
    ) -> DeliveryDisposition {
        relationship_disposition(recipient_ownership)
    }

    fn response_policy_for_peer(&self, peer_id: &PeerId) -> PeerResponsePolicy {
        let sender = sender_policy_for(&self.policy, peer_id);
        PeerResponsePolicy {
            mode: sender
                .and_then(|policy| policy.response_mode.as_ref())
                .map_or(self.policy.automation.value, |mode| mode.value),
            auto_response: sender.and_then(|policy| policy.auto_response.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{CorrelationId, MessageKind};

    fn header() -> MessageHeader {
        MessageHeader {
            sender: AgentId::from_validated("parent"),
            recipient: AgentId::from_validated("child"),
            correlation_id: CorrelationId::new("c"),
            kind: MessageKind::PeerMessage,
            sequence: None,
            verified_peer_id: None,
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

    #[test]
    fn effective_policy_uses_verified_sender_and_keeps_relationship_separate() {
        use crate::domain::models::{
            IndividualPolicy, PeerId, PolicySource, Resolved, SenderBinding, SenderIdentity,
            SenderPolicy,
        };

        let sender = PeerId::from_public_key(&[7_u8; 32]).expect("valid peer id");
        let mut effective = crate::domain::services::team_policy::resolve_effective_policy(
            &IndividualPolicy::default(),
            None,
            &[],
        );
        effective.sender_overrides.push(SenderPolicy {
            alias: "trusted-peer".to_owned(),
            identity: SenderIdentity::Pinned {
                peer_id: sender.clone(),
                binding: SenderBinding::DeclaredPeerId,
            },
            response_mode: Some(Resolved {
                value: ResponseMode::NotifyAndAuto,
                source: PolicySource::Default,
                individual: ResponseMode::NotifyAndAuto,
                team: None,
            }),
            notification: None,
            auto_response: Some("acknowledged".to_owned()),
            deferred_types: vec![],
        });
        let policy = EffectiveDeliveryPolicy::new(Arc::new(effective));

        let mut verified = header();
        verified.verified_peer_id = Some(sender);
        assert_eq!(
            policy.response_policy(&verified),
            PeerResponsePolicy {
                mode: ResponseMode::NotifyAndAuto,
                auto_response: Some("acknowledged".to_owned()),
            }
        );
        assert_eq!(
            policy.decide(&verified, OwnershipKind::Peer),
            DeliveryDisposition::MayRefuse,
            "response automation must not mutate relationship consent"
        );

        let mut claimed_only = header();
        claimed_only.sender = AgentId::from_validated("trusted-peer");
        assert_eq!(
            policy.response_policy(&claimed_only).mode,
            ResponseMode::NotifyAndWait,
            "a claimed AgentId must not impersonate a verified peer identity"
        );
    }
}
