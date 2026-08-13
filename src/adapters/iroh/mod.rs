mod ingress;

pub use ingress::{IrohPeerIngress, PeerIngressError};

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use ::iroh::endpoint::{Connection, presets};
use ::iroh::{Endpoint, EndpointAddr, EndpointId};
use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::{Mutex, RwLock, Semaphore, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::domain::models::{AgentEnvelope, PeerId};
use crate::domain::ports::{InboundFrame, PeerAddress, PeerTransport, PeerTransportError};

const PEER_ALPN: &[u8] = b"rustain/peer/1";
const INBOUND_CAPACITY: usize = 128;
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

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
            let endpoint_addr = decode_address(&address)?;
            let derived = derive_peer_endpoint_identity(endpoint_addr.id.as_bytes())?;
            if derived.peer_id != peer_id {
                return Err(PeerTransportError::Address(format!(
                    "endpoint identifier does not derive configured peer {peer_id}"
                )));
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
    ) -> Result<(), PeerTransportError> {
        let connection = self.connection(peer).await?;
        let bytes = serde_json::to_vec(&envelope)
            .map_err(|error| PeerTransportError::Send(error.to_string()))?;
        if bytes.len() > MAX_FRAME_BYTES {
            return Err(PeerTransportError::Send(format!(
                "frame exceeds {MAX_FRAME_BYTES} bytes"
            )));
        }
        let mut stream = connection
            .open_uni()
            .await
            .map_err(|error| PeerTransportError::Send(error.to_string()))?;
        stream
            .write_all(&bytes)
            .await
            .map_err(|error| PeerTransportError::Send(error.to_string()))?;
        stream
            .finish()
            .map_err(|error| PeerTransportError::Send(error.to_string()))?;
        Ok(())
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

            loop {
                let stream = tokio::select! {
                    _ = accept_cancel.cancelled() => return,
                    stream = tokio::time::timeout(
                        INBOUND_CONNECTION_IDLE_TIMEOUT,
                        connection.accept_uni(),
                    ) => match stream {
                        Ok(Ok(stream)) => stream,
                        Ok(Err(_)) => return,
                        Err(_) => {
                            connection.close(0u32.into(), b"idle connection");
                            return;
                        }
                    },
                };
                let mut stream = stream;
                let bytes = tokio::select! {
                    _ = accept_cancel.cancelled() => return,
                    bytes = tokio::time::timeout(
                        INBOUND_CONNECTION_IDLE_TIMEOUT,
                        stream.read_to_end(MAX_FRAME_BYTES),
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
                if tokio::select! {
                    _ = accept_cancel.cancelled() => return,
                    sent = inbound_tx.send(InboundFrame {
                        envelope,
                        peer_id: identity.peer_id.clone(),
                    }) => sent,
                }
                .is_err()
                {
                    return;
                }
            }
        });
    }
}
