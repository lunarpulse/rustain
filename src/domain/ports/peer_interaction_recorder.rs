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

/// A frame refused by the **transport allowlist**, before any envelope signature
/// was checked (Story 18.4d, AC6).
///
/// Distinct from [`PeerDeliveryRecord`] because it happens one layer earlier and
/// claims strictly less: there is no local recipient node, nothing was verified
/// cryptographically beyond the QUIC key binding, and no delivery was attempted.
/// Folding it into the delivery record would have made every one of those
/// absences look like a value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransportRefusalRecord {
    /// The remote identity the connection is bound to. QUIC binds the key, so
    /// this is who knocked — ⛔ it is **not** a statement about the envelope,
    /// whose signature this path never reaches.
    pub peer: PeerId,
    /// The typed refusal, in this host's own words. ⛔ Never remote-authored
    /// prose, and never a claim that a signature held.
    pub detail: String,
    /// The frame's correlation id, when the frame parsed far enough to carry one.
    pub correlation_id: Option<CorrelationId>,
}

/// Fail-closed recorder for verified peer-origin deliveries.
#[async_trait::async_trait]
pub trait PeerInteractionRecorder: Send + Sync {
    /// Persist a delivery outcome before an accepted peer message is allowed to
    /// remain live. Implementations return a sanitized diagnostic for logs only.
    async fn record_peer_delivery(&self, record: PeerDeliveryRecord) -> Result<(), String>;

    /// Persist one transport-allowlist refusal.
    ///
    /// ⚠ The caller is expected to have already applied a quota
    /// ([`crate::domain::services::refusal_quota`]): an unlisted stranger
    /// presents no credential, so a per-frame durable write here is an
    /// unbounded write an attacker controls.
    ///
    /// Deliberately **not** defaulted. A recorder that silently dropped these
    /// would make `peer revoke` have no receiver-side record, which is the one
    /// fact the two-host demo cross-checks.
    async fn record_transport_refusal(&self, record: TransportRefusalRecord) -> Result<(), String>;
}
