#![cfg(feature = "p2p")]

//! Story 18.4d, the live half — two endpoints, the real `peer ping` binary, and
//! the receiver's own durable record.
//!
//! The stem contains `p2p`, so
//! `capability_provider.rs::every_p2p_integration_test_is_wired_into_the_ci_p2p_lane`
//! requires this target to be named in the CI `p2p` lane, and it is.
//!
//! # Why the sender is a subprocess
//!
//! AC5's front door is the `rustain peer ping` binary path, and its forbidden
//! bypass is a keystone that calls `send_to`, `dial` or `IrohPeerTransport::bind`
//! directly. `peer_bridge::run_cli` is `pub(crate)`, so the nearest seam that is
//! still **on** the production path is the binary itself — which is what these
//! tests drive, through `CARGO_BIN_EXE_rustain`. Fixtures are seeded around that
//! launch (the allowlist, the reach store, the identity key's data dir); nothing
//! inside the verb is fabricated.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use base64::Engine as _;
use rustain::adapters::iroh::{IrohPeerIngress, IrohPeerTransport, derive_peer_endpoint_identity};
use rustain::adapters::p2p_reach::load_workspace_p2p_reach;
use rustain::adapters::rap::{
    AgentSigner, PeerDeliveryError, VerifiedPeerConsent, VerifiedPeerConsumer,
    VerifiedPeerFrameHandler,
};
use rustain::domain::models::{
    AgentEnvelope, AgentEnvelopeHeader, AgentId, AgentMessage, CorrelationId, Ed25519Sig,
    JournalRecord, MessageKind, PeerFrameAttemptOutcome, PeerId, PeerIdentity, RelayMode,
    RoomEvent,
};
use rustain::domain::ports::{
    AgentMessageBus, PeerDeliveryRecord, PeerInteractionRecorder, PeerTransport,
    RelationshipDeliveryPolicy, TransportRefusalRecord,
};
use rustain::domain::services::peer_reach_filter::imported_reach;
use rustain::infrastructure::paths::{workspace_p2p_config_path, workspace_p2p_reach_path};
use rustain::infrastructure::subagent::{LocalMessageBus, NodeTree};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn signing_key(seed: u8) -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
}

fn base64_key(seed: u8) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(signing_key(seed).verifying_key().to_bytes())
}

fn write_allowlist(workspace: &Path, entries: &[(&str, String)], listen: bool) {
    let dir = workspace.join(".rustain");
    std::fs::create_dir_all(&dir).expect("create config dir");
    let agents = entries
        .iter()
        .map(|(alias, x)| format!(r#""{alias}":{{"pinnedKey":{{"alg":"EdDSA","x":"{x}"}}}}"#))
        .collect::<Vec<_>>()
        .join(",");
    std::fs::write(
        dir.join("p2p.json"),
        format!(r#"{{"listen":{listen},"agents":{{{agents}}}}}"#),
    )
    .expect("write allowlist");
}

// ── The receiver's front door ───────────────────────────────────────────────

#[derive(Default)]
struct AcceptingConsumer {
    bodies: Mutex<Vec<String>>,
}

#[async_trait]
impl VerifiedPeerConsumer for AcceptingConsumer {
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
        _peer_id: &PeerId,
    ) -> Result<(), String> {
        self.bodies.lock().await.push(content.content);
        Ok(())
    }
}

#[derive(Default)]
struct CountingRecorder {
    deliveries: Mutex<Vec<PeerDeliveryRecord>>,
    refusals: Mutex<Vec<TransportRefusalRecord>>,
}

#[async_trait]
impl PeerInteractionRecorder for CountingRecorder {
    async fn record_peer_delivery(&self, record: PeerDeliveryRecord) -> Result<(), String> {
        self.deliveries.lock().await.push(record);
        Ok(())
    }

    async fn record_transport_refusal(&self, record: TransportRefusalRecord) -> Result<(), String> {
        self.refusals.lock().await.push(record);
        Ok(())
    }
}

struct Receiver {
    transport: Arc<IrohPeerTransport>,
    ingress: Arc<IrohPeerIngress>,
    consumer: Arc<AcceptingConsumer>,
    recorder: Arc<CountingRecorder>,
    peer_id: PeerId,
}

/// Bind a receiver on `workspace`, with an injected clock so the refusal quota is
/// exercised deterministically rather than by waiting.
async fn receiver(workspace: &Path, seed: u8, now_ms: i64) -> Receiver {
    let identity = derive_peer_endpoint_identity(&signing_key(seed).verifying_key().to_bytes())
        .expect("receiver identity");
    let transport = Arc::new(
        IrohPeerTransport::bind(
            signing_key(seed).to_bytes(),
            HashMap::new(),
            &RelayMode::Disabled,
        )
        .await
        .expect("bind receiver"),
    );
    let consumer = Arc::new(AcceptingConsumer::default());
    let recorder = Arc::new(CountingRecorder::default());
    let (domain_tx, domain_rx) = mpsc::unbounded_channel();
    // Held so the front door's receipt channel stays open for the whole test.
    std::mem::forget(domain_rx);
    let node_tree = NodeTree::new();
    let bus = Arc::new(LocalMessageBus::new(
        node_tree.clone(),
        Arc::new(RelationshipDeliveryPolicy),
    )) as Arc<dyn AgentMessageBus>;
    let handler = Arc::new(VerifiedPeerFrameHandler::new(
        node_tree,
        Arc::new(ArcSwap::from_pointee(bus)),
        domain_tx,
        consumer.clone(),
        recorder.clone(),
    ));
    let ingress = Arc::new(
        IrohPeerIngress::with_now(
            transport.clone(),
            handler,
            workspace.to_path_buf(),
            move || now_ms,
        )
        .expect("compose ingress"),
    );
    Receiver {
        transport,
        ingress,
        consumer,
        recorder,
        peer_id: identity.peer_id,
    }
}

// ── The sender, driven through the real binary ──────────────────────────────

struct Sender {
    workspace: tempfile::TempDir,
    data_dir: tempfile::TempDir,
    peer_id: PeerId,
    /// The base64url JWK `x` a receiver's allowlist pins.
    pinned_x: String,
}

/// Stand up a sender whose identity key the binary itself generated.
///
/// The key is learned the way an operator learns it — by running `peer invite`
/// and reading the ticket — so nothing here hand-builds the identity the receiver
/// will pin.
fn sender() -> Sender {
    let workspace = tempfile::tempdir().expect("sender workspace");
    let data_dir = tempfile::tempdir().expect("sender data dir");
    let out = run_rustain(&workspace, &data_dir, &["peer", "invite"]);
    assert!(out.status.success(), "peer invite failed: {}", out.stderr);
    let blob = out
        .stdout
        .lines()
        .find(|line| line.starts_with("rustain-peer1."))
        .expect("the invite prints a ticket blob")
        .trim()
        .to_owned();
    let ticket = rustain::domain::models::PeerTicket::decode(&blob, 0).expect("decode our ticket");
    let peer_id = ticket.peer_id().expect("sender peer id");
    Sender {
        workspace,
        data_dir,
        peer_id,
        pinned_x: ticket.offered_key.x,
    }
}

struct Output {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

fn run_rustain(
    workspace: &tempfile::TempDir,
    data_dir: &tempfile::TempDir,
    args: &[&str],
) -> Output {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_rustain"))
        .args(args)
        .current_dir(workspace.path())
        .env("RUSTAIN_DATA_DIR", data_dir.path())
        .env("RUSTAIN_CONFIG_DIR", data_dir.path())
        .env("NO_COLOR", "1")
        .output()
        .expect("run the rustain binary");
    Output {
        status: out.status,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// Every outbound frame-attempt row this workspace's journal holds.
async fn frame_attempts(workspace: &Path) -> Vec<(PeerFrameAttemptOutcome, Option<String>)> {
    let journal =
        rustain::infrastructure::subagent::node_journal::NodeJournal::open_workspace(workspace)
            .await
            .expect("open journal");
    journal
        .load()
        .await
        .expect("load journal")
        .into_iter()
        .filter_map(|entry| match entry.record {
            JournalRecord::Room(RoomEvent::PeerFrameAttempted {
                outcome,
                correlation,
                ..
            }) => Some((outcome, Some(correlation))),
            _ => None,
        })
        .collect()
}

/// AC2 through the front door: `rustain peer invite` publishes the reach the
/// listener recorded, and says so.
///
/// Mutants: (a) keeping `Vec::new()` in the mint path renders the empty sentence
/// even with a self record present; (c)/(d) the two copies swap. The positive
/// control is the same binary run on the same workspace **before** the record
/// exists, which must emit the honest empty case.
#[tokio::test]
async fn the_invite_verb_publishes_the_reach_the_listener_recorded() {
    let host = sender();

    // Before: no self record, so the honest empty copy.
    let bare = run_rustain(&host.workspace, &host.data_dir, &["peer", "invite"]);
    assert!(bare.status.success(), "{}", bare.stderr);
    assert!(
        bare.stdout.contains("carries no network address"),
        "positive control: with no record the empty case must render: {}",
        bare.stdout
    );
    assert!(
        !bare.stdout.contains("direct address"),
        "a ticket with no address must advertise none: {}",
        bare.stdout
    );

    // A real endpoint publishes a real record, exactly as the daemon does at bind.
    let transport = IrohPeerTransport::bind(
        signing_key(99).to_bytes(),
        HashMap::new(),
        &RelayMode::Disabled,
    )
    .await
    .expect("bind");
    let address = transport.local_address().expect("address");
    rustain::adapters::p2p_reach::publish_self_reach(
        &workspace_p2p_reach_path(host.workspace.path()),
        &address,
        1,
    )
    .expect("publish self reach");

    let reachable = run_rustain(&host.workspace, &host.data_dir, &["peer", "invite"]);
    assert!(reachable.status.success(), "{}", reachable.stderr);
    assert!(
        reachable.stdout.contains("direct address"),
        "the ticket must now name the addresses it carries: {}",
        reachable.stdout
    );
    assert!(
        !reachable.stdout.contains("carries no network address"),
        "the empty sentence must not survive a populated record: {}",
        reachable.stdout
    );

    // The blob the binary printed carries exactly the record that was published,
    // byte for byte and once.
    let blob = reachable
        .stdout
        .lines()
        .find(|line| line.starts_with("rustain-peer1."))
        .expect("a ticket blob")
        .trim();
    let ticket = rustain::domain::models::PeerTicket::decode(blob, 1).expect("decode");
    assert_eq!(ticket.addresses.len(), 1, "exactly one bundle (D4)");
    assert_eq!(
        ticket.addresses[0].as_slice(),
        address.as_bytes(),
        "the minted bundle must be the published record, unaltered"
    );
    // ⚠ D4's id check is deliberately NOT asserted here: this test publishes a
    // *foreign* endpoint's address, because the binary owns its identity key and
    // an integration test cannot bind with it. In production the published record
    // is this host's own endpoint, and that coupling is proven where the identity
    // really is the endpoint's — `a_real_endpoint_address_decodes_through_the_
    // import_filter` and the end-to-end ping below. So the mismatch is what this
    // ticket must show, and it does:
    assert_eq!(
        imported_reach(
            &ticket.addresses,
            &ticket.peer_id().expect("peer id"),
            /* allow_local */ true
        ),
        Err(rustain::domain::services::peer_reach_filter::ReachRefusal::KeyMismatch),
        "a bundle naming an endpoint other than the ticket's key must be refused"
    );

    transport.shutdown().await.expect("shutdown");
}

/// AC5 positive control, through the front door: two **successive** invocations
/// of the real verb are both accepted.
///
/// This is the property the whole acknowledged-frame design exists for. The
/// sender keeps no durable cursor, so the second process starts at sequence 1
/// again — which the receiver, remembering the first frame, refuses as a replay.
/// It then answers with the position it *will* accept, and the sender's single
/// guided retry lands. ⛔ Skipping that retry (mutant g1) leaves the second
/// invocation refused.
#[tokio::test(flavor = "multi_thread")]
async fn two_successive_ping_invocations_are_both_accepted() {
    let receiver_workspace = tempfile::tempdir().expect("receiver workspace");
    let sender = sender();
    // The receiver admits the sender's key — the one the sender's own ticket
    // carried, not one this test invented.
    write_allowlist(
        receiver_workspace.path(),
        &[("a", sender.pinned_x.clone())],
        true,
    );
    let receiver = receiver(receiver_workspace.path(), 91, 1_000).await;
    let address = receiver.transport.local_address().expect("address");

    // The sender pins the receiver and records where to reach it. Seeding these
    // two files is fixture setup **around** the real launch, which Rule 2 allows;
    // the interactive `peer add` path is AC7's capture to drive.
    write_allowlist(sender.workspace.path(), &[("b", base64_key(91))], false);
    let reach_path = workspace_p2p_reach_path(sender.workspace.path());
    rustain::adapters::p2p_reach::record_peer_reach(
        &reach_path,
        &workspace_p2p_config_path(sender.workspace.path()),
        "b",
        &receiver.peer_id,
        &address,
        0,
    )
    .expect("record peer reach");
    assert_eq!(
        load_workspace_p2p_reach(&reach_path)
            .store()
            .and_then(|store| store.peer("b"))
            .map(|reach| reach.address.clone()),
        Some(address),
        "positive control: the fixture really is readable by the builder's loader"
    );

    let shutdown = CancellationToken::new();
    let running = tokio::spawn(Arc::clone(&receiver.ingress).run(shutdown.clone()));

    for attempt in 1..=2 {
        let out = tokio::task::spawn_blocking({
            let workspace = sender.workspace.path().to_path_buf();
            let data_dir = sender.data_dir.path().to_path_buf();
            move || {
                std::process::Command::new(env!("CARGO_BIN_EXE_rustain"))
                    .args(["peer", "ping", "b"])
                    .current_dir(&workspace)
                    .env("RUSTAIN_DATA_DIR", &data_dir)
                    .env("RUSTAIN_CONFIG_DIR", &data_dir)
                    .env("NO_COLOR", "1")
                    .output()
                    .expect("run peer ping")
            }
        })
        .await
        .expect("ping task");
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            out.status.success(),
            "invocation {attempt} failed: {stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            stdout.contains("— accepted."),
            "invocation {attempt} must repeat the verdict it received: {stdout}"
        );
    }

    // The receiver took both frames, and both journaled acceptance against the
    // identity the sender's own ticket carried.
    assert_eq!(
        receiver.consumer.bodies.lock().await.as_slice(),
        &["ping", "ping"]
    );
    let deliveries = receiver.recorder.deliveries.lock().await;
    assert_eq!(deliveries.len(), 2);
    assert!(
        deliveries
            .iter()
            .all(|record| record.peer == sender.peer_id),
        "the receiver's rows must name the sender's pinned identity"
    );
    drop(deliveries);
    assert!(
        receiver.recorder.refusals.lock().await.is_empty(),
        "an accepted frame must not journal a refusal"
    );

    // AC9 / NFR74 against **real rows**: the sender's own journal carries the
    // outbound facts, with only `PeerId`-class identity in them.
    let attempts = frame_attempts(sender.workspace.path()).await;
    assert!(
        attempts
            .iter()
            .filter(|(outcome, _)| *outcome == PeerFrameAttemptOutcome::Accepted)
            .count()
            >= 2,
        "both invocations must have journaled an acceptance: {attempts:?}"
    );
    let raw = std::fs::read_to_string(
        rustain::infrastructure::subagent::node_journal::NodeJournal::open_workspace(
            sender.workspace.path(),
        )
        .await
        .expect("open journal")
        .path(),
    )
    .expect("read journal");
    for forbidden in ["\"addrs\"", "203.0.113", "127.0.0.1", "\"Ip\""] {
        assert!(
            !raw.contains(forbidden),
            "NFR74: a transport address reached the durable journal ({forbidden})"
        );
    }
    assert!(
        raw.contains("peer_frame_attempted"),
        "positive control: the assertion above must be running against real rows"
    );

    shutdown.cancel();
    let _ = running.await;
    receiver
        .transport
        .shutdown()
        .await
        .expect("shutdown receiver");
}

/// AC6 — a refused frame is journaled once, and the sender is told the class.
///
/// The receiver's allowlist admits nobody, so every frame is refused at
/// admission. Ratchet: N refusals inside one quota interval produce **exactly
/// one** durable row, asserted with a pinned clock rather than a sleep window.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_frame_is_journaled_once_and_reported_to_the_sender() {
    let receiver_workspace = tempfile::tempdir().expect("receiver workspace");
    // Present and empty: a well-formed "admit nobody".
    write_allowlist(receiver_workspace.path(), &[], true);
    let receiver = receiver(receiver_workspace.path(), 92, 5_000).await;
    let address = receiver.transport.local_address().expect("address");

    let sender_signer = AgentSigner::from_signing_key(signing_key(93));
    let sender_peer = sender_signer.identity().peer_id.clone();
    let client = IrohPeerTransport::bind(
        signing_key(93).to_bytes(),
        HashMap::from([(receiver.peer_id.clone(), address)]),
        &RelayMode::Disabled,
    )
    .await
    .expect("bind client");

    let mut prev_hash = Vec::new();
    let mut sequence = 1u64;
    for frame in 1..=4u64 {
        let envelope = signed(&sender_signer, sequence, prev_hash.clone(), frame);
        let (sent, accepted) = tokio::join!(
            client.send_to(&receiver.peer_id, envelope),
            receiver.ingress.accept_next()
        );
        assert!(accepted.is_err(), "an unlisted peer must be refused");
        let verdict = sent.expect("the frame still travels");
        assert_eq!(
            verdict.outcome(),
            rustain::domain::models::FrameOutcome::Refused(
                rustain::domain::models::FrameRefusal::NotAdmitted
            ),
            "the sender must be told the class, on frame {frame}"
        );
        // A refused frame never advances the receiver's head, so the sender's
        // position does not move either.
        sequence = 1;
        prev_hash = Vec::new();
    }

    // Ratchet: four refusals, one row. Deterministic — the clock never moved.
    let refusals = receiver.recorder.refusals.lock().await;
    assert_eq!(
        refusals.len(),
        1,
        "the durable refusal record is rate-bounded, not per frame"
    );
    assert_eq!(refusals[0].peer, sender_peer);
    assert!(
        refusals[0].detail.contains("admits nobody"),
        "the row must name the typed refusal: {}",
        refusals[0].detail
    );
    // ⛔ Truthfulness: the refusal precedes signature checking.
    for banned in ["signature", "authenticated", "tamper"] {
        assert!(
            !refusals[0].detail.to_ascii_lowercase().contains(banned),
            "the row must claim nothing about the envelope: {}",
            refusals[0].detail
        );
    }
    drop(refusals);
    assert_eq!(
        receiver.ingress.suppressed_refusals().await,
        3,
        "the suppressed repeats are counted, never journaled"
    );

    client.shutdown().await.expect("shutdown client");
    receiver
        .transport
        .shutdown()
        .await
        .expect("shutdown receiver");
}

fn signed(
    signer: &AgentSigner,
    sequence: u64,
    prev_hash: Vec<u8>,
    nonce: u64,
) -> AgentEnvelope<serde_json::Value> {
    let sender =
        AgentId::from_peer_path(&format!("{}/peer-ping", signer.identity().peer_id.as_str()))
            .expect("peer-rooted sender");
    let recipient = AgentId::from_peer_path(&format!(
        "{}/peer-ping-recipient",
        signer.identity().peer_id.as_str()
    ))
    .expect("peer-rooted recipient");
    signer
        .sign(
            sender,
            recipient,
            CorrelationId::new(format!("corr-{nonce}")),
            MessageKind::PeerMessage,
            String::new(),
            sequence,
            i64::MAX,
            format!("nonce-{nonce}"),
            prev_hash,
            serde_json::Value::String("ping".to_owned()),
        )
        .expect("sign")
}

/// Class-C guard for the ungated import filter.
///
/// The filter decodes reach with its own mirror of the adapter's encoding, so a
/// shape change in the adapter would otherwise be discovered by a refused dial in
/// production rather than here. This decodes a **real bound endpoint's** address
/// through it.
#[tokio::test]
async fn a_real_endpoint_address_decodes_through_the_import_filter() {
    let transport = IrohPeerTransport::bind(
        signing_key(94).to_bytes(),
        HashMap::new(),
        &RelayMode::Disabled,
    )
    .await
    .expect("bind");
    let address = transport.local_address().expect("local address");
    let identity = derive_peer_endpoint_identity(&signing_key(94).verifying_key().to_bytes())
        .expect("identity");

    // A freshly bound endpoint advertises this machine's own addresses, so the
    // per-import opt-in is exactly the flag a same-machine demo needs.
    let imported = imported_reach(
        &[address.as_bytes().to_vec()],
        &identity.peer_id,
        /* allow_local */ true,
    )
    .expect("a real address must decode through the filter")
    .expect("a bound endpoint advertises at least one address");
    assert_eq!(imported, address, "the filter must not rewrite the bundle");
    // And the same address is accepted by `bind`, so the filter's output is
    // consumable rather than merely non-empty.
    IrohPeerTransport::bind(
        signing_key(95).to_bytes(),
        HashMap::from([(identity.peer_id.clone(), imported)]),
        &RelayMode::Disabled,
    )
    .await
    .expect("the filtered address must still bind")
    .shutdown()
    .await
    .expect("shutdown");

    // The bundle names the endpoint it claims to name; a mismatch is a refusal.
    assert!(
        imported_reach(
            &[address.as_bytes().to_vec()],
            &derive_peer_endpoint_identity(&signing_key(96).verifying_key().to_bytes())
                .expect("other identity")
                .peer_id,
            true
        )
        .is_err(),
        "a bundle naming another endpoint must be refused"
    );

    transport.shutdown().await.expect("shutdown");
}

/// AC9 — `DF-18-4-SENDER-BINDING-UNTESTED`: a sender name, once bound to one
/// peer, can never be claimed by another.
///
/// ⚠ **This collision cannot be constructed through the ingress path**, and that
/// is a finding, not a gap: `sign_envelope` and `verify_envelope_crypto` both
/// refuse a sender that is not rooted at the signer's own `PeerId`, and ingress
/// additionally refuses an envelope whose signer is not the transport peer. So the
/// guard is **defence in depth**, exercised here at the handler's own front door —
/// `handle_verified_peer_frame`, which is the only public entry it has.
#[tokio::test]
async fn a_bound_sender_name_cannot_be_claimed_by_a_second_peer() {
    let consumer = Arc::new(AcceptingConsumer::default());
    let (domain_tx, domain_rx) = mpsc::unbounded_channel();
    std::mem::forget(domain_rx);
    let node_tree = NodeTree::new();
    let bus = Arc::new(LocalMessageBus::new(
        node_tree.clone(),
        Arc::new(RelationshipDeliveryPolicy),
    )) as Arc<dyn AgentMessageBus>;
    let handler = VerifiedPeerFrameHandler::new(
        node_tree,
        Arc::new(ArcSwap::from_pointee(bus)),
        domain_tx,
        consumer.clone(),
        Arc::new(CountingRecorder::default()),
    );

    let first = AgentSigner::from_signing_key(signing_key(97));
    let first_peer = first.identity().peer_id.clone();
    let envelope = signed(&first, 1, Vec::new(), 1);
    let sender_name = envelope.header.sender.clone();
    handler
        .handle_verified_peer_frame(envelope, first_peer.clone())
        .await
        .expect("the first peer binds its own sender name");

    // Positive control: the legitimate repeat is admitted and consumes no new
    // budget. ⛔ Without this, a guard that refused every resend would pass.
    //
    // ⚠ The handler holds no replay window — that is the ingress's job — so this
    // exercises the binding table alone, which is the guard under test.
    let resend = handler
        .handle_verified_peer_frame(signed(&first, 2, Vec::new(), 2), first_peer.clone())
        .await;
    assert!(
        !matches!(resend, Err(PeerDeliveryError::PeerBindingMismatch)),
        "the same peer resending under its own bound name must not hit the guard: {resend:?}"
    );

    // The collision: a different peer presenting the already-bound sender name.
    let second_peer =
        PeerIdentity::from_public_key(signing_key(98).verifying_key().to_bytes().to_vec())
            .expect("second identity")
            .peer_id;
    let forged = AgentEnvelope::new(
        AgentEnvelopeHeader {
            message_type: String::new(),
            sender: sender_name,
            recipient: AgentId::parse("peer-ping-recipient").expect("recipient"),
            correlation_id: CorrelationId::new("forged"),
            kind: MessageKind::PeerMessage,
            sequence: 1,
            not_after: i64::MAX,
            nonce: "forged".to_owned(),
            content_hash: Vec::new(),
            prev_hash: Vec::new(),
        },
        serde_json::Value::String("ping".to_owned()),
        PeerIdentity::from_public_key(signing_key(98).verifying_key().to_bytes().to_vec())
            .expect("second identity"),
        Ed25519Sig(vec![0u8; 64]),
    );
    assert!(
        matches!(
            handler
                .handle_verified_peer_frame(forged, second_peer)
                .await,
            Err(PeerDeliveryError::PeerBindingMismatch)
        ),
        "a second peer must never claim a bound sender name"
    );
}

/// Rule 1 — after this story, exactly one **non-test** call site reaches
/// `PeerTransport::send_to`, and it is the `peer ping` handler.
///
/// ⚠ A count alone would be a source assertion, which Rule 1 forbids on its own;
/// the behavioural half is `two_successive_ping_invocations_are_both_accepted`
/// above, which drives the real binary. This is the companion ratchet that keeps
/// a second, unexercised producer from appearing.
#[test]
fn exactly_one_production_caller_reaches_send_to() {
    let mut callers = Vec::new();
    for relative in walk_sources(&root().join("src")) {
        let text = std::fs::read_to_string(&relative).expect("read source");
        // Strip every test module, including the feature-gated `#[cfg(all(test,
        // …))]` spelling the p2p fixtures use — the shape a naive
        // `#[cfg(test)]` split misses, and the one that would have made this
        // ratchet count a fixture as a production caller.
        let production = text
            .split("#[cfg(test)]")
            .next()
            .unwrap_or(&text)
            .split("#[cfg(all(test")
            .next()
            .unwrap_or(&text)
            .to_owned();
        let name = relative
            .strip_prefix(root())
            .expect("inside the crate")
            .to_string_lossy()
            .into_owned();
        // The port declaration and the adapter's own implementation are the
        // mechanism, not callers of it.
        if name.ends_with("domain/ports/peer_transport.rs")
            || name.ends_with("adapters/iroh/mod.rs")
        {
            continue;
        }
        for _ in 0..production.matches(".send_to(").count() {
            callers.push(name.clone());
        }
    }
    // `AttachServer::send_to` is the daemon's own unrelated frame writer on the
    // local Unix socket; it is not this port.
    callers.retain(|name| !name.ends_with("adapters/daemon/server.rs"));
    assert_eq!(
        callers,
        vec!["src/infrastructure/runtime/peer_bridge.rs".to_owned()],
        "the peer transport must have exactly one production caller, and it is `peer ping`"
    );
}

fn walk_sources(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in std::fs::read_dir(&next).expect("read dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}
