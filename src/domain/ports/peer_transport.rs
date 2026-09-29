use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

use crate::domain::models::{AgentEnvelope, FrameReply, FrameVerdict, PeerId};

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
///
/// ⚠ Not `Clone` and not comparable, on purpose: it now owns the one channel the
/// sender is waiting on, and a frame that could be duplicated would let two
/// answers race for one verdict.
#[derive(Debug)]
pub struct InboundFrame {
    pub envelope: AgentEnvelope<Value>,
    pub peer_id: PeerId,
    /// Where this receiver answers the sender.
    ///
    /// `None` for a frame that arrived with no answer channel — a fire-and-forget
    /// stream, or a hermetic fixture driving `process_frame` directly. A missing
    /// responder is never an error: the sender then reports
    /// [`crate::domain::models::FrameOutcome::Unanswered`], which is the honest
    /// thing and ⛔ never acceptance.
    pub responder: Option<FrameResponder>,
}

/// The receiver's one-shot answer channel for one frame.
///
/// The verdict travels back over the same authenticated connection, so it needs
/// no signature of its own: `dial` already fails
/// [`PeerTransportError::SignatureInvalid`] when the remote identity does not
/// match the pinned key. This type exists so the *domain* can answer without
/// holding a transport stream — the adapter owns the stream and the wire codec,
/// and one codec is the whole point.
#[derive(Debug)]
pub struct FrameResponder(oneshot::Sender<FrameReply>);

impl FrameResponder {
    /// Create the responder and the receiver the transport adapter awaits.
    #[must_use]
    pub fn channel() -> (Self, oneshot::Receiver<FrameReply>) {
        let (tx, rx) = oneshot::channel();
        (Self(tx), rx)
    }

    /// Answer once. A dropped receiver is not an error: the sender may already
    /// have stopped waiting, and the receiver's own durable row is the
    /// independent record either way.
    ///
    /// ⚑ Takes a [`FrameReply`], ⛔ not a `FrameVerdict` (Story 18.4c review):
    /// a receiver observes no path, and the reply type has none to drop — so
    /// no adapter can hand a reply back from `send_to` as though it were this
    /// host's own verdict.
    pub fn answer(self, reply: FrameReply) {
        let _ = self.0.send(reply);
    }
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
    /// The implementation does not provide an optional capability.
    ///
    /// ⚑ Story 18.4a: the default [`PeerTransport::gossip_topic`] returns this
    /// rather than `Ok(())`. A defaulted method that silently succeeds is a
    /// mechanism whose absence is indistinguishable from its presence — the
    /// false-green this port's own `send_to` doc warns about.
    #[error("peer {peer_id} transport does not provide {capability}")]
    Unsupported {
        peer_id: String,
        capability: &'static str,
    },
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

    /// Send one signed envelope to an addressed peer and learn what became of it.
    ///
    /// ⚠ **Reshaped by Story 18.4d, not duplicated.** This used to return `()`
    /// over a fire-and-forget stream, which made every honest caller say only
    /// "written" — and, worse, left the sender unable to learn the feed position
    /// the receiver expects, so a short-lived sender could never chain a second
    /// frame. Adding an acknowledged twin beside the old method would have
    /// shipped a method with no production caller; `send_to` had zero, so
    /// reshaping it cost only the shipped fixtures. If a later story needs
    /// fire-and-forget gossip, it adds one **with its own producer**.
    ///
    /// `Ok` means the frame was written and carries whatever answer came back —
    /// including [`crate::domain::models::FrameOutcome::Unanswered`] when none
    /// did. ⛔ A successful write is never acceptance.
    async fn send_to(
        &self,
        peer: &PeerId,
        envelope: AgentEnvelope<Value>,
    ) -> Result<FrameVerdict, PeerTransportError>;

    /// Gossip one signed topic frame to an addressed peer (Story 18.4a, FR150).
    ///
    /// ⚑ **The sibling [`Self::send_to`]'s doc authorized exactly this, with a
    /// condition**: *"if a later story needs fire-and-forget gossip, it adds one
    /// **with its own producer**."* Story 18.4a is that story;
    /// `VerifiedPeerFrameHandler::regossip` and `rustain peer share` are the two
    /// producers, and ⛔ this method must never ship without one.
    ///
    /// # Why it is not `send_to`
    ///
    /// `send_to` is *acknowledged*: every caller must handle a
    /// [`FrameVerdict`], because a message that was refused is a fact an
    /// operator has to be told. An advertisement is not a message that must
    /// land — a peer that never hears it simply keeps the head it holds — so
    /// this method exposes **no outcome at all**. ⛔ There is no spelling of
    /// "the peer accepted this advertisement", which is what makes the honest
    /// claim the only claim.
    ///
    /// # What `Ok` means, and the one thing it carries
    ///
    /// The frame was written. ⛔ Never acceptance, never delivery.
    ///
    /// `Ok(Some(position))` adds exactly one fact a stateless sender cannot
    /// otherwise learn: the [`FeedPosition`] the receiver said it would accept
    /// instead. That is 18.4d's D9 ruling applied here — *make the receiver tell
    /// the sender its feed position and you no longer need to store one* — and
    /// it is why no durable gossip cursor exists. `Ok(None)` means the receiver
    /// volunteered nothing, which includes the ordinary case where it took the
    /// frame. ⛔ A caller may use the position to re-sign **once**; a second
    /// guided retry per destination lets a hostile receiver spin the sender.
    ///
    /// # Default
    ///
    /// Defaulted so no existing implementation breaks (the
    /// additive-with-defaults discipline `ContextPort` documents). ⚠ The default
    /// is a refusal, ⛔ not a silent success: an implementation that has not
    /// opted in must not let a caller believe a head was advertised.
    async fn gossip_topic(
        &self,
        peer: &PeerId,
        envelope: AgentEnvelope<Value>,
    ) -> Result<Option<crate::domain::models::FeedPosition>, PeerTransportError> {
        let _ = envelope;
        Err(PeerTransportError::Unsupported {
            peer_id: peer.to_string(),
            capability: "topic gossip",
        })
    }

    /// Transfer ownership of the accepted-frame receiver to the caller.
    fn inbound(&self) -> Result<mpsc::Receiver<InboundFrame>, PeerTransportError>;

    /// Close the listener and all live connections.
    async fn shutdown(&self) -> Result<(), PeerTransportError>;
}
