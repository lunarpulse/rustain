//! Durable recording seam for verified peer-message interactions.
//!
//! RAP delivery cannot import the A2A transparency adapter merely to append an
//! audit record: adapters depend on domain ports, never on one another. The
//! composition root supplies an implementation when durable transparency is
//! available; a RAP-only deployment deliberately supplies none.

use crate::domain::models::{AgentId, CorrelationId, PeerId};

/// A peer-origin message that crossed (or was refused at) the local delivery
/// boundary. It carries metadata only; peer content never enters this port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerDeliveryRecord {
    /// Authenticated remote principal that sent the message.
    pub peer: PeerId,
    /// Local peer-owned node that received the delivery.
    pub node: AgentId,
    /// Remote correlation id, capped by the concrete recorder before journaling.
    pub correlation_id: CorrelationId,
    /// Byte length of the peer-supplied content that reached the delivery boundary.
    pub content_bytes: usize,
    /// Whether the delivery was accepted or consent-refused.
    pub outcome: PeerDeliveryOutcome,
}

/// The durable outcome of one peer-origin delivery.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerDeliveryOutcome {
    Accepted,
    Refused,
}

/// Fail-closed recorder for verified peer-origin deliveries.
#[async_trait::async_trait]
pub trait PeerInteractionRecorder: Send + Sync {
    /// Persist a delivery outcome before an accepted peer message is allowed to
    /// remain live. Implementations return a sanitized diagnostic for logs only.
    async fn record_peer_delivery(&self, record: PeerDeliveryRecord) -> Result<(), String>;
}
