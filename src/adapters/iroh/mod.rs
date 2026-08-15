mod ingress;

pub use ingress::{IrohPeerIngress, PeerIngressError};

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use ::iroh::endpoint::{Connection, presets};
use ::iroh::{Endpoint, EndpointAddr, EndpointId};
use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{Mutex, RwLock, Semaphore, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::domain::models::{
    AgentEnvelope, FeedPosition, FrameOutcome, FrameRefusal, FrameVerdict, PeerId,
};
use crate::domain::ports::{
    FrameResponder, InboundFrame, PeerAddress, PeerTransport, PeerTransportError,
};

const PEER_ALPN: &[u8] = b"rustain/peer/1";
const INBOUND_CAPACITY: usize = 128;
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Longest verdict this transport will read or write.
///
/// A verdict is an outcome, a class and a feed position — a few hundred bytes at
/// most. Bounding it is what stops a hostile receiver from answering a ping with
/// a stream it never ends.
const MAX_VERDICT_BYTES: usize = 4 * 1024;

/// How long either side waits for a verdict before calling the outcome unknown.
///
/// ⚠ A silent receiver must not stall the sender indefinitely, and a slow local
/// consumer must not hold a connection's stream queue forever.
const VERDICT_TIMEOUT: Duration = Duration::from_secs(20);

/// Limits unadmitted connections so each may consume at most one frame buffer
/// without allowing remote handshakes to grow tasks and heap without bound.
const MAX_CONCURRENT_INBOUND_CONNECTIONS: usize = 64;
/// Bounds a peer's handshake, stream wait, and frame read while it holds a slot.
const INBOUND_CONNECTION_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// The two identities derived from one Ed25519 public key.
///
/// `peer_id` is the stable domain identity. `endpoint_id` is transport-only
/// reachability data and must never enter room authority or provenance records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerEndpointIdentity {
    pub peer_id: PeerId,
    pub endpoint_id: EndpointId,
}

/// Derive the domain peer identity and iroh endpoint identifier from the same
/// Ed25519 public key.
pub fn derive_peer_endpoint_identity(
    public_key: &[u8],
) -> Result<PeerEndpointIdentity, PeerTransportError> {
    let key_bytes: [u8; 32] = public_key.try_into().map_err(|_| {
        PeerTransportError::Address(format!(
            "Ed25519 public key must be 32 bytes, got {}",
            public_key.len()
        ))
    })?;
    let peer_id = PeerId::from_public_key(&key_bytes)
        .map_err(|error| PeerTransportError::Address(error.to_string()))?;
    let endpoint_id = EndpointId::from_bytes(&key_bytes)
        .map_err(|error| PeerTransportError::Address(error.to_string()))?;
    Ok(PeerEndpointIdentity {
        peer_id,
        endpoint_id,
    })
}

/// iroh 1.0 adapter for the cross-host [`PeerTransport`] port.
///
/// The endpoint is built with [`presets::Minimal`]: no relay and no address
/// lookup. This cut therefore reaches directly-addressable peers only.
pub struct IrohPeerTransport {
    endpoint: Endpoint,
    peer_addresses: Arc<HashMap<PeerId, EndpointAddr>>,
    connections: Arc<RwLock<HashMap<PeerId, Connection>>>,
    inbound_rx: Mutex<Option<mpsc::Receiver<InboundFrame>>>,
    accept_cancel: CancellationToken,
    accept_task: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for IrohPeerTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IrohPeerTransport")
            .field("endpoint_id", &self.endpoint.id())
            .field("peer_count", &self.peer_addresses.len())
            .field("closed", &self.endpoint.is_closed())
            .finish_non_exhaustive()
    }
}

impl IrohPeerTransport {
    /// Bind an endpoint whose transport key is the same Ed25519 key used for
    /// signed peer envelopes.
    pub async fn bind(
        secret_key_bytes: [u8; 32],
        peer_addresses: HashMap<PeerId, PeerAddress>,
    ) -> Result<Self, PeerTransportError> {
        let mut decoded = HashMap::with_capacity(peer_addresses.len());
        for (peer_id, address) in peer_addresses {
            // A stored entry this build cannot read — or one whose endpoint
            // does not derive the pinned key — must not take the listener
            // down: reach degrades ("dial nobody for that peer"), exactly as
            // an absent or malformed reach store does. Only an operator edit
            // or disk damage can produce this; every write path filters.
            let endpoint_addr = match decode_address(&address) {
                Ok(endpoint_addr) => endpoint_addr,
                Err(error) => {
                    tracing::warn!(%peer_id, %error, "ignoring an unreadable reach entry");
                    continue;
                }
            };
            match derive_peer_endpoint_identity(endpoint_addr.id.as_bytes()) {
                Ok(derived) if derived.peer_id == peer_id => {}
                Ok(_) => {
                    tracing::warn!(
                        %peer_id,
                        "ignoring a reach entry whose endpoint derives a different peer"
                    );
                    continue;
                }
                Err(error) => {
                    tracing::warn!(%peer_id, %error, "ignoring an unreadable reach entry");
                    continue;
                }
            }
            decoded.insert(peer_id, endpoint_addr);
        }

        let endpoint = Endpoint::builder(presets::Minimal)
            .secret_key(::iroh::SecretKey::from_bytes(&secret_key_bytes))
            .alpns(vec![PEER_ALPN.to_vec()])
            .bind()
            .await
            .map_err(|error| PeerTransportError::Address(error.to_string()))?;
        let (inbound_tx, inbound_rx) = mpsc::channel(INBOUND_CAPACITY);
        let accept_cancel = CancellationToken::new();
        let accept_task = tokio::spawn(accept_frames(
            endpoint.clone(),
            inbound_tx,
            accept_cancel.clone(),
        ));

        Ok(Self {
            endpoint,
            peer_addresses: Arc::new(decoded),
            connections: Arc::new(RwLock::new(HashMap::new())),
            inbound_rx: Mutex::new(Some(inbound_rx)),
            accept_cancel,
            accept_task: Mutex::new(Some(accept_task)),
        })
    }

    async fn connection(&self, peer: &PeerId) -> Result<Connection, PeerTransportError> {
        {
            let mut connections = self.connections.write().await;
            if let Some(connection) = connections.get(peer) {
                if connection.close_reason().is_none() {
                    return Ok(connection.clone());
                }
            }
            connections.remove(peer);
        }
        self.dial(peer).await?;
        self.connections
            .read()
            .await
            .get(peer)
            .cloned()
            .ok_or_else(|| PeerTransportError::Unreachable(peer.clone()))
    }

    /// Number of cached live connections, exposed for lifecycle conformance.
    pub async fn active_connection_count(&self) -> usize {
        let mut connections = self.connections.write().await;
        connections.retain(|_, connection| connection.close_reason().is_none());
        connections.len()
    }
}

impl Drop for IrohPeerTransport {
    fn drop(&mut self) {
        self.accept_cancel.cancel();
    }
}

#[async_trait]
impl PeerTransport for IrohPeerTransport {
    fn local_address(&self) -> Result<PeerAddress, PeerTransportError> {
        if self.endpoint.is_closed() {
            return Err(PeerTransportError::Closed);
        }
        let bytes = serde_json::to_vec(&self.endpoint.addr())
            .map_err(|error| PeerTransportError::Address(error.to_string()))?;
        PeerAddress::from_bytes(bytes)
    }

    async fn dial(&self, peer: &PeerId) -> Result<(), PeerTransportError> {
        if self.endpoint.is_closed() {
            return Err(PeerTransportError::Closed);
        }
        {
            let mut connections = self.connections.write().await;
            if let Some(connection) = connections.get(peer) {
                if connection.close_reason().is_none() {
                    return Ok(());
                }
            }
            connections.remove(peer);
        }
        let address = self
            .peer_addresses
            .get(peer)
            .cloned()
            .ok_or_else(|| PeerTransportError::Unreachable(peer.clone()))?;
        let connection = self
            .endpoint
            .connect(address, PEER_ALPN)
            .await
            .map_err(|error| {
                if self.endpoint.is_closed() {
                    PeerTransportError::Closed
                } else {
                    PeerTransportError::Dial(error.to_string())
                }
            })?;
        let remote = derive_peer_endpoint_identity(connection.remote_id().as_bytes())?;
        if remote.peer_id != *peer {
            connection.close(0u32.into(), b"peer identity mismatch");
            return Err(PeerTransportError::SignatureInvalid(
                "transport endpoint identifier does not match PeerId".to_owned(),
            ));
        }
        let mut connections = self.connections.write().await;
        if self.endpoint.is_closed() {
            connection.close(0u32.into(), b"transport closed");
            return Err(PeerTransportError::Closed);
        }
        if let Some(existing) = connections.get(peer) {
            if existing.close_reason().is_none() {
                connection.close(0u32.into(), b"duplicate connection");
                return Ok(());
            }
        }
        connections.insert(peer.clone(), connection);
        Ok(())
    }

    async fn send_to(
        &self,
        peer: &PeerId,
        envelope: AgentEnvelope<Value>,
    ) -> Result<FrameVerdict, PeerTransportError> {
        let connection = self.connection(peer).await?;
        let bytes = serde_json::to_vec(&envelope)
            .map_err(|error| PeerTransportError::Send(error.to_string()))?;
        if bytes.len() > MAX_FRAME_BYTES {
            return Err(PeerTransportError::Send(format!(
                "frame exceeds {MAX_FRAME_BYTES} bytes"
            )));
        }
        // A frame is a request, so it travels on a bidirectional stream: the
        // receiver answers on the same authenticated connection, which is what
        // lets a short-lived sender learn the feed position it must chain to.
        let (mut send, mut recv) = connection
            .open_bi()
            .await
            .map_err(|error| PeerTransportError::Send(error.to_string()))?;
        send.write_all(&bytes)
            .await
            .map_err(|error| PeerTransportError::Send(error.to_string()))?;
        send.finish()
            .map_err(|error| PeerTransportError::Send(error.to_string()))?;

        // ⛔ Past this point a failure is NOT a send failure and must never be
        // reported as one: the frame is on the wire and the receiver may well
        // have taken it. An unreadable answer is an unknown outcome, which is
        // the only honest thing to say — and is never acceptance.
        let reply =
            tokio::time::timeout(VERDICT_TIMEOUT, recv.read_to_end(MAX_VERDICT_BYTES)).await;
        Ok(match reply {
            Ok(Ok(reply)) => decode_verdict(&reply).unwrap_or_else(FrameVerdict::unanswered),
            Ok(Err(_)) | Err(_) => FrameVerdict::unanswered(),
        })
    }

    fn inbound(&self) -> Result<mpsc::Receiver<InboundFrame>, PeerTransportError> {
        self.inbound_rx
            .try_lock()
            .map_err(|_| PeerTransportError::InboundUnavailable)?
            .take()
            .ok_or(PeerTransportError::InboundUnavailable)
    }

    async fn shutdown(&self) -> Result<(), PeerTransportError> {
        self.accept_cancel.cancel();
        self.endpoint.close().await;
        if let Some(task) = self.accept_task.lock().await.take() {
            task.await
                .map_err(|error| PeerTransportError::Shutdown(error.to_string()))?;
        }
        self.connections.write().await.clear();
        Ok(())
    }
}

fn decode_address(address: &PeerAddress) -> Result<EndpointAddr, PeerTransportError> {
    serde_json::from_slice(address.as_bytes())
        .map_err(|error| PeerTransportError::Address(error.to_string()))
}

// ── The verdict wire codec ──────────────────────────────────────────────────
//
// One codec, in the one adapter that owns the stream. The domain's
// [`FrameVerdict`] deliberately derives no `Serialize`: a domain-level derive
// would quietly become a second wire contract that nothing keeps in step with
// this one.
//
// Forward compatibility is explicit rather than accidental. The outcome tag and
// the refusal class each carry their own `#[serde(other)]` fallback, and every
// field is `default`, so a newer receiver's answer degrades to "refused, class
// unknown" or "unknown outcome" instead of failing the decode and being reported
// as *no answer at all*. ⛔ A decode failure must never round up to acceptance.

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireOutcome {
    Accepted,
    Refused,
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct WireVerdict {
    outcome: WireOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    refusal: Option<FrameRefusal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_sequence: Option<u64>,
    /// The expected predecessor hash, base64url. Absent and empty are the same
    /// claim — "chain to nothing" — and both decode to an empty `prev_hash`.
    #[serde(skip_serializing_if = "Option::is_none")]
    prev_hash: Option<String>,
}

fn encode_verdict(verdict: &FrameVerdict) -> Vec<u8> {
    let (outcome, refusal) = match verdict.outcome {
        FrameOutcome::Accepted => (WireOutcome::Accepted, None),
        FrameOutcome::Refused(refusal) => (WireOutcome::Refused, Some(refusal)),
        // ⛔ Never written: "no answer" is what an absent reply means, and
        // answering with it would claim the receiver reached a decision it did
        // not reach.
        FrameOutcome::Unanswered => (WireOutcome::Unknown, None),
    };
    let wire = WireVerdict {
        outcome,
        refusal,
        next_sequence: verdict.expected.as_ref().map(|at| at.next_sequence),
        prev_hash: verdict
            .expected
            .as_ref()
            .map(|at| URL_SAFE_NO_PAD.encode(&at.prev_hash)),
    };
    serde_json::to_vec(&wire).unwrap_or_default()
}

fn decode_verdict(bytes: &[u8]) -> Option<FrameVerdict> {
    if bytes.is_empty() {
        return None;
    }
    let wire: WireVerdict = serde_json::from_slice(bytes).ok()?;
    let outcome = match wire.outcome {
        WireOutcome::Accepted => FrameOutcome::Accepted,
        WireOutcome::Refused => {
            FrameOutcome::Refused(wire.refusal.unwrap_or(FrameRefusal::Unclassified))
        }
        WireOutcome::Unknown => FrameOutcome::Unanswered,
    };
    // The position is shape-checked here, at the boundary, so no caller can
    // reach the local signing path with a hostile sequence or a truncated hash.
    let expected = wire.next_sequence.and_then(|next_sequence| {
        let prev_hash = match wire.prev_hash.as_deref() {
            Some(encoded) => URL_SAFE_NO_PAD.decode(encoded).ok()?,
            None => Vec::new(),
        };
        let at = FeedPosition {
            next_sequence,
            prev_hash,
        };
        at.is_wellformed().then_some(at)
    });
    Some(FrameVerdict { outcome, expected })
}

async fn accept_frames(
    endpoint: Endpoint,
    inbound_tx: mpsc::Sender<InboundFrame>,
    accept_cancel: CancellationToken,
) {
    let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_INBOUND_CONNECTIONS));
    loop {
        let incoming = tokio::select! {
            _ = accept_cancel.cancelled() => return,
            incoming = endpoint.accept() => incoming,
        };
        let Some(incoming) = incoming else {
            return;
        };
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            incoming.refuse();
            continue;
        };
        let inbound_tx = inbound_tx.clone();
        let accept_cancel = accept_cancel.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let connection = tokio::select! {
                _ = accept_cancel.cancelled() => return,
                connection = tokio::time::timeout(
                    INBOUND_CONNECTION_IDLE_TIMEOUT,
                    incoming,
                ) => match connection {
                    Ok(Ok(connection)) => connection,
                    Ok(Err(_)) | Err(_) => return,
                },
            };
            let Ok(identity) = derive_peer_endpoint_identity(connection.remote_id().as_bytes())
            else {
                connection.close(0u32.into(), b"invalid peer identity");
                return;
            };

            // One stream at a time, and the verdict is written before the next
            // stream is accepted. That is not a simplification: a sender chains
            // frame N+1 to the position frame N's verdict names, so answering in
            // order is what makes `--count` on one connection meaningful.
            loop {
                let (mut send, mut recv) = tokio::select! {
                    _ = accept_cancel.cancelled() => return,
                    stream = tokio::time::timeout(
                        INBOUND_CONNECTION_IDLE_TIMEOUT,
                        connection.accept_bi(),
                    ) => match stream {
                        Ok(Ok(stream)) => stream,
                        Ok(Err(_)) => return,
                        Err(_) => {
                            connection.close(0u32.into(), b"idle connection");
                            return;
                        }
                    },
                };
                let bytes = tokio::select! {
                    _ = accept_cancel.cancelled() => return,
                    bytes = tokio::time::timeout(
                        INBOUND_CONNECTION_IDLE_TIMEOUT,
                        recv.read_to_end(MAX_FRAME_BYTES),
                    ) => match bytes {
                        Ok(Ok(bytes)) => bytes,
                        Ok(Err(_)) => {
                            connection.close(0u32.into(), b"invalid peer frame");
                            return;
                        }
                        Err(_) => {
                            connection.close(0u32.into(), b"idle connection");
                            return;
                        }
                    },
                };
                let Ok(envelope) = serde_json::from_slice(&bytes) else {
                    connection.close(0u32.into(), b"invalid peer frame");
                    return;
                };
                let (responder, verdict) = FrameResponder::channel();
                if tokio::select! {
                    _ = accept_cancel.cancelled() => return,
                    sent = inbound_tx.send(InboundFrame {
                        envelope,
                        peer_id: identity.peer_id.clone(),
                        responder: Some(responder),
                    }) => sent,
                }
                .is_err()
                {
                    return;
                }
                // A local consumer that never answers must not hold this
                // connection open forever; the sender then reads an empty reply
                // and reports an unknown outcome.
                let answered = tokio::select! {
                    _ = accept_cancel.cancelled() => return,
                    answered = tokio::time::timeout(VERDICT_TIMEOUT, verdict) => match answered {
                        Ok(Ok(verdict)) => Some(verdict),
                        Ok(Err(_)) | Err(_) => None,
                    },
                };
                if let Some(verdict) = answered {
                    let _ = send.write_all(&encode_verdict(&verdict)).await;
                }
                let _ = send.finish();
            }
        });
    }
}
