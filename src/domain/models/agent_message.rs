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
    /// Transport-authenticated peer identity. Claimed sender ids never drive policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_peer_id: Option<super::PeerId>,
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
    /// Story 14-4a (AC3) — consent disposition stamped by the bus at delivery
    /// time. The recipient's `Op::Deliver` dispatch enforces it: `MayRefuse`
    /// may consent-refuse; `MustReport` must process.
    pub disposition: DeliveryDisposition,
    /// Response automation selected independently from relationship consent.
    pub response_policy: crate::domain::ports::PeerResponsePolicy,
}

impl AgentDelivery {
    pub(crate) fn new(
        envelope: Envelope<AgentMessage>,
        mode: DeliveryMode,
        disposition: DeliveryDisposition,
    ) -> Self {
        Self::new_with_response_policy(envelope, mode, disposition, Default::default())
    }

    pub(crate) fn new_with_response_policy(
        envelope: Envelope<AgentMessage>,
        mode: DeliveryMode,
        disposition: DeliveryDisposition,
        response_policy: crate::domain::ports::PeerResponsePolicy,
    ) -> Self {
        Self {
            envelope,
            mode,
            disposition,
            response_policy,
        }
    }
}

#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefuseReason {
    /// Admission-synchronous only: the recipient's mailbox budget is full.
    /// Receipts never carry this reason because capacity failures never pass
    /// admission (they return `DeliveryError::Full` synchronously).
    Capacity,
    /// The recipient has entered a terminal state (Completed/Failed/Cancelled).
    TerminalState,
    /// Consent-refusal: the recipient's delivery policy refused the message
    /// (e.g., a Peer with a RefuseAll disposition).
    Policy,
    /// The recipient is a crash-recovered node not yet resumed (a later story
    /// attaches the live runner). There is no consumer for a queued delivery,
    /// so accepting it would silently black-hole the message.
    AwaitingResume,
    /// The recipient has no live consumer for this delivery. This is an
    /// operational refusal, never a consent-policy decision.
    Unavailable,
}

/// Story 14-4a (AC4) — honest outcome enum. `Accepted` is the sole variant:
/// "slot reserved + Op handed off; settlement is turn-injection or a receipt."
/// `Delivered` and `Queued` were lies the bus could not truthfully assert.
/// Synchronous failures remain `Err(DeliveryError::…)`; asynchronous facts
/// are receipts (`MessageRefused`).
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryOutcome {
    Accepted,
}

/// Consent disposition stamped by the bus at delivery time.
///
/// - `MustReport`: the recipient must process (Owned nodes). The durable node
///   journal records an obligation at accepted delivery, discharges it when
///   the recipient reports with the same correlation id, and records any
///   outstanding obligation when the node reaches a terminal state.
/// - `MayRefuse`: the recipient may consent-refuse (Peer nodes).
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

/// Story 14-4a (F8/F11), hoisted out of `run_child`'s nested scope by Story
/// 18.3 (Task 1) so both enforcement shells share one predicate.
///
/// Is this recipient **permitted** to consent-refuse? `MayRefuse` (a `Peer`
/// relationship) says yes; `MustReport` (`Owned`/`Self_`) says no — an owned
/// recipient owes a report and cannot decline the message.
///
/// Deliberately a *permission*, not a refusal. The two shells establish that the
/// recipient actually declined by different means — the in-process runner
/// declines every peer delivery outright (a local subagent runner has no peer
/// consumer), while the RAP peer path declines only when the verified-peer
/// consumer rejects the ingest. Both ask THIS function whether the relationship
/// lets that refusal count, so the `MayRefuse` check cannot drift between them.
///
/// Effect-free and value-returning per the Decision-Core Pattern (Story 18.0):
/// the budget release and the receipt emission belong to the shells.
pub fn may_consent_refuse(disposition: DeliveryDisposition) -> bool {
    disposition == DeliveryDisposition::MayRefuse
}

/// The one `MessageRefused` receipt shape, shared by every refusal path.
///
/// Story 18.3 (Task 1). The in-process runner and the RAP peer path emit through
/// different sinks (an `EventBus` vs. the daemon's `domain_tx`), so the
/// *emission* cannot be shared — but the receipt **value** must be identical, or
/// a sender learns a different story depending on which recipient refused.
/// Building it here is what prevents a second refusal path from existing.
///
/// Takes the `MessageHeader` rather than the whole `AgentDelivery` because the
/// RAP peer shell moves `envelope.body` into the verified-peer consumer before
/// it knows whether the ingest was declined — so no `&AgentDelivery` survives to
/// the refusal point. The header is everything a receipt needs anyway.
pub fn refusal_receipt(
    header: &MessageHeader,
    recipient: &AgentId,
    reason: RefuseReason,
) -> crate::domain::events::AppEvent {
    crate::domain::events::AppEvent::Subagent(crate::domain::models::SubagentEnvelope::new(
        header.sender.as_str().to_string(),
        recipient.clone(),
        header.kind.clone(),
        crate::domain::models::SubagentEvent::MessageRefused {
            correlation_id: header.correlation_id.clone(),
            reason,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> AgentId {
        AgentId::from_validated(s.to_string())
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
            verified_peer_id: None,
        };
        let env = Envelope::new(header, AgentMessage::new("hello"));
        let json = serde_json::to_string(&env).unwrap();
        assert!(!json.contains("sequence"));
        let round: Envelope<AgentMessage> = serde_json::from_str(&json).unwrap();
        assert_eq!(round.header.correlation_id, CorrelationId::new("c-1"));
        assert_eq!(round.header.sequence, None);
    }
}
