mod ingress;

pub use ingress::{IrohPeerIngress, PeerIngressError};

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use ::iroh::endpoint::{Connection, presets};
use ::iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode as IrohRelayMode, TransportAddr};
use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{Mutex, RwLock, Semaphore, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::domain::models::{
    AgentEnvelope, FeedPosition, FrameOutcome, FrameRefusal, FrameReply, FrameVerdict,
    PathObservation, PeerId, RelayMode, RelaySet,
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
/// # The composition, and why it starts where it does
///
/// Every endpoint is built from [`presets::Minimal`], which sets **only** the
/// rustls crypto provider — no relay and, decisively, **no address lookup**.
/// The relay is then layered on with `.relay_mode(…)` according to the
/// operator's [`RelayMode`]: `Disabled` adds nothing, `N0Default` adds the n0
/// list, `Configured` adds exactly the relays they named.
///
/// ⛔ Never `presets::N0`, and ⛔ never `presets::N0DisableRelay`: the latter is
/// `N0.apply(builder).relay_mode(Disabled)` — the **full** N0 preset, including
/// all three n0 address-lookup services, with the relay switched off
/// afterwards. Zero-phone-home has two halves, and starting from `Minimal` is
/// what closes the second one for free. ⛔ Never call `.address_lookup(…)`
/// either; that reopens it.
///
/// # What a relay observes (FR159-a)
///
/// A relay is a forward-only conduit for an end-to-end-encrypted session: it
/// carries packets and therefore observes the **social graph** — which endpoint
/// exchanged traffic with which, when, and how much. ⛔ No string in this tree
/// may claim it *cannot* read anything: this cut tests no confidentiality
/// property and may claim none.
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
    /// signed peer envelopes, composing the operator's relay mode onto
    /// [`presets::Minimal`].
    ///
    /// ⚠ `relay` reaches **both** production binds — the daemon listener and
    /// the `peer ping` client. A mode honoured in one and not the other is a
    /// host whose ping takes a path its own listener would not.
    pub async fn bind(
        secret_key_bytes: [u8; 32],
        peer_addresses: HashMap<PeerId, PeerAddress>,
        relay: &RelayMode,
    ) -> Result<Self, PeerTransportError> {
        Self::compose(secret_key_bytes, peer_addresses, relay, true).await
    }

    /// Bind with the direct (IP) transport removed, so the only way out is the
    /// relay the operator configured.
    ///
    /// ⚠ **Test builds only.** The gate is `p2p-test-utils` **and**
    /// `debug_assertions`, so no binary a release profile could ship — not even
    /// an `--all-features` one — has a path to this call (Story 18.4c review).
    ///
    /// It exists because *"with the direct path disabled (relay-only)"* is
    /// NFR72(a)'s literal wording, and on a single host loopback hole-punching
    /// otherwise wins inside the first round trip: measured, the same exchange
    /// observed `Relayed` once and `Direct` once. Asserting a relayed path
    /// without removing the direct transport would therefore be asserting a
    /// **race**, which is exactly what a deterministic control has to replace.
    ///
    /// ⛔ It is not a second composition. It enters the same [`Self::compose`],
    /// with the same preset and the same relay mode, and differs by one builder
    /// call — so it cannot drift into a double more forgiving than production.
    #[cfg(all(feature = "p2p-test-utils", debug_assertions))]
    pub async fn bind_without_direct_paths(
        secret_key_bytes: [u8; 32],
        peer_addresses: HashMap<PeerId, PeerAddress>,
        relay: &RelayMode,
    ) -> Result<Self, PeerTransportError> {
        Self::compose(secret_key_bytes, peer_addresses, relay, false).await
    }

    /// The one endpoint composition. `direct_paths` is `true` on every
    /// production path; only the test-gated entry above passes `false`.
    async fn compose(
        secret_key_bytes: [u8; 32],
        peer_addresses: HashMap<PeerId, PeerAddress>,
        relay: &RelayMode,
        direct_paths: bool,
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

        let mut builder = Endpoint::builder(presets::Minimal)
            .secret_key(::iroh::SecretKey::from_bytes(&secret_key_bytes))
            .alpns(vec![PEER_ALPN.to_vec()]);
        builder = match relay {
            // The shipped composition, byte-identical: adding nothing is what
            // `disabled` means, and it is what an install with no `relay.json`
            // has always had.
            RelayMode::Disabled => builder,
            RelayMode::N0Default => builder.relay_mode(IrohRelayMode::Default),
            RelayMode::Configured { urls } => {
                let parsed = parse_relay_urls(urls)?;
                builder.relay_mode(IrohRelayMode::custom(parsed))
            }
        };
        if !direct_paths {
            // ⚠ Reachable only from the test-gated entry: every production
            // caller passes `true`. Removing the IP transport is what makes
            // *"the direct path disabled"* a fact rather than a hope.
            builder = builder.clear_ip_transports();
        }
        // ⚠ TEST BUILDS ONLY, and it is compiled out of every binary a profile
        // could ever ship: the gate is `p2p-test-utils` **and**
        // `debug_assertions`, so a `--all-features` release build does not
        // carry it either — a non-default feature alone is a convention, and
        // conventions drift (Story 18.4c review). The hermetic relay fixture
        // serves a self-signed certificate, and without this the handshake
        // fails in a way that reads exactly like a relay bug.
        #[cfg(all(feature = "p2p-test-utils", debug_assertions))]
        let builder = builder.ca_tls_config(::iroh::tls::CaTlsConfig::insecure_skip_verify());
        let endpoint = builder
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

    /// Run `sink` whenever this endpoint has a **publishable** own address —
    /// now, and each time it or its relay session changes (Story 18.4c, AC3).
    ///
    /// # Why a watcher and ⛔ never `Endpoint::online()`
    ///
    /// `Endpoint::addr()` returns whatever is known *now*, and its own doc
    /// directs callers to await `online()` first — but `online()` **pends
    /// forever when no relay is configured**, which is exactly the `disabled`
    /// host. So the bind-time publish stays as it is (a relay-less record on a
    /// relay-enabled host is the honest fact at that instant), and this loop
    /// corrects it the moment a home relay is actually established.
    ///
    /// # The current value is emitted first (Story 18.4c review)
    ///
    /// ⚑ `updated()` completes only on a value **newer** than the watcher's
    /// creation snapshot. A relay established between the bind-time publish and
    /// this watcher's creation is therefore the *initial* value and would never
    /// be reported — leaving the self reach record and every later ticket
    /// relay-less until the next network change. Emitting the current value
    /// first closes that window, and `publish_self_reach_on_change` already
    /// suppresses the unchanged case.
    ///
    /// # An address naming a relay is published only once its session is up
    ///
    /// ⛔ iroh publishes a relay into the address the moment it is **selected**
    /// — `RelayConnectionState::Connecting`, before the dial — and the address
    /// watcher carries no state. Publishing that would advertise (and mint into
    /// tickets) a relay this host has no session with (Story 18.4c review,
    /// owner ruling). So a relay-bearing address is sunk only when
    /// [`Endpoint::home_relay_status`] reports that relay `Connected`; a
    /// relay-less address is always publishable.
    ///
    /// ⛔ No sleep, ⛔ no unbounded await, ⛔ no polling: it returns when the
    /// token is cancelled or the endpoint's last clone is dropped.
    pub async fn republish_address_on_change(
        &self,
        cancel: CancellationToken,
        mut sink: impl FnMut(PeerAddress),
    ) {
        use ::iroh::Watcher as _;

        let mut addresses = self.endpoint.watch_addr();
        let mut relay_status = self.endpoint.home_relay_status();
        let mut current = addresses.get();
        loop {
            let ready = relay_sessions_ready(&current, &relay_status.get());
            if ready {
                match serde_json::to_vec(&current) {
                    Ok(bytes) => match PeerAddress::from_bytes(bytes) {
                        Ok(address) => sink(address),
                        Err(error) => {
                            tracing::warn!(%error, "this host's changed address did not encode");
                        }
                    },
                    Err(error) => {
                        tracing::warn!(%error, "this host's changed address did not encode");
                    }
                }
            }
            tokio::select! {
                () = cancel.cancelled() => return,
                updated = addresses.updated() => {
                    let Ok(addr) = updated else {
                        // The last `Endpoint` clone is gone; there is nothing
                        // left to observe and nothing to report.
                        return;
                    };
                    current = addr;
                }
                changed = relay_status.updated() => {
                    // The address did not move; the relay's *session* did.
                    // Re-evaluate readiness against the fresh status — this is
                    // the arm that publishes a selected relay once it connects.
                    if changed.is_err() {
                        return;
                    }
                }
            }
        }
    }

    /// The home relays this endpoint **holds sessions with**, as `(url,
    /// connected)` (Story 18.4c, AC1's positive control).
    ///
    /// ⚠ Test builds only: the gate is `p2p-test-utils` **and**
    /// `debug_assertions`, so no release profile can carry it. This is the
    /// observation AC1 named — the *composed endpoint's* own report of which
    /// relay it uses, ⛔ not a pre-composition domain value a mutant beside the
    /// composition could satisfy.
    #[cfg(all(feature = "p2p-test-utils", debug_assertions))]
    #[must_use]
    pub fn home_relay_sessions(&self) -> Vec<(String, bool)> {
        use ::iroh::Watcher as _;

        self.endpoint
            .home_relay_status()
            .get()
            .into_iter()
            .map(|status| (status.url().to_string(), status.is_connected()))
            .collect()
    }

    /// Await the first home relay this endpoint **holds a session with**, and
    /// return its URL (Story 18.4c, AC1's positive control).
    ///
    /// ⚠ Test builds only, same gate as [`Self::home_relay_sessions`]. Event-
    /// driven ⛔ not polled: the current value is checked first (a relay
    /// connected before this watcher existed must not be missed), then each
    /// subsequent status change.
    #[cfg(all(feature = "p2p-test-utils", debug_assertions))]
    pub async fn await_connected_home_relay(&self) -> String {
        use ::iroh::Watcher as _;

        let mut watcher = self.endpoint.home_relay_status();
        loop {
            if let Some(status) = watcher.get().iter().find(|status| status.is_connected()) {
                return status.url().to_string();
            }
            if watcher.updated().await.is_err() {
                // The last `Endpoint` clone is gone; there is nothing to await.
                return String::new();
            }
        }
    }

    /// The endpoint's net-report and portmap counters, as `(reports,
    /// portmap_attempts)` (Story 18.4c, test gate 15).
    ///
    /// ⚠ Compiled **only** into the test build. `p2p-test-utils` is a
    /// non-default cargo key, so the shipped binary gains no accessor and no
    /// caller — an observability surface whose only caller is a test is the
    /// mechanism-without-a-trigger class this epic keeps paying for, and
    /// keeping it out of the release build is how this one avoids joining it.
    ///
    /// Under [`presets::Minimal`] both must stay at zero whatever the
    /// destination, which is the one **host-blind-proof** leg of the
    /// zero-phone-home claim: the counters name no host, so a claim about
    /// *which* host was contacted cannot rest on them alone.
    #[cfg(feature = "p2p-test-utils")]
    #[must_use]
    pub fn net_report_counters(&self) -> (u64, u64) {
        let metrics = self.endpoint.metrics();
        (
            metrics.net_report.reports.get(),
            metrics.net_report.portmap_attempts.get(),
        )
    }

    /// Bytes this endpoint has sent to **a** relay — ⛔ not to a named one.
    ///
    /// Useful only as a positive control: paired with a structurally
    /// single-entry relay map, "bytes to a relay" can mean bytes to just one
    /// relay. ⚠ With `iroh-metrics/metrics` off this returns a hardcoded `0`
    /// rather than failing to compile, which is exactly why the assertion that
    /// uses it is a `> 0` control.
    #[cfg(feature = "p2p-test-utils")]
    #[must_use]
    pub fn relay_bytes_sent(&self) -> u64 {
        self.endpoint.metrics().socket.send_relay.get()
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

        // ⚑ The path is read HERE — immediately after the write, while it still
        // describes the connection that carried **this frame's bytes** — and it
        // is minted **with** the verdict rather than fetched by an accessor
        // afterwards (Story 18.4c review). Reading it after the reply would
        // misattribute: iroh holepunches *after* connecting, so the connection
        // can migrate relay→direct while the answer is in flight, and the first
        // frame — the one most likely still on the relay — would be reported
        // under the path its answer arrived on. One read, at the moment the
        // frame travelled: still no second read of a moving thing, and an
        // unanswered frame still drops the path entirely (`unanswered()` takes
        // none), so a receiver that drops its responder can never get
        // `Carried directly.` printed beneath *"the peer did not answer"*.
        let path = observe_path(&connection);

        // ⛔ Past this point a failure is NOT a send failure and must never be
        // reported as one: the frame is on the wire and the receiver may well
        // have taken it. An unreadable answer is an unknown outcome, which is
        // the only honest thing to say — and is never acceptance.
        let reply =
            tokio::time::timeout(VERDICT_TIMEOUT, recv.read_to_end(MAX_VERDICT_BYTES)).await;
        Ok(match reply {
            Ok(Ok(reply)) => decode_verdict(&reply, path).unwrap_or_else(FrameVerdict::unanswered),
            // ⛔ No path here, and there is no way to attach one: `unanswered`
            // takes none.
            Ok(Err(_)) | Err(_) => FrameVerdict::unanswered(),
        })
    }

    /// Write one topic advertisement and return only the receiver's expected
    /// feed position (Story 18.4a).
    ///
    /// # Why this overrides the default rather than inheriting it
    ///
    /// The port's default is `Err(Unsupported)` on purpose — a defaulted method
    /// that silently succeeds is a mechanism whose absence is
    /// indistinguishable from its presence. This is the one real transport, so
    /// it is the one implementation that must opt in.
    ///
    /// # Why it reuses the same bidirectional stream
    ///
    /// The receiver chains **every** frame from one sender by `prev_hash`,
    /// gossip included, so a sender that could not learn the position it must
    /// chain to would fork the feed on the first restart of either side —
    /// 18.4d's D9, exactly. The reply is read for that one fact and for nothing
    /// else: ⛔ no outcome, no path, no acceptance is returned to the caller,
    /// because an advertisement has none to give.
    async fn gossip_topic(
        &self,
        peer: &PeerId,
        envelope: AgentEnvelope<Value>,
    ) -> Result<Option<crate::domain::models::FeedPosition>, PeerTransportError> {
        let connection = self.connection(peer).await?;
        let bytes = serde_json::to_vec(&envelope)
            .map_err(|error| PeerTransportError::Send(error.to_string()))?;
        if bytes.len() > MAX_FRAME_BYTES {
            return Err(PeerTransportError::Send(format!(
                "frame exceeds {MAX_FRAME_BYTES} bytes"
            )));
        }
        let (mut send, mut recv) = connection
            .open_bi()
            .await
            .map_err(|error| PeerTransportError::Send(error.to_string()))?;
        send.write_all(&bytes)
            .await
            .map_err(|error| PeerTransportError::Send(error.to_string()))?;
        send.finish()
            .map_err(|error| PeerTransportError::Send(error.to_string()))?;

        // ⛔ Past this point a failure is not a send failure: the frame is on
        // the wire. An unreadable answer simply means no guidance was offered.
        let reply =
            tokio::time::timeout(VERDICT_TIMEOUT, recv.read_to_end(MAX_VERDICT_BYTES)).await;
        Ok(match reply {
            // ⛔ The path is discarded: a fire-and-forget advertisement makes
            // no path claim, so the placeholder is the never-direct arm and
            // nothing reads it.
            Ok(Ok(reply)) => decode_verdict(&reply, PathObservation::Other)
                .and_then(|verdict| verdict.expected.clone()),
            Ok(Err(_)) | Err(_) => None,
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

// ── The relay composition ───────────────────────────────────────────────────

/// Parse the operator's canonical relay URLs into iroh's own type.
///
/// ⚠ Both sides of the membership rule go through the **same** WHATWG parser:
/// `RelayUrl` is `Arc<url::Url>` whose `FromStr` delegates to `Url::from_str`,
/// and the canonical text this receives came out of that same parser. So
/// "configured" and "offered" are compared as one relation, not two.
fn parse_relay_urls(urls: &[String]) -> Result<Vec<::iroh::RelayUrl>, PeerTransportError> {
    urls.iter()
        .map(|url| {
            url.parse::<::iroh::RelayUrl>().map_err(|error| {
                PeerTransportError::Address(format!("relay URL {url:?} did not parse: {error}"))
            })
        })
        .collect()
}

/// The relay hosts the n0 default mode resolves to.
///
/// Resolved through iroh's own `RelayMode::relay_map()` rather than copied, so
/// a list that moves upstream moves here too — the membership rule must be
/// checked against the relays this endpoint would actually dial, ⛔ never
/// against a snapshot that has drifted from them.
#[must_use]
pub fn n0_default_relay_set() -> RelaySet {
    IrohRelayMode::Default
        .relay_map()
        .urls::<Vec<::iroh::RelayUrl>>()
        .into_iter()
        .map(|url| url.as_str().to_owned())
        .collect()
}

/// Whether an address may be published as this host's reach: every relay it
/// names must have a **connected session** (Story 18.4c review, owner ruling).
///
/// iroh publishes a relay into the endpoint address the moment the relay is
/// *selected* — `RelayConnectionState::Connecting`, before the dial — and the
/// address watcher carries no connection state. Sinking such an address would
/// advertise (and mint into freshly signed tickets) a relay this host has no
/// session with. A relay-less address names only direct sockets, which need no
/// session to be a fact.
fn relay_sessions_ready(address: &EndpointAddr, status: &[::iroh::endpoint::RelayStatus]) -> bool {
    address.addrs.iter().all(|addr| match addr {
        TransportAddr::Relay(url) => status
            .iter()
            .any(|status| status.url() == url && status.is_connected()),
        // Direct sockets and any transport a newer iroh adds name no relay,
        // so no session is required for them to be a publishable fact.
        _ => true,
    })
}

/// How the frame that just got an answer actually travelled.
///
/// ⚠ `for path in connection.paths()` does **not** compile — `IntoIterator` is
/// implemented on `&PathList`, not `PathList` — so the snapshot is bound first.
/// And the match is on `remote_addr()` rather than on `is_ip()`/`is_relay()`:
/// those are two bools with no `is_custom()` companion, so a custom-transport
/// path answers **false to both** and code that assumes `!is_relay() ⇒ direct`
/// renders it as direct. Matching the address makes the unknown case its own
/// arm — and hands over the relay host the disclosure has to name.
fn observe_path(connection: &Connection) -> PathObservation {
    let paths = connection.paths();
    let Some(selected) = (&paths)
        .into_iter()
        .find(::iroh::endpoint::Path::is_selected)
    else {
        return PathObservation::Other;
    };
    match selected.remote_addr() {
        TransportAddr::Ip(_) => PathObservation::Direct,
        TransportAddr::Relay(url) => PathObservation::Relayed {
            host: url.to_string(),
        },
        _ => PathObservation::Other,
    }
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

fn encode_verdict(reply: &FrameReply) -> Vec<u8> {
    let (outcome, refusal) = match reply.outcome {
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
        next_sequence: reply.expected.as_ref().map(|at| at.next_sequence),
        prev_hash: reply
            .expected
            .as_ref()
            .map(|at| URL_SAFE_NO_PAD.encode(&at.prev_hash)),
    };
    serde_json::to_vec(&wire).unwrap_or_default()
}

/// Decode one answer, and attach the path this host watched the frame take.
///
/// ⚑ `path` is consumed by the two answered arms and **dropped** on the unknown
/// arm. There is no way to keep it there: [`FrameVerdict::unanswered`] takes no
/// argument, which is why the mutant *"print a path on `Unanswered`"* is not a
/// rule to remember but a line that does not compile.
fn decode_verdict(bytes: &[u8], path: PathObservation) -> Option<FrameVerdict> {
    if bytes.is_empty() {
        return None;
    }
    let wire: WireVerdict = serde_json::from_slice(bytes).ok()?;
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
    let verdict = match wire.outcome {
        WireOutcome::Accepted => FrameVerdict::accepted(path),
        WireOutcome::Refused => {
            FrameVerdict::refused(wire.refusal.unwrap_or(FrameRefusal::Unclassified), path)
        }
        WireOutcome::Unknown => FrameVerdict::unanswered(),
    };
    Some(match expected {
        Some(expected) => verdict.with_expected(expected),
        None => verdict,
    })
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
