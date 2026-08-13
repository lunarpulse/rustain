use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::domain::models::{AgentEnvelope, PeerId};

/// Dialable transport coordinates encoded by an adapter.
///
/// The bytes are opaque to the domain: this is neither a peer identity nor an
/// iroh type. [`PeerId`] remains the only identity accepted by the port.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PeerAddress(Vec<u8>);

impl PeerAddress {
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, PeerTransportError> {
        if bytes.is_empty() {
            return Err(PeerTransportError::Address(
                "peer address must not be empty".to_owned(),
            ));
        }
        Ok(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// Frame asserted by the transport to originate from `peer_id`.
///
/// This frame is **unverified**. Callers must cryptographically verify the
/// envelope and its [`PeerId`] binding before dispatching it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboundFrame {
    pub envelope: AgentEnvelope<Value>,
    pub peer_id: PeerId,
}

/// Typed peer-transport failures. Refusal causes stay distinct from signature
/// and reachability failures so callers can explain the real operator action.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum PeerTransportError {
    #[error("peer allowlist is absent")]
    AllowlistAbsent,
    #[error("peer allowlist is empty")]
    AllowlistEmpty,
    #[error("peer allowlist is malformed: {0}")]
    AllowlistMalformed(String),
    #[error("configured peer {0} has no pinned Ed25519 key")]
    PeerUnpinned(String),
    #[error("peer {0} is not allowlisted")]
    PeerUnlisted(PeerId),
    #[error("peer signature is invalid: {0}")]
    SignatureInvalid(String),
    /// The signature verified but the frame's position in the sender's feed did
    /// not: a replay, a duplicate nonce, a fork, or another frame from the same
    /// peer still in flight. Distinct from [`Self::SignatureInvalid`] because the
    /// operator action is different — a replayed frame is not a forged one.
    #[error("peer frame was rejected by the replay window: {0}")]
    ReplayRejected(String),
    /// The envelope's own validity window closed before it arrived.
    #[error("peer frame expired: {0}")]
    FrameExpired(String),
    #[error("peer address is invalid: {0}")]
    Address(String),
    #[error("peer dial failed: {0}")]
    Dial(String),
    #[error("peer {0} is unreachable")]
    Unreachable(PeerId),
    #[error("peer send failed: {0}")]
    Send(String),
    #[error("the inbound frame receiver has already been taken")]
    InboundUnavailable,
    #[error("peer transport shutdown failed: {0}")]
    Shutdown(String),
    #[error("peer transport is closed")]
    Closed,
}

/// Addressed cross-host transport port.
///
/// [`AgentTransport`] is the two-method in-process signed-envelope bus and
/// `RapTransport` is its broadcast adapter. `PeerTransport` is their sibling:
/// it adds remote addressing, an accept side, and connection lifecycle without
/// widening the shipped in-process contract.
#[async_trait]
pub trait PeerTransport: Send + Sync {
    /// Return this endpoint's opaque, dialable address.
    fn local_address(&self) -> Result<PeerAddress, PeerTransportError>;

    /// Establish or confirm a connection to an already-addressable peer.
    async fn dial(&self, peer: &PeerId) -> Result<(), PeerTransportError>;

    /// Send one signed envelope to an addressed peer.
    async fn send_to(
        &self,
        peer: &PeerId,
        envelope: AgentEnvelope<Value>,
    ) -> Result<(), PeerTransportError>;

    /// Transfer ownership of the accepted-frame receiver to the caller.
    fn inbound(&self) -> Result<mpsc::Receiver<InboundFrame>, PeerTransportError>;

    /// Close the listener and all live connections.
    async fn shutdown(&self) -> Result<(), PeerTransportError>;
}
