#![cfg(feature = "p2p")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use arc_swap::ArcSwap;
use async_trait::async_trait;
use base64::Engine as _;
use rustain::adapters::iroh::{IrohPeerIngress, IrohPeerTransport, PeerIngressError};
use rustain::adapters::p2p_config::p2p_listener_requested;
use rustain::adapters::rap::{
    AgentSigner, VerifiedPeerConsent, VerifiedPeerConsumer, VerifiedPeerFrameHandler, entry_hash,
};
use rustain::domain::models::{
    AgentEnvelope, AgentId, AgentMessage, CorrelationId, Ed25519Sig, MessageKind, PeerId,
};
use rustain::domain::ports::{
    AgentMessageBus, PeerDeliveryRecord, PeerInteractionRecorder, PeerTransport,
    PeerTransportError, RelationshipDeliveryPolicy,
};
use rustain::infrastructure::paths::workspace_p2p_config_path;
use rustain::infrastructure::subagent::{LocalMessageBus, NodeTree};
use tokio::sync::{Mutex, mpsc};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn source(relative: &str) -> String {
    std::fs::read_to_string(root().join(relative))
        .unwrap_or_else(|error| panic!("read {relative}: {error}"))
}

fn rust_string_literals(source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let mut strings = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index..].starts_with(b"//") {
            index += 2;
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        if bytes[index..].starts_with(b"/*") {
            index += 2;
            let mut depth = 1;
            while index < bytes.len() && depth > 0 {
                if bytes[index..].starts_with(b"/*") {
                    depth += 1;
                    index += 2;
                } else if bytes[index..].starts_with(b"*/") {
                    depth -= 1;
                    index += 2;
                } else {
                    index += 1;
                }
            }
            continue;
        }

        let raw_start = match bytes[index] {
            b'r' => Some(index + 1),
            b'b' if bytes.get(index + 1) == Some(&b'r') => Some(index + 2),
            _ => None,
        };
        if let Some(mut cursor) = raw_start {
            let mut hashes = 0;
            while bytes.get(cursor) == Some(&b'#') {
                hashes += 1;
                cursor += 1;
            }
            if bytes.get(cursor) == Some(&b'"') {
                let content_start = cursor + 1;
                cursor = content_start;
                while cursor < bytes.len() {
                    if bytes[cursor] == b'"'
                        && bytes
                            .get(cursor + 1..cursor + 1 + hashes)
                            .is_some_and(|suffix| suffix.iter().all(|byte| *byte == b'#'))
                    {
                        strings.push(source[content_start..cursor].to_owned());
                        index = cursor + 1 + hashes;
                        break;
                    }
                    cursor += 1;
                }
                if index > content_start {
                    continue;
                }
            }
        }

        let quote = if bytes[index] == b'"' {
            Some(index)
        } else if bytes[index] == b'b' && bytes.get(index + 1) == Some(&b'"') {
            Some(index + 1)
        } else {
            None
        };
        if let Some(quote) = quote {
            let content_start = quote + 1;
            let mut cursor = content_start;
            while cursor < bytes.len() {
                match bytes[cursor] {
                    b'\\' => cursor += 2,
                    b'"' => {
                        strings.push(source[content_start..cursor].to_owned());
                        index = cursor + 1;
                        break;
                    }
                    _ => cursor += 1,
                }
            }
            if index > content_start {
                continue;
            }
        }
        index += 1;
    }
    strings
}

fn function_source<'a>(source: &'a str, signature: &str) -> &'a str {
    let body = &source[source
        .find(signature)
        .unwrap_or_else(|| panic!("positive control: missing {signature}"))..];
    &body[..body
        .find("\n}\n")
        .unwrap_or_else(|| panic!("positive control: {signature} does not close"))]
}

fn signing_key(seed: u8) -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
}

fn write_allowlist(workspace: &Path, seed: Option<u8>) {
    write_allowlist_pins(workspace, seed.as_slice());
}

fn write_allowlist_pins(workspace: &Path, seeds: &[u8]) {
    let rustain = workspace.join(".rustain");
    std::fs::create_dir_all(&rustain).expect("create config dir");
    let entries = seeds
        .iter()
        .map(|seed| {
            let x = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(signing_key(*seed).verifying_key().to_bytes());
            format!(r#""client-{seed}":{{"pinnedKey":{{"alg":"EdDSA","x":"{x}"}}}}"#)
        })
        .collect::<Vec<_>>()
        .join(",");
    std::fs::write(
        rustain.join("p2p.json"),
        format!(r#"{{"listen":true,"agents":{{{entries}}}}}"#),
    )
    .expect("write allowlist");
}

fn signed_envelope(
    signer: &AgentSigner,
    sequence: u64,
    prev_hash: Vec<u8>,
    correlation: &str,
    body: &str,
) -> AgentEnvelope<serde_json::Value> {
    signed_envelope_to(
        signer,
        "local-recipient",
        sequence,
        prev_hash,
        correlation,
        body,
    )
}

/// Address a specific local recipient. Two peers writing to the SAME recipient
/// are serialised by that recipient's own mailbox, so a test about transport
/// isolation has to give them separate recipients or it measures the wrong seam.
fn signed_envelope_to(
    signer: &AgentSigner,
    recipient: &str,
    sequence: u64,
    prev_hash: Vec<u8>,
    correlation: &str,
    body: &str,
) -> AgentEnvelope<serde_json::Value> {
    let sender = AgentId::from_peer_path(&format!(
        "{}/peer-transport",
        signer.identity().peer_id.as_str()
    ))
    .expect("peer-rooted sender");
    signer
        .sign(
            sender,
            AgentId::parse(recipient).expect("recipient"),
            CorrelationId::new(correlation),
            MessageKind::PeerMessage,
            sequence,
            2_000,
            format!("nonce-{sequence}"),
            prev_hash,
            serde_json::json!(body),
        )
        .expect("sign envelope")
}

impl Default for RecordingConsumer {
    fn default() -> Self {
        Self {
            bodies: Mutex::new(Vec::new()),
            failures_remaining: AtomicUsize::new(0),
            taken: AtomicUsize::new(0),
            stalled_peer: Mutex::new(None),
            gate: tokio::sync::Semaphore::new(0),
        }
    }
}

struct RecordingConsumer {
    bodies: Mutex<Vec<String>>,
    failures_remaining: AtomicUsize,
    taken: AtomicUsize,
    /// Frames from this peer stall inside `ingest` until `release` is called —
    /// the shape of a recipient that has taken a message and not finished.
    stalled_peer: Mutex<Option<PeerId>>,
    gate: tokio::sync::Semaphore,
}

impl RecordingConsumer {
    fn fail_next(&self) {
        self.failures_remaining.store(1, Ordering::SeqCst);
    }

    async fn stall(&self, peer: &PeerId) {
        *self.stalled_peer.lock().await = Some(peer.clone());
    }

    /// Let the stalled recipient finish and stop stalling its later frames.
    async fn release(&self) {
        *self.stalled_peer.lock().await = None;
        self.gate.add_permits(1);
    }

    /// Messages the recipient has taken, whether or not it finished with them.
    fn taken(&self) -> usize {
        self.taken.load(Ordering::SeqCst)
    }

    async fn bodies(&self) -> Vec<String> {
        self.bodies.lock().await.clone()
    }
}

#[async_trait]
impl VerifiedPeerConsumer for RecordingConsumer {
    async fn consent(
        &self,
        _recipient: &AgentId,
        _content: &AgentMessage,
        _peer_id: &PeerId,
    ) -> Result<VerifiedPeerConsent, String> {
        Ok(VerifiedPeerConsent::Accept)
    }

    async fn ingest(
        &self,
        _recipient: &AgentId,
        content: AgentMessage,
        peer_id: &PeerId,
    ) -> Result<(), String> {
        self.taken.fetch_add(1, Ordering::SeqCst);
        if self
            .failures_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Err("injected downstream refusal".to_owned());
        }
        let stalled = self.stalled_peer.lock().await.clone();
        if stalled.as_ref() == Some(peer_id) {
            self.gate.acquire().await.expect("gate stays open").forget();
        }
        self.bodies.lock().await.push(content.content);
        Ok(())
    }
}

#[derive(Default)]
struct RecordingRecorder(Mutex<Vec<PeerDeliveryRecord>>);

#[async_trait]
impl PeerInteractionRecorder for RecordingRecorder {
    async fn record_peer_delivery(&self, record: PeerDeliveryRecord) -> Result<(), String> {
        self.0.lock().await.push(record);
        Ok(())
    }
}

fn front_door_fixture() -> (
    Arc<VerifiedPeerFrameHandler>,
    Arc<RecordingConsumer>,
    mpsc::UnboundedReceiver<rustain::domain::events::AppEvent>,
) {
    let (domain_tx, domain_rx) = mpsc::unbounded_channel();
    let node_tree = NodeTree::new();
    let bus = Arc::new(LocalMessageBus::new(
        node_tree.clone(),
        Arc::new(RelationshipDeliveryPolicy),
    )) as Arc<dyn AgentMessageBus>;
    let bus_slot = Arc::new(ArcSwap::from_pointee(bus));
    let consumer = Arc::new(RecordingConsumer::default());
    let handler = Arc::new(VerifiedPeerFrameHandler::new(
        node_tree,
        bus_slot,
        domain_tx,
        consumer.clone(),
        Arc::new(RecordingRecorder::default()),
    ));
    (handler, consumer, domain_rx)
}

async fn endpoint_fixture(
    workspace: &Path,
) -> (
    Arc<IrohPeerTransport>,
    Arc<IrohPeerTransport>,
    IrohPeerIngress,
    Arc<RecordingConsumer>,
    mpsc::UnboundedReceiver<rustain::domain::events::AppEvent>,
    PeerId,
) {
    write_allowlist(workspace, Some(23));
    let server = Arc::new(
        IrohPeerTransport::bind(signing_key(17).to_bytes(), HashMap::new())
            .await
            .expect("bind server"),
    );
    let server_identity = rustain::adapters::iroh::derive_peer_endpoint_identity(
        &signing_key(17).verifying_key().to_bytes(),
    )
    .expect("server identity");
    let client = Arc::new(
        IrohPeerTransport::bind(
            signing_key(23).to_bytes(),
            HashMap::from([(
                server_identity.peer_id.clone(),
                server.local_address().expect("server address"),
            )]),
        )
        .await
        .expect("bind client"),
    );
    client
        .dial(&server_identity.peer_id)
        .await
        .expect("dial server");
    let (handler, consumer, domain_rx) = front_door_fixture();
    let ingress =
        IrohPeerIngress::with_now(server.clone(), handler, workspace.to_path_buf(), || 1_000)
            .expect("compose ingress");
    (
        server,
        client,
        ingress,
        consumer,
        domain_rx,
        server_identity.peer_id,
    )
}

#[tokio::test]
async fn two_endpoint_keystone_verifies_before_reaching_the_front_door() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (server, client, ingress, consumer, _domain_rx, server_id) =
        endpoint_fixture(tmp.path()).await;
    let signer = AgentSigner::from_signing_key(signing_key(23));
    let first = signed_envelope(&signer, 1, Vec::new(), "corr-1", "accepted");

    client
        .send_to(&server_id, first.clone())
        .await
        .expect("send valid frame");
    assert_eq!(ingress.accept_next().await.expect("accept frame"), 1);
    assert_eq!(consumer.bodies.lock().await.as_slice(), &["accepted"]);

    let mut tampered = signed_envelope(
        &signer,
        2,
        entry_hash(&first.header).expect("first hash"),
        "corr-2",
        "must not arrive",
    );
    let mut signature = tampered.signature.as_bytes().to_vec();
    signature[0] ^= 0x01;
    tampered.signature = Ed25519Sig(signature);
    client
        .send_to(&server_id, tampered)
        .await
        .expect("transport carries untrusted frame");
    assert!(matches!(
        ingress.accept_next().await,
        Err(PeerIngressError::Transport(
            PeerTransportError::SignatureInvalid(_)
        ))
    ));
    assert_eq!(consumer.bodies.lock().await.as_slice(), &["accepted"]);

    client.shutdown().await.expect("shutdown client");
    server.shutdown().await.expect("shutdown server");
}

#[tokio::test]
async fn replay_position_commits_only_after_downstream_acceptance() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (server, client, ingress, consumer, _domain_rx, server_id) =
        endpoint_fixture(tmp.path()).await;
    consumer.fail_next();
    let signer = AgentSigner::from_signing_key(signing_key(23));
    let envelope = signed_envelope(&signer, 1, Vec::new(), "retryable", "retry me");

    client
        .send_to(&server_id, envelope.clone())
        .await
        .expect("send first attempt");
    assert!(matches!(
        ingress.accept_next().await,
        Err(PeerIngressError::Delivery(_))
    ));
    client
        .send_to(&server_id, envelope)
        .await
        .expect("retry same frame");
    assert_eq!(ingress.accept_next().await.expect("retry accepted"), 1);
    assert_eq!(consumer.bodies.lock().await.as_slice(), &["retry me"]);

    client.shutdown().await.expect("shutdown client");
    server.shutdown().await.expect("shutdown server");
}

#[tokio::test]
async fn allowlist_removal_refuses_the_next_frame_on_the_same_open_connection() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (server, client, ingress, consumer, _domain_rx, server_id) =
        endpoint_fixture(tmp.path()).await;
    let signer = AgentSigner::from_signing_key(signing_key(23));
    let first = signed_envelope(&signer, 1, Vec::new(), "before-removal", "before");
    client
        .send_to(&server_id, first.clone())
        .await
        .expect("send before removal");
    ingress
        .accept_next()
        .await
        .expect("accepted before removal");
    assert_eq!(client.active_connection_count().await, 1);

    write_allowlist(tmp.path(), None);
    let second = signed_envelope(
        &signer,
        2,
        entry_hash(&first.header).expect("first hash"),
        "after-removal",
        "after",
    );
    client
        .send_to(&server_id, second.clone())
        .await
        .expect("same connection remains writable");
    assert!(matches!(
        ingress.accept_next().await,
        Err(PeerIngressError::Transport(
            PeerTransportError::AllowlistEmpty
        ))
    ));
    assert_eq!(consumer.bodies.lock().await.as_slice(), &["before"]);
    assert_eq!(client.active_connection_count().await, 1);

    write_allowlist(tmp.path(), Some(23));
    client
        .send_to(&server_id, second)
        .await
        .expect("retry after re-allow");
    assert_eq!(ingress.accept_next().await.expect("re-allowed"), 2);
    assert_eq!(
        consumer.bodies.lock().await.as_slice(),
        &["before", "after"]
    );
    assert_eq!(client.active_connection_count().await, 1);

    client.shutdown().await.expect("shutdown client");
    server.shutdown().await.expect("shutdown server");
}

#[test]
fn p2p_listen_config_is_ungated_and_startup_owned() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write_allowlist(tmp.path(), Some(23));
    assert!(
        p2p_listener_requested(&workspace_p2p_config_path(tmp.path())).expect("read listener key")
    );

    let startup = source("src/infrastructure/startup.rs");
    assert!(startup.contains("p2p_listener_requested"));
    assert!(startup.contains("p2p_listen"));
    assert!(!source("src/main.rs").contains("spawn_p2p_listener"));
}

#[test]
fn keystone_and_production_ingress_forbid_all_three_front_door_bypasses() {
    let test_source = source("tests/conformance_p2p_ingress.rs");
    let agent_delivery = concat!("Agent", "Delivery {");
    let direct_deliver = concat!(".", "deliver(");
    let direct_handler = concat!(".", "handle_verified_peer_frame(");
    assert!(!test_source.contains(agent_delivery));
    assert!(!test_source.contains(direct_deliver));
    assert!(!test_source.contains(direct_handler));

    let ingress = source("src/adapters/iroh/ingress.rs");
    assert!(!ingress.contains(agent_delivery));
    assert!(!ingress.contains(concat!("Local", "MessageBus")));
    let process = ingress
        .split("async fn process_frame")
        .nth(1)
        .expect("production frame processor");
    let verify = process
        .find("verify_envelope_reserved")
        .expect("shared verifier call");
    let front_door = process
        .find("handle_verified_peer_frame")
        .expect("existing front-door call");
    assert!(
        verify < front_door,
        "front door must follow shared verification"
    );
}

fn hermetic_ingress(
    workspace: &Path,
) -> (
    Arc<IrohPeerIngress>,
    mpsc::Sender<rustain::domain::ports::InboundFrame>,
    Arc<RecordingConsumer>,
    mpsc::UnboundedReceiver<rustain::domain::events::AppEvent>,
) {
    let (handler, consumer, domain_rx) = front_door_fixture();
    let (frames_tx, frames_rx) = mpsc::channel(16);
    let ingress = Arc::new(IrohPeerIngress::with_inbound(
        frames_rx,
        handler,
        workspace.to_path_buf(),
        || 1_000,
    ));
    (ingress, frames_tx, consumer, domain_rx)
}

fn inbound(
    envelope: AgentEnvelope<serde_json::Value>,
    peer_id: &PeerId,
) -> rustain::domain::ports::InboundFrame {
    rustain::domain::ports::InboundFrame {
        envelope,
        peer_id: peer_id.clone(),
    }
}

/// Let every task that became runnable make progress. Under `start_paused` the
/// sleeps are instant, so this settles the runtime without a wall-clock race.
async fn settle_runtime() {
    for _ in 0..64 {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
}

/// AC2/m4, review decision D2. An ingest outcome that says nothing about
/// whether the recipient took the message must not be guessed at: the frame is
/// neither committed (which would lose it) nor released (which would let the
/// same signed frame be delivered twice).
#[tokio::test(start_paused = true)]
async fn an_uncertain_ingest_parks_the_reservation_until_the_worker_settles() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write_allowlist(tmp.path(), Some(23));
    let (ingress, frames, consumer, _domain_rx) = hermetic_ingress(tmp.path());
    let signer = AgentSigner::from_signing_key(signing_key(23));
    let peer = signer.identity().peer_id.clone();
    consumer.stall(&peer).await;

    let first = signed_envelope(&signer, 1, Vec::new(), "parked", "taken then stalled");
    let chained = signed_envelope(
        &signer,
        2,
        entry_hash(&first.header).expect("first hash"),
        "after-settlement",
        "after settlement",
    );
    for frame in [first.clone(), first.clone(), chained] {
        frames
            .send(inbound(frame, &peer))
            .await
            .expect("queue frame");
    }

    // The recipient takes the message and stalls; our own wait expires without
    // learning the outcome.
    assert!(matches!(
        ingress.accept_next().await,
        Err(PeerIngressError::Delivery(
            rustain::adapters::rap::PeerDeliveryError::IngestTimeout
        ))
    ));
    assert_eq!(consumer.taken(), 1);

    // The identical signed frame is refused while the outcome is unknown — a
    // rollback here is what would hand a peer a duplicate delivery.
    match ingress.accept_next().await {
        Err(PeerIngressError::Transport(PeerTransportError::ReplayRejected(reason))) => {
            assert!(
                reason.contains("pending"),
                "the refusal must name the parked reservation, got {reason}"
            );
        }
        other => panic!("expected a parked-reservation refusal, got {other:?}"),
    }
    assert_eq!(consumer.taken(), 1, "no second delivery of the same frame");

    // Once the recipient finishes, the worker's own outcome resolves the parked
    // reservation: the position commits, so the next frame in the chain fits.
    consumer.release().await;
    settle_runtime().await;
    assert_eq!(ingress.accept_next().await.expect("chained frame"), 2);
    assert_eq!(
        consumer.bodies().await.as_slice(),
        &["taken then stalled", "after settlement"]
    );
}

/// Review decision D3. Admission is per peer, so progress must be too: one
/// stalled recipient may not suspend every other peer's frames.
#[tokio::test(start_paused = true)]
async fn a_stalled_peer_does_not_block_another_peers_frame() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write_allowlist_pins(tmp.path(), &[23, 29]);
    let (ingress, frames, consumer, _domain_rx) = hermetic_ingress(tmp.path());
    let stalled_signer = AgentSigner::from_signing_key(signing_key(23));
    let live_signer = AgentSigner::from_signing_key(signing_key(29));
    let stalled_peer = stalled_signer.identity().peer_id.clone();
    let live_peer = live_signer.identity().peer_id.clone();
    consumer.stall(&stalled_peer).await;

    let shutdown = tokio_util::sync::CancellationToken::new();
    let listener = tokio::spawn(Arc::clone(&ingress).run(shutdown.child_token()));

    frames
        .send(inbound(
            signed_envelope_to(
                &stalled_signer,
                "recipient-stalled",
                1,
                Vec::new(),
                "stalled",
                "stalled body",
            ),
            &stalled_peer,
        ))
        .await
        .expect("queue stalled frame");
    frames
        .send(inbound(
            signed_envelope_to(
                &live_signer,
                "recipient-live",
                1,
                Vec::new(),
                "live",
                "live body",
            ),
            &live_peer,
        ))
        .await
        .expect("queue live frame");

    settle_runtime().await;
    assert_eq!(
        consumer.bodies().await.as_slice(),
        &["live body"],
        "the unrelated peer's frame must land while the first recipient stalls"
    );

    consumer.release().await;
    settle_runtime().await;
    assert_eq!(consumer.bodies().await.len(), 2);

    shutdown.cancel();
    listener
        .await
        .expect("listener task")
        .expect("listener stops cleanly");
}

/// The other half of D3: concurrency across peers must not become reordering
/// within a peer. The feed is `prev_hash`-chained, so a swapped pair would be
/// refused as a fork rather than delivered out of order.
#[tokio::test(start_paused = true)]
async fn one_peers_frames_stay_in_arrival_order() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write_allowlist(tmp.path(), Some(23));
    let (ingress, frames, consumer, _domain_rx) = hermetic_ingress(tmp.path());
    let signer = AgentSigner::from_signing_key(signing_key(23));
    let peer = signer.identity().peer_id.clone();

    let first = signed_envelope(&signer, 1, Vec::new(), "order-1", "first");
    let second = signed_envelope(
        &signer,
        2,
        entry_hash(&first.header).expect("first hash"),
        "order-2",
        "second",
    );

    let shutdown = tokio_util::sync::CancellationToken::new();
    let listener = tokio::spawn(Arc::clone(&ingress).run(shutdown.child_token()));
    for frame in [first, second] {
        frames
            .send(inbound(frame, &peer))
            .await
            .expect("queue frame");
    }

    settle_runtime().await;
    assert_eq!(consumer.bodies().await.as_slice(), &["first", "second"]);

    shutdown.cancel();
    listener
        .await
        .expect("listener task")
        .expect("listener stops cleanly");
}

#[test]
fn every_p2p_operator_string_stays_within_the_wording_ceiling() {
    let owned_modules = [
        "src/domain/ports/peer_transport.rs",
        "src/domain/models/peer_identity.rs",
        "src/domain/models/p2p_peer_spec.rs",
        "src/domain/services/peer_dial.rs",
        "src/adapters/p2p_config.rs",
        "src/adapters/iroh/mod.rs",
        "src/adapters/iroh/ingress.rs",
    ];
    let mut strings = Vec::new();
    for relative in owned_modules {
        let contents = source(relative);
        let production = contents.split("#[cfg(test)]").next().unwrap_or(&contents);
        strings.extend(rust_string_literals(production));
    }

    let startup = source("src/infrastructure/startup.rs");
    strings.extend(rust_string_literals(function_source(
        &startup,
        "fn ensure_p2p_feature_enabled(",
    )));
    let daemon = source("src/adapters/daemon/mod.rs");
    for function in [
        "async fn spawn_p2p_listener(",
        "async fn compose_p2p_listener(",
    ] {
        strings.extend(rust_string_literals(function_source(&daemon, function)));
    }

    let reach = "P2P listener ready; directly-addressable peers only; relay disabled";
    assert!(
        strings.iter().any(|value| value == reach),
        "positive control: the one operator-visible reach statement must remain exact and readable"
    );
    assert!(
        strings.len() >= 20,
        "positive control: wording scan found suspiciously few story-owned strings"
    );
    for value in &strings {
        let lowered = value.to_ascii_lowercase();
        for forbidden in [
            "authenticated",
            "tamper-evident",
            "verified",
            "audit trail",
            "evidence",
            "proof",
            "private",
            "anonymous",
            "end-to-end secure",
            // The relay claim this cut may not make, in either of the two
            // phrasings the PRD amendment and `DF-18-CRYPTO-CLUSTER` forbid.
            "the relay cannot read",
            "cannot read",
            "ciphertext only",
            "zero-config",
            "any-nat",
            "relay-reachable",
        ] {
            assert!(
                !lowered.contains(forbidden),
                "forbidden P2P wording {forbidden:?} in shipped string {value:?}"
            );
        }
    }
}
