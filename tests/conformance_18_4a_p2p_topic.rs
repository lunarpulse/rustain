//! Story 18.4a — Topic & shared-context replication conformance.
//!
//! FR150 · FR150-a · FR151 · NFR71 · NFR72 clause (b) — and nothing else.
//!
//! # The front doors these keystones enter
//!
//! * **Inbound replication** — `IrohPeerIngress::with_inbound` + `accept_next`,
//!   the hermetic seam the shipped p2p harnesses use: every frame travels the
//!   same `process_frame` path the production listener uses (allowlist →
//!   signature → feed chain → the verified-peer delivery front door → the Topic
//!   store). ⛔ No keystone calls `PeerTopicStore::admit` in place of it when
//!   the claim under test is about the wire path; the store's own admission
//!   rules are pinned by its unit tests.
//! * **Context injection** — `inject_assembled_context`
//!   (`adapters/tui/handlers/context_command.rs`), whose sole production caller
//!   is `LocalTurnDriver::submit`. ⛔ The taint bit is derived by the same
//!   production expression that caller uses — `bundle.has_peer_origin()` —
//!   never set by hand.
//!
//! # Deterministic time
//!
//! Every clock here is `MockClock` / a constant closure; ⛔ no sleep gates a
//! correctness assertion (the AC8 relay exchange waits on events and a
//! deadline, exactly as `conformance_p2p_relay.rs` does).

#![cfg(all(feature = "p2p", feature = "p2p-test-utils"))]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use base64::Engine as _;

use rustain::adapters::iroh::{IrohPeerIngress, IrohPeerTransport, derive_peer_endpoint_identity};
use rustain::adapters::rap::{
    AgentSigner, PeerTopicStore, TopicGossip, VerifiedPeerConsent, VerifiedPeerConsumer,
    VerifiedPeerFrameHandler, entry_hash,
};
use rustain::adapters::relay_config::load_workspace_relay_config;
use rustain::domain::models::{
    AgentEnvelope, AgentId, ArtifactId, CompletionOptions, ContentHash, ContextBudget, ContextRef,
    ContextRefProvenance, ContextSource, ContextSummary, CorrelationId, FeedPosition, MessageKind,
    PeerId, RoomEvent, StopReason, StreamChunk, ToolDefinition, ToolResult, TopicHead,
};
use rustain::domain::ports::{
    AgentMessageBus, ContextPort, InboundFrame, PeerDeliveryRecord, PeerInteractionRecorder,
    PeerTransport, PeerTransportError, RelationshipDeliveryPolicy, SecurityPort, StreamingProvider,
    ToolSetPort, UsageLedgerPort,
};
use rustain::domain::services::peer_reach_filter::{canonical_relay_url, describe_reach};
use rustain::domain::services::transparency::{TransparencyFilter, TransparencyKind};
use rustain::infrastructure::paths::workspace_relay_config_path;
use rustain::infrastructure::subagent::{LocalMessageBus, NodeTree};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

const DEADLINE: Duration = Duration::from_secs(30);

// ── Harness ─────────────────────────────────────────────────────────────────

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn source(relative: &str) -> String {
    std::fs::read_to_string(root().join(relative))
        .unwrap_or_else(|error| panic!("read {relative}: {error}"))
}

fn signing_key(seed: u8) -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
}

fn peer_of(seed: u8) -> PeerId {
    derive_peer_endpoint_identity(&signing_key(seed).verifying_key().to_bytes())
        .expect("identity")
        .peer_id
}

fn signer_of(seed: u8) -> AgentSigner {
    AgentSigner::from_signing_key(signing_key(seed))
}

fn write_allowlist(workspace: &Path, seeds: &[u8]) {
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

/// One signed handle, with the bound summary the constructor enforces.
fn mk_handle(content: u8, issuer: &PeerId, summary: &str) -> ContextRef {
    mk_handle_until(content, issuer, summary, 1_000 + 60_000)
}

fn mk_handle_until(content: u8, issuer: &PeerId, summary: &str, not_after: i64) -> ContextRef {
    let hash = ContentHash::from_bytes([content; 32]);
    ContextRef {
        artifact: ArtifactId::from(hash),
        content_hash: hash,
        producer: AgentId::parse("producer").expect("valid agent id"),
        issuer: issuer.clone(),
        summary: ContextSummary::new(summary).expect("bounded summary"),
        provenance: ContextRefProvenance::Authored,
        not_after,
    }
}

/// Sign one Topic-gossip frame, through the single `rap::wire` sign seam.
fn sign_gossip(
    signer: &AgentSigner,
    topic: &str,
    sequence: u64,
    prev_hash: Vec<u8>,
    refs: Vec<ContextRef>,
    heads: Vec<TopicHead>,
) -> AgentEnvelope<serde_json::Value> {
    sign_gossip_at(
        signer,
        topic,
        sequence,
        prev_hash,
        refs,
        heads,
        1_000 + 60_000,
    )
}

/// `not_after` is wall **milliseconds** — the unit this frame path's verify
/// seam uses.
fn sign_gossip_at(
    signer: &AgentSigner,
    topic: &str,
    sequence: u64,
    prev_hash: Vec<u8>,
    refs: Vec<ContextRef>,
    heads: Vec<TopicHead>,
    not_after: i64,
) -> AgentEnvelope<serde_json::Value> {
    let sender = AgentId::from_peer_path(&format!(
        "{}/{}",
        signer.identity().peer_id.as_str(),
        rustain::adapters::rap::TOPIC_SENDER_SUFFIX
    ))
    .expect("peer-rooted sender");
    let recipient = AgentId::from_peer_path(&format!(
        "{}/{}",
        signer.identity().peer_id.as_str(),
        rustain::adapters::rap::TOPIC_RECIPIENT_SUFFIX
    ))
    .expect("peer-rooted recipient");
    let body = serde_json::to_value(TopicGossip { refs, heads }).expect("gossip encodes");
    signer
        .sign(
            sender,
            recipient,
            CorrelationId::new(topic),
            MessageKind::TopicGossip,
            sequence,
            not_after,
            format!("topic-{topic}-{sequence}"),
            prev_hash,
            body,
        )
        .expect("sign gossip frame")
}

struct AcceptingConsumer;

#[async_trait]
impl VerifiedPeerConsumer for AcceptingConsumer {
    async fn consent(
        &self,
        _recipient: &AgentId,
        _content: &rustain::domain::models::AgentMessage,
        _peer_id: &PeerId,
    ) -> Result<VerifiedPeerConsent, String> {
        Ok(VerifiedPeerConsent::Accept)
    }

    async fn ingest(
        &self,
        _recipient: &AgentId,
        _content: rustain::domain::models::AgentMessage,
        _peer_id: &PeerId,
    ) -> Result<(), String> {
        Ok(())
    }
}

struct AcceptingRecorder;

#[async_trait]
impl PeerInteractionRecorder for AcceptingRecorder {
    async fn record_peer_delivery(&self, _record: PeerDeliveryRecord) -> Result<(), String> {
        Ok(())
    }

    async fn record_transport_refusal(
        &self,
        _record: rustain::domain::ports::TransportRefusalRecord,
    ) -> Result<(), String> {
        Ok(())
    }
}

/// One receiving host, wired the way the production listener is: the same
/// handler type, the same ingress, the same admission order — only the socket
/// is replaced by a channel, which `with_inbound` exists for.
struct Receiver {
    ingress: Arc<IrohPeerIngress>,
    handler: Arc<VerifiedPeerFrameHandler>,
    sender: mpsc::Sender<InboundFrame>,
    store: Arc<PeerTopicStore>,
    /// Held, ⛔ never read: dropping it closes the domain event channel, and a
    /// frame that would otherwise deliver would then fail for a harness
    /// artifact instead — the exact false-refusal the namespace keystone below
    /// must not be satisfiable by.
    _domain_rx: mpsc::UnboundedReceiver<rustain::domain::events::AppEvent>,
}

fn receiver(workspace: &Path, admitted: &[u8]) -> Receiver {
    write_allowlist(workspace, admitted);
    let node_tree = NodeTree::new();
    let bus = Arc::new(LocalMessageBus::new(
        node_tree.clone(),
        Arc::new(RelationshipDeliveryPolicy),
    )) as Arc<dyn AgentMessageBus>;
    let (domain_tx, domain_rx) = mpsc::unbounded_channel();
    let store = Arc::new(PeerTopicStore::new());
    let handler = Arc::new(
        VerifiedPeerFrameHandler::new(
            node_tree,
            Arc::new(ArcSwap::from_pointee(bus)),
            domain_tx,
            Arc::new(AcceptingConsumer),
            Arc::new(AcceptingRecorder),
        )
        .with_topics(Arc::clone(&store))
        .with_now(|| 1_000),
    );
    let (sender, inbound) = mpsc::channel(16);
    let ingress = Arc::new(IrohPeerIngress::with_inbound(
        inbound,
        Arc::clone(&handler),
        workspace.to_path_buf(),
        || 1_000,
    ));
    Receiver {
        ingress,
        handler,
        sender,
        store,
        _domain_rx: domain_rx,
    }
}

#[derive(Debug)]
enum Admission {
    Accepted,
    /// Feed fork or gap — the predecessor has not landed yet. Retried.
    OutOfOrder,
    /// Refused for a reason a sender would not retry, carrying the refusal text
    /// so a keystone can assert the refusal is **attributable** — satisfied by
    /// the mechanism under test, ⛔ never by an unrelated failure.
    Terminal(String),
}

/// Drive one frame through the receiving host's production admission path.
async fn deliver(
    host: &Receiver,
    envelope: AgentEnvelope<serde_json::Value>,
    from: &PeerId,
) -> Admission {
    host.sender
        .send(InboundFrame {
            envelope,
            peer_id: from.clone(),
            responder: None,
        })
        .await
        .expect("inbound channel open");
    match host.ingress.accept_next().await {
        Ok(_) => Admission::Accepted,
        Err(error) => {
            // The sender-side discipline, modelled: a feed-position refusal is
            // the one a peer retries against, exactly as `peer ping` retries
            // against `FeedPositionMismatch`. Every other refusal is terminal —
            // ⛔ re-sending a replay is the livelock this trace would otherwise
            // spin in.
            if error.to_string().contains("fork or gap") {
                Admission::OutOfOrder
            } else {
                Admission::Terminal(error.to_string())
            }
        }
    }
}

/// A deterministic sender retry schedule: deliver each frame in the given
/// order; a frame the feed chain refuses goes to the back of the queue, which
/// is what a sender does with a guided feed-position verdict — ⛔ not a sleep.
async fn replay_trace(host: &Receiver, frames: Vec<(AgentEnvelope<serde_json::Value>, PeerId)>) {
    let mut pending: std::collections::VecDeque<_> = frames.into();
    let mut spins = 0usize;
    while let Some((envelope, from)) = pending.pop_front() {
        spins += 1;
        assert!(
            spins < 10_000,
            "the deterministic trace did not converge — the feed wedged"
        );
        if matches!(
            deliver(host, envelope.clone(), &from).await,
            Admission::OutOfOrder
        ) {
            pending.push_back((envelope, from));
        }
    }
}

/// Whether one frame was refused, for assertion sites.
async fn not_accepted(
    host: &Receiver,
    envelope: AgentEnvelope<serde_json::Value>,
    from: &PeerId,
) -> bool {
    let admitted = matches!(deliver(host, envelope, from).await, Admission::Accepted);
    !admitted
}

/// The terminal-refusal text, for attributable-refusal assertions.
async fn terminal_refusal(
    host: &Receiver,
    envelope: AgentEnvelope<serde_json::Value>,
    from: &PeerId,
) -> String {
    match deliver(host, envelope, from).await {
        Admission::Terminal(text) => text,
        other => panic!("expected a terminal refusal, got {other:?}"),
    }
}

/// Whether one frame was admitted, for assertion sites.
async fn accepted(
    host: &Receiver,
    envelope: AgentEnvelope<serde_json::Value>,
    from: &PeerId,
) -> bool {
    matches!(deliver(host, envelope, from).await, Admission::Accepted)
}

// ── AC1 — order-insensitive assembly over a Topic (FR150, NFR71, gate 1) ────

#[tokio::test]
async fn ac1_handles_assemble_byte_identically_under_permuted_arrival() {
    let dir_a = tempfile::tempdir().expect("tempdir");
    let dir_b = tempfile::tempdir().expect("tempdir");
    let host_a = receiver(dir_a.path(), &[11, 12]);
    let host_b = receiver(dir_b.path(), &[11, 12]);

    let signer_a = signer_of(11);
    let signer_b = signer_of(12);
    let peer_a = peer_of(11);
    let peer_b = peer_of(12);

    // ⚑ Positive control: the set is non-trivial — TWO distinct issuers and a
    // dedup collision (both peers assert the same content) — so the permutation
    // assertion is not comparing two empty bundles.
    let collision_a = mk_handle(7, &peer_a, "the event bus is NATS");
    let collision_b = mk_handle(7, &peer_b, "the event bus is Kafka");
    let frames_ab = [
        sign_gossip(
            &signer_a,
            "t-ac1",
            1,
            Vec::new(),
            vec![collision_a.clone()],
            vec![],
        ),
        sign_gossip(
            &signer_b,
            "t-ac1",
            1,
            Vec::new(),
            vec![collision_b.clone()],
            vec![],
        ),
    ];
    let frames_ba = [
        sign_gossip(&signer_b, "t-ac1", 1, Vec::new(), vec![collision_b], vec![]),
        sign_gossip(&signer_a, "t-ac1", 1, Vec::new(), vec![collision_a], vec![]),
    ];

    // A founds the topic first (an empty member set is established by the
    // frame that opens it); the capability grant to B comes **after**, which
    // is the order a real grant arrives in.
    assert!(accepted(&host_a, frames_ab[0].clone(), &peer_a).await);
    assert!(accepted(&host_b, frames_ba[1].clone(), &peer_a).await);
    host_a
        .store
        .grant_membership(&CorrelationId::new("t-ac1"), &peer_b)
        .await;
    host_b
        .store
        .grant_membership(&CorrelationId::new("t-ac1"), &peer_b)
        .await;
    assert!(accepted(&host_a, frames_ab[1].clone(), &peer_b).await);
    assert!(accepted(&host_b, frames_ba[0].clone(), &peer_b).await);

    let handles_a = host_a.store.live_handles(1_000).await;
    let handles_b = host_b.store.live_handles(1_000).await;
    assert_eq!(
        handles_a, handles_b,
        "byte-identical handle sets under permuted arrival"
    );
    // The collision kept exactly one survivor, on both hosts, and it is the
    // total-order minimum — ⛔ never "whichever arrived last" (the mutant).
    assert_eq!(handles_a.len(), 1, "the collision must actually collide");
    let minimum_issuer = peer_a.clone().min(peer_b.clone());
    assert_eq!(
        handles_a[0].issuer, minimum_issuer,
        "the survivor is the total-order minimum's issuer, on both orders"
    );

    // And the assembled bundle — entries **and diagnostics** — is identical,
    // which is what NFR71 needs. This assert is what the pre-18.4a
    // `per_source_tokens` HashMap would have flaked.
    let provider_a =
        rustain::adapters::peer_context::PeerContextProvider::new(Arc::clone(&host_a.store))
            .with_now(|| 1_000);
    let provider_b =
        rustain::adapters::peer_context::PeerContextProvider::new(Arc::clone(&host_b.store))
            .with_now(|| 1_000);
    let bundle_a = provider_a
        .assemble("q", ContextBudget::new(4096))
        .await
        .expect("assembles");
    let bundle_b = provider_b
        .assemble("q", ContextBudget::new(4096))
        .await
        .expect("assembles");
    assert_eq!(bundle_a, bundle_b);
    assert!(!bundle_a.is_empty(), "positive control: not two empties");
}

/// AC1 mutant, pinned as its own keystone: a peer entry must never be deduped
/// into the local-memory class, where it could evict the operator's own row.
#[test]
fn ac1_peer_dedup_class_can_never_evict_a_local_memory_row() {
    let classes: HashMap<u8, &str> = [
        (ContextSource::MemoryMd.dedup_class(), "memory"),
        (
            ContextSource::DailyLog(chrono::NaiveDate::from_ymd_opt(2026, 1, 1).expect("date"))
                .dedup_class(),
            "daily",
        ),
        (
            ContextSource::Project("CLAUDE.md".into()).dedup_class(),
            "project",
        ),
        (
            ContextSource::Recall("honcho".into()).dedup_class(),
            "recall",
        ),
        (ContextSource::Group(1).dedup_class(), "group"),
        (ContextSource::Peer(peer_of(31)).dedup_class(), "peer"),
    ]
    .into_iter()
    .collect();
    assert_eq!(
        classes.len(),
        5,
        "the memory pair shares class 0 by design; every other source — peer included — is its own class"
    );
    assert_eq!(
        ContextSource::MemoryMd.dedup_class(),
        ContextSource::DailyLog(chrono::NaiveDate::from_ymd_opt(2026, 1, 1).expect("date"))
            .dedup_class(),
        "positive control: the one shared class is still shared"
    );
}

// ── AC2 — divergent receive order converges (NFR71, gate 2) ────────────────

/// Two receiving hosts, the same signed frames, different drop/dup/reorder
/// traces, a deterministic retry schedule — identical heads, identical context.
#[tokio::test]
async fn ac2_divergent_receive_order_converges_to_one_head_and_one_context() {
    let dir_a = tempfile::tempdir().expect("tempdir");
    let dir_b = tempfile::tempdir().expect("tempdir");
    let host_a = receiver(dir_a.path(), &[11]);
    let host_b = receiver(dir_b.path(), &[11]);
    let signer = signer_of(11);
    let peer = peer_of(11);

    // Three chained frames. ⚑ The chain is what the anti-vacuity control below
    // strips: `prev_hash` is inside the signed header, so these signatures are
    // the real ones the production path verifies.
    let f1 = sign_gossip(
        &signer,
        "t-ac2",
        1,
        Vec::new(),
        vec![mk_handle(1, &peer, "first finding")],
        vec![],
    );
    let h1 = entry_hash(&f1.header).expect("hash");
    let f2 = sign_gossip(
        &signer,
        "t-ac2",
        2,
        h1,
        vec![mk_handle(2, &peer, "second finding")],
        vec![],
    );
    let h2 = entry_hash(&f2.header).expect("hash");
    let f3 = sign_gossip(
        &signer,
        "t-ac2",
        3,
        h2,
        vec![mk_handle(3, &peer, "third finding")],
        vec![],
    );

    // R1: in order, with a duplicate. R2: fully reversed (every frame lands
    // before its predecessor) with one frame dropped and re-sent.
    replay_trace(
        &host_a,
        vec![
            (f1.clone(), peer.clone()),
            (f1.clone(), peer.clone()),
            (f2.clone(), peer.clone()),
            (f3.clone(), peer.clone()),
        ],
    )
    .await;
    replay_trace(
        &host_b,
        vec![
            (f3.clone(), peer.clone()),
            (f2.clone(), peer.clone()),
            (f1.clone(), peer.clone()),
        ],
    )
    .await;

    let handles_a = host_a.store.live_handles(1_000).await;
    let handles_b = host_b.store.live_handles(1_000).await;
    assert_eq!(
        handles_a.len(),
        3,
        "positive control: all three frames landed on R1"
    );
    assert_eq!(
        handles_a, handles_b,
        "the replicated handle logs agree on both hosts"
    );
    let holdings_a = host_a.store.holdings(&CorrelationId::new("t-ac2")).await;
    let holdings_b = host_b.store.holdings(&CorrelationId::new("t-ac2")).await;
    assert_eq!(holdings_a, holdings_b, "identical heads on both hosts");
    assert_eq!(holdings_a.len(), 1);
    assert_eq!(holdings_a[0].sequence, 3);
}

/// The anti-vacuity control AC2 names verbatim (ledger `:133`): strip
/// `prev_hash` from the signed header and the test above goes red. This twin
/// drives the stripped trace and asserts the feed **cannot** advance past the
/// genesis frame — so the convergence above is attributable to the chain, not
/// to the harness.
#[tokio::test]
async fn ac2_stripping_prev_hash_from_the_signed_header_stops_convergence() {
    let dir = tempfile::tempdir().expect("tempdir");
    let host = receiver(dir.path(), &[11]);
    let signer = signer_of(11);
    let peer = peer_of(11);

    let f1 = sign_gossip(
        &signer,
        "t-ac2-strip",
        1,
        Vec::new(),
        vec![mk_handle(1, &peer, "first finding")],
        vec![],
    );
    // Stripped: sequence 2 arrives with an EMPTY predecessor. The signature is
    // valid — it covers the header as sent — but the feed cannot chain it.
    let f2_stripped = sign_gossip(
        &signer,
        "t-ac2-strip",
        2,
        Vec::new(),
        vec![mk_handle(2, &peer, "second finding")],
        vec![],
    );

    assert!(accepted(&host, f1, &peer).await, "genesis chains");
    assert!(
        not_accepted(&host, f2_stripped, &peer).await,
        "a frame that cannot name its predecessor is refused"
    );
    let holdings = host
        .store
        .holdings(&CorrelationId::new("t-ac2-strip"))
        .await;
    assert_eq!(
        holdings.len(),
        1,
        "the feed stopped at the frame it could chain"
    );
    assert_eq!(holdings[0].sequence, 1);
}

// ── AC3 — a forged summary is rejected, attributable (FR150, gate 3) ───────

#[tokio::test]
async fn ac3_a_forged_summary_never_reaches_the_bundle() {
    let dir = tempfile::tempdir().expect("tempdir");
    let host = receiver(dir.path(), &[11]);
    let signer = signer_of(11);
    let peer = peer_of(11);

    let honest = sign_gossip(
        &signer,
        "t-ac3",
        1,
        Vec::new(),
        vec![mk_handle(1, &peer, "the event bus is NATS")],
        vec![],
    );

    // Positive control: the honest frame verifies and lands.
    rustain::adapters::rap::verify_envelope(&honest, 1_000, None)
        .expect("the honest frame verifies");
    assert!(accepted(&host, honest.clone(), &peer).await);

    // The forgery: same signature, altered summary. The summary is inside the
    // signed body, so any change breaks the content hash and the signature.
    let forged_body: serde_json::Value = serde_json::to_value(TopicGossip {
        refs: vec![mk_handle(1, &peer, "the event bus is Kafka")],
        heads: vec![],
    })
    .expect("encodes");
    // Keep the honest envelope's header and signature; swap only the body.
    let forged = AgentEnvelope::new(
        honest.header.clone(),
        forged_body.clone(),
        honest.signer.clone(),
        honest.signature.clone(),
    );
    let error = rustain::adapters::rap::verify_envelope(&forged, 1_000, None)
        .expect_err("a forged summary must fail verification");
    assert!(
        matches!(
            error,
            rustain::adapters::rap::VerifyError::ContentHashMismatch
                | rustain::adapters::rap::VerifyError::BadSignature
        ),
        "attributable: the refusal names integrity, got {error:?}"
    );
    assert!(
        not_accepted(&host, forged, &peer).await,
        "the forged frame is refused at the front door"
    );
    let handles = host.store.live_handles(1_000).await;
    assert_eq!(handles.len(), 1, "only the honest handle is held");
    assert_eq!(handles[0].summary.as_str(), "the event bus is NATS");
}

// ── AC4 — divergent head → RoomEvent::PeerEquivocated (FR150-a) ─────────────

#[tokio::test]
async fn ac4_a_divergent_advertised_head_is_journaled_and_renders() {
    let dir = tempfile::tempdir().expect("tempdir");
    let workspace = dir.path();
    let host = receiver(workspace, &[11, 12]);
    let peer_a = peer_of(11);
    let peer_b = peer_of(12);
    let signer_a = signer_of(11);
    let signer_b = signer_of(12);

    // A contributes one handle at sequence 1 — this host now HOLDS a head for
    // A's feed.
    let f1 = sign_gossip(
        &signer_a,
        "t-ac4",
        1,
        Vec::new(),
        vec![mk_handle(1, &peer_a, "a finding")],
        vec![],
    );
    assert!(accepted(&host, f1.clone(), &peer_a).await);
    let held = host
        .store
        .holdings(&CorrelationId::new("t-ac4"))
        .await
        .pop()
        .expect("a held head")
        .head;

    // B is a member (capability grant) and advertises a DIFFERENT head for the
    // same (issuer, topic, sequence) — the cross-peer observation a local
    // replay window structurally cannot make.
    host.store
        .grant_membership(&CorrelationId::new("t-ac4"), &peer_b)
        .await;
    // The production producer wiring: journal at this workspace, this host's
    // own signer, and a recording transport for the re-gossip.
    let transport = Arc::new(RecordingTransport {
        gossiped: Mutex::new(Vec::new()),
    });
    assert!(
        host.handler
            .bind_topic_effects(rustain::adapters::rap::TopicEffects {
                workspace: workspace.to_path_buf(),
                transport: transport as Arc<dyn PeerTransport>,
                signer: signer_of(40),
            },)
    );
    let divergent = sign_gossip(
        &signer_b,
        "t-ac4",
        1,
        Vec::new(),
        vec![],
        vec![TopicHead {
            topic: CorrelationId::new("t-ac4"),
            issuer: peer_a.clone(),
            sequence: 1,
            head: vec![0xEEu8; 32],
        }],
    );
    assert!(
        accepted(&host, divergent.clone(), &peer_b).await,
        "the frame itself is admissible — divergence is recorded, not refused"
    );
    assert_ne!(held, vec![0xEEu8; 32], "positive control: the heads differ");

    // The durable record, through the production journal path.
    let journal =
        rustain::infrastructure::subagent::node_journal::NodeJournal::open_workspace(workspace)
            .await
            .expect("open journal");
    let entries = journal.load().await.expect("read journal");
    let equivocations: Vec<_> = entries
        .iter()
        .filter(|entry| {
            matches!(
                &entry.record,
                rustain::domain::models::JournalRecord::Room(RoomEvent::PeerEquivocated { .. })
            )
        })
        .collect();
    assert!(
        !entries.is_empty(),
        "positive control: the journal is not empty"
    );
    assert_eq!(
        equivocations.len(),
        1,
        "exactly one PeerEquivocated record, through the production append path"
    );
    let row = rustain::domain::services::transparency::transparency_row(equivocations[0])
        .expect("the row renders in the transparency log");
    assert_eq!(row.kind, TransparencyKind::PeerEquivocated);
    assert_eq!(row.kind.glyph(), "≠");
    assert_eq!(row.kind.label(), "peer-equivocated");
    // The record names the advertiser and the feed owner, and states the
    // observation — ⛔ never an accusation, never a punishment.
    assert!(row.summary.contains("two different heads"));
    assert!(row.summary.contains("nothing was excluded"));

    // TransparencyFilter::parse — the sixth touch point.
    let filter = TransparencyFilter::parse("kind=peer-equivocated")
        .expect("the filter accepts the new kind");
    assert!(filter.matches(&row), "the filter selects the row");

    // Negative control: an advertisement that MATCHES the held head produces no
    // record.
    let agreeing = sign_gossip(
        &signer_b,
        "t-ac4",
        2,
        entry_hash(&divergent.header).expect("hash"),
        vec![],
        vec![TopicHead {
            topic: CorrelationId::new("t-ac4"),
            issuer: peer_a.clone(),
            sequence: 1,
            head: held,
        }],
    );
    assert!(accepted(&host, agreeing, &peer_b).await);
    let entries = journal.load().await.expect("read journal");
    let count = entries
        .iter()
        .filter(|entry| {
            matches!(
                &entry.record,
                rustain::domain::models::JournalRecord::Room(RoomEvent::PeerEquivocated { .. })
            )
        })
        .count();
    assert_eq!(count, 1, "an agreeing advertisement journals nothing");
}

// ── AC5 — tainted peer context + destructive sink → approval (FR151, gate 4)

struct BashProvider {
    calls: AtomicUsize,
}

#[async_trait]
impl StreamingProvider for BashProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<rustain::domain::models::Message>,
        _options: CompletionOptions,
    ) -> Result<
        std::pin::Pin<Box<dyn futures::Stream<Item = StreamChunk> + Send>>,
        rustain::domain::errors::ProviderError,
    > {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let chunks = if call == 0 {
            vec![
                StreamChunk::ToolUse {
                    id: "bash-1".into(),
                    name: "Bash".into(),
                    input: serde_json::json!({"command": "rm -rf scratch"}),
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::ToolUse,
                },
            ]
        } else {
            vec![StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            }]
        };
        Ok(Box::pin(futures::stream::iter(chunks)))
    }

    async fn abort(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }
    fn provider_id(&self) -> String {
        "bash-script".to_string()
    }
    fn list_models(&self) -> Vec<rustain::domain::models::ModelDescriptor> {
        vec![]
    }
    async fn health_check(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }
    async fn connectivity_probe(
        &self,
    ) -> Result<rustain::domain::ports::ProbeOutcome, rustain::domain::errors::ProviderError> {
        Ok(rustain::domain::ports::ProbeOutcome {
            latency: Duration::ZERO,
        })
    }
}

struct CountingTools {
    executed: AtomicUsize,
}

#[async_trait]
impl ToolSetPort for CountingTools {
    fn available_tools(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "Bash".to_string(),
            description: "run a command".to_string(),
            input_schema: serde_json::json!({}),
            parallel_safe: false,
        }]
    }
    async fn execute(
        &self,
        _tool_name: &str,
        _input: serde_json::Value,
        _cancel: CancellationToken,
    ) -> Result<ToolResult, rustain::domain::errors::ToolError> {
        self.executed.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult {
            tool_use_id: String::new(),
            content: "done".to_string(),
            is_error: false,
        })
    }
}

struct YoloSecurity;

#[async_trait]
impl SecurityPort for YoloSecurity {
    fn check_blocklist(
        &self,
        _command: &str,
    ) -> Result<(), rustain::domain::errors::PermissionError> {
        Ok(())
    }
    fn check_workspace_access(
        &self,
        _path: &std::path::Path,
        _op: rustain::domain::models::FileOperation,
    ) -> Result<rustain::domain::models::PathAccessType, rustain::domain::errors::PermissionError>
    {
        Ok(rustain::domain::models::PathAccessType::Workspace)
    }
    fn current_mode(&self) -> rustain::domain::models::PermissionMode {
        rustain::domain::models::PermissionMode::Yolo
    }
    fn set_mode(&self, _mode: rustain::domain::models::PermissionMode) {}
}

/// Drive the production turn-origination door — `LocalTurnDriver::submit` —
/// with the given context slot, and watch what the scheduler does with the
/// destructive dispatch the scripted provider asks for.
///
/// ⚑ Code-review P13: this keystone previously **copied** the
/// `has_peer_origin` expression into the harness and spawned `run_turn`
/// directly, so deleting the production derivation at `turn_driver.rs` kept
/// the test green — the bypass the story's own Task 4 names as a listed
/// mutant. Now the bit is derived by the production caller, from the bundle
/// the production front door cached, inside the production submit path.
///
/// Returns `(prompted, executed)`: whether the scheduler parked the Bash
/// dispatch on an approval, and whether it executed.
async fn drive_turn_through_submit(
    context: Arc<ArcSwap<Arc<dyn ContextPort>>>,
    tools: Arc<CountingTools>,
    workspace: &Path,
) -> (bool, bool) {
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let security: Arc<dyn SecurityPort> = Arc::new(YoloSecurity);
    let approval = rustain::domain::services::approval_runtime::ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    // `ToolScheduler::new` already returns `Arc<ToolScheduler>`.
    let scheduler = rustain::domain::services::tool_scheduler::ToolScheduler::new(
        security.clone(),
        tools.clone() as Arc<dyn ToolSetPort>,
        approval,
        16,
    );
    let mut transitions = scheduler.subscribe();

    let driver = rustain::infrastructure::runtime::turn_driver::LocalTurnDriver::new(
        Arc::new(BashProvider {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(ArcSwap::from_pointee(
            rustain::domain::models::AppConfig::default(),
        )),
        event_tx,
        security,
        tools.clone() as Arc<dyn ToolSetPort>,
        scheduler,
        Arc::new(rustain::adapters::skill_activation::SkillActivator::new()),
        Arc::new(rustain::adapters::noop::NoOpPersona),
        context,
        Arc::new(ArcSwap::from_pointee(
            None as Option<Arc<dyn rustain::domain::ports::ContextAssemblerPort>>,
        )),
        workspace.to_path_buf(),
        Arc::new(rustain::adapters::filesystem::FileSystemStorage::new(
            workspace.join("sessions"),
        )),
        Arc::new(rustain::adapters::noop::NoOpStorage),
        Arc::new(rustain::domain::services::plan_mode_injector::DefaultPlanInjector::new()),
        Arc::new(rustain::adapters::noop::NoOpUsageLedger) as Arc<dyn UsageLedgerPort>,
        rustain::infrastructure::telemetry::ActiveRatioWindow::new_in_memory(),
    );

    let mut conversation = rustain::domain::models::Conversation {
        id: rustain::domain::models::generate_conversation_id(),
        title: String::new(),
        messages: vec![],
        turns: Vec::new(),
        created_at: 0,
        updated_at: 0,
        last_response_at: None,
        session_id: None,
        usage: None,
        plans: std::collections::HashMap::new(),
        fork_source: None,
        compaction: None,
    };
    let mut streaming = rustain::domain::models::StreamingState::default();
    let mut state = rustain::adapters::tui::state::TuiState::new(120, 24);
    let mut active_turn = None;
    let mut session_manager =
        rustain::domain::models::SessionManager::new(rustain::domain::models::SessionState::Empty);
    let cancel = CancellationToken::new();

    driver
        .submit(
            rustain::infrastructure::runtime::turn_driver::UserSubmission {
                text: "do it".into(),
                images: vec![],
                synthetic: false,
                activation_set: None,
                agent_snapshot: None,
                turn_cancel: cancel.clone(),
            },
            rustain::infrastructure::runtime::turn_driver::TurnViewState {
                conversation: &mut conversation,
                streaming: &mut streaming,
                state: &mut state,
                active_turn: &mut active_turn,
                session_manager: &mut session_manager,
            },
        )
        .await;

    // Watch the scheduler's own transitions: AwaitingApproval is the observable
    // the taint gate produces, and it is the only state this test may treat as
    // "Prompt".
    let mut prompted = false;
    let watcher = tokio::time::timeout(DEADLINE, async {
        while let Ok(transition) = transitions.recv().await {
            if matches!(
                transition.call,
                rustain::domain::models::ToolCall::AwaitingApproval { .. }
            ) {
                prompted = true;
                cancel.cancel();
                break;
            }
            if matches!(
                transition.call,
                rustain::domain::models::ToolCall::Success { .. }
            ) {
                break;
            }
        }
    });
    let _ = watcher.await;
    if let Some(handle) = active_turn.take() {
        let _ = tokio::time::timeout(DEADLINE, handle).await;
    }
    (prompted, tools.executed.load(Ordering::SeqCst) > 0)
}

#[tokio::test]
async fn ac5_peer_origin_context_prompts_a_destructive_dispatch() {
    let dir = tempfile::tempdir().expect("tempdir");
    let host = receiver(dir.path(), &[11]);
    let peer = peer_of(11);
    let frame = sign_gossip(
        &signer_of(11),
        "t-ac5",
        1,
        Vec::new(),
        vec![mk_handle(1, &peer, "teammate context")],
        vec![],
    );
    assert!(accepted(&host, frame, &peer).await);

    // The composite is the production adapter shape — local memory context
    // plus the peer provider — so the turn's bundle is merged, not fabricated.
    let local: Arc<dyn rustain::domain::ports::ContextPort> =
        Arc::new(rustain::adapters::noop::NoOpContext);
    let peer_provider: Arc<dyn rustain::domain::ports::ContextPort> = Arc::new(
        rustain::adapters::peer_context::PeerContextProvider::new(Arc::clone(&host.store))
            .with_now(|| 1_000),
    );
    let composite: Arc<dyn rustain::domain::ports::ContextPort> = Arc::new(
        rustain::adapters::composite_context_adapter::CompositeContextAdapter::new(
            local,
            peer_provider,
        ),
    );

    // ⚑ THE FRONT DOOR, end to end: `LocalTurnDriver::submit` →
    // `inject_assembled_context` → the cached bundle → the production taint
    // derivation → the real scheduler. Nothing in this harness sets the bit.
    let tools = Arc::new(CountingTools {
        executed: AtomicUsize::new(0),
    });
    let (prompted, executed) = drive_turn_through_submit(
        Arc::new(ArcSwap::from_pointee(composite)),
        Arc::clone(&tools),
        dir.path(),
    )
    .await;
    assert!(
        prompted,
        "a destructive dispatch over peer-origin context must stop at approval"
    );
    assert!(
        !executed,
        "the destructive dispatch must not execute while approval is pending"
    );

    // The untainted control: same driver, same provider, same tools, but a
    // context port with no peer entries — the same dispatch runs. Without
    // this, the assertion above proves nothing.
    let untainted: Arc<dyn rustain::domain::ports::ContextPort> =
        Arc::new(rustain::adapters::noop::NoOpContext);
    let tools = Arc::new(CountingTools {
        executed: AtomicUsize::new(0),
    });
    let (prompted, executed) = drive_turn_through_submit(
        Arc::new(ArcSwap::from_pointee(untainted)),
        Arc::clone(&tools),
        dir.path(),
    )
    .await;
    assert!(
        !prompted && executed,
        "positive control: the same destructive dispatch executes with no approval when no peer context is present"
    );
}
/// Code-review D2: the daemon is the process that owns both the filled Topic
/// store and the attach-mode operator turn. This keystone drives its real
/// `DaemonTurnRuntime::drive_turn` path — not `LocalTurnDriver` — and proves
/// the composed bundle reaches `run_turn`'s taint argument on an interactive
/// daemon turn.
#[tokio::test]
async fn review_daemon_interactive_turn_prompts_over_peer_context() {
    let dir = tempfile::tempdir().expect("tempdir");
    let host = receiver(dir.path(), &[11]);
    let peer = peer_of(11);
    let frame = sign_gossip(
        &signer_of(11),
        "t-daemon-taint",
        1,
        Vec::new(),
        vec![mk_handle(1, &peer, "peer-origin architecture context")],
        vec![],
    );
    assert!(accepted(&host, frame, &peer).await);

    let local: Arc<dyn ContextPort> = Arc::new(rustain::adapters::noop::NoOpContext);
    let peer_context: Arc<dyn ContextPort> = Arc::new(
        rustain::adapters::peer_context::PeerContextProvider::new(Arc::clone(&host.store))
            .with_now(|| 1_000),
    );
    let context: Arc<dyn ContextPort> = Arc::new(
        rustain::adapters::composite_context_adapter::CompositeContextAdapter::new(
            local,
            peer_context,
        ),
    );
    let tools = Arc::new(CountingTools {
        executed: AtomicUsize::new(0),
    });
    let security: Arc<dyn SecurityPort> = Arc::new(YoloSecurity);
    let approval = rustain::domain::services::approval_runtime::ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let scheduler = rustain::domain::services::tool_scheduler::ToolScheduler::new(
        security.clone(),
        tools.clone() as Arc<dyn ToolSetPort>,
        approval.clone(),
        16,
    );
    let mut transitions = scheduler.subscribe();
    let runtime = rustain::adapters::daemon::runtime::DaemonTurnRuntime {
        provider: Arc::new(BashProvider {
            calls: AtomicUsize::new(0),
        }),
        app_config: Arc::new(ArcSwap::from_pointee(
            rustain::domain::models::AppConfig::default(),
        )),
        security,
        tools: tools.clone() as Arc<dyn ToolSetPort>,
        tool_scheduler: scheduler,
        persona: Arc::new(rustain::adapters::noop::NoOpPersona),
        context_assembler: Arc::new(ArcSwap::from_pointee(
            None as Option<Arc<dyn rustain::domain::ports::ContextAssemblerPort>>,
        )),
        context: Arc::new(ArcSwap::from_pointee(context)),
        storage: Arc::new(rustain::adapters::noop::NoOpStorage),
        fs_storage: Arc::new(rustain::adapters::filesystem::FileSystemStorage::new(
            dir.path().join("sessions"),
        )),
        usage_ledger: Arc::new(rustain::adapters::noop::NoOpUsageLedger)
            as Arc<dyn UsageLedgerPort>,
        telemetry: rustain::infrastructure::telemetry::ActiveRatioWindow::new_in_memory(),
        plan_injector: Arc::new(
            rustain::domain::services::plan_mode_injector::DefaultPlanInjector::new(),
        ),
        approval,
        workspace: dir.path().to_path_buf(),
        #[cfg(feature = "mcp")]
        mcp_task_runtimes: Vec::new(),
    };
    let mut conversation = rustain::domain::models::Conversation {
        id: "daemon-taint-review".into(),
        ..Default::default()
    };
    let (domain_tx, _domain_rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let handle = runtime
        .drive_turn(
            "do it".into(),
            rustain::domain::models::ChannelKind::Terminal,
            &mut conversation,
            &domain_tx,
            rustain::domain::models::TurnOrigin::Interactive,
            cancel.clone(),
        )
        .await;

    let prompted = tokio::time::timeout(DEADLINE, async {
        while let Ok(transition) = transitions.recv().await {
            if matches!(
                transition.call,
                rustain::domain::models::ToolCall::AwaitingApproval { .. }
            ) {
                cancel.cancel();
                return true;
            }
            if matches!(
                transition.call,
                rustain::domain::models::ToolCall::Success { .. }
            ) {
                return false;
            }
        }
        false
    })
    .await
    .expect("the daemon turn reaches the scheduler");
    let _ = tokio::time::timeout(DEADLINE, handle).await;
    assert!(
        prompted,
        "the daemon's interactive path must prompt over peer-origin context"
    );
    assert_eq!(
        tools.executed.load(Ordering::SeqCst),
        0,
        "the destructive tool must not execute before approval"
    );
}

/// The Vex rule, pinned (17.1b AC8 / 17.2a landmine #4): there is no field a
/// peer can set to clear taint. This is a structural property of the value
/// type, so the assertion is over the type itself — a mutant that adds a
/// peer-supplied `tainted: bool` to `ContextRef` turns this RED.
#[test]
fn ac5_no_peer_supplied_bit_can_clear_taint() {
    let handle = mk_handle(1, &peer_of(31), "anything");
    let serialized = serde_json::to_value(&handle).expect("serializes");
    let object = serialized.as_object().expect("a record");
    assert!(
        !object.contains_key("tainted"),
        "a peer-asserted taint flag must not exist on the handle"
    );
    // And the derivation is from the source variant, which a peer does not
    // choose: the receiver constructs `ContextSource::Peer` itself.
    assert!(ContextSource::Peer(peer_of(31)).is_peer_origin());
}

// ── AC6 — replay and body-drop are both byte-identical (gates 6 and 8) ──────

#[tokio::test]
async fn ac6_replay_folds_identically_and_a_dropped_event_diverges() {
    let dir = tempfile::tempdir().expect("tempdir");
    let workspace = dir.path();
    let journal =
        rustain::infrastructure::subagent::node_journal::NodeJournal::open_workspace(workspace)
            .await
            .expect("open journal");
    journal
        .append_room(RoomEvent::PeerEquivocated {
            peer: Some(peer_of(12)),
            issuer: Some(peer_of(11)),
            topic: "t-ac6".to_owned(),
            sequence: 3,
            held: hex::encode([1u8; 32]),
            advertised: hex::encode([2u8; 32]),
        })
        .await
        .expect("append");

    // Gate 6: the room projection folded twice over the same journal is
    // identical — the new variant included.
    let first = journal.load().await.expect("read");
    let second = journal.load().await.expect("read");
    assert_eq!(first, second, "replay from disk is byte-stable");
    let room_a = rustain::domain::models::OrchestrationRoom::project_for_host(
        journal_room_id(workspace),
        first
            .iter()
            .filter_map(|entry| match &entry.record {
                rustain::domain::models::JournalRecord::Room(event) => Some(event.clone()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        "host-a",
    );
    let room_b = rustain::domain::models::OrchestrationRoom::project_for_host(
        journal_room_id(workspace),
        second
            .iter()
            .filter_map(|entry| match &entry.record {
                rustain::domain::models::JournalRecord::Room(event) => Some(event.clone()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        "host-a",
    );
    assert_eq!(room_a, room_b, "the projection is a pure fold");

    // The anti-vacuity twin (the 17.2a template): a dropped event MUST diverge
    // the projection — the new variant is a fold no-op by decision, so the
    // divergence must be observable in the transparency projection instead.
    let rows_full: Vec<_> = first
        .iter()
        .filter_map(rustain::domain::services::transparency::transparency_row)
        .collect();
    let rows_dropped: Vec<_> = first[..first.len().saturating_sub(1)]
        .iter()
        .filter_map(rustain::domain::services::transparency::transparency_row)
        .collect();
    assert_ne!(
        rows_full.len(),
        rows_dropped.len(),
        "dropping the equivocation record must change the rendered log"
    );
}

fn journal_room_id(workspace: &Path) -> rustain::domain::models::OrchestrationRoomId {
    rustain::domain::models::OrchestrationRoomId::parse(format!(
        "room-{}",
        rustain::infrastructure::paths::workspace_hash(workspace)
    ))
    .expect("workspace hash produces a valid room id")
}

/// Gate 8, as the structural ratchet it is (Rule 4): a peer entry's `content`
/// is the signed summary, and no fetched body can reach it. The mutant — the
/// assembler inlining a body it already fetched — turns this RED.
#[tokio::test]
async fn ac6b_peer_entry_content_is_the_signed_summary_and_nothing_else() {
    let dir = tempfile::tempdir().expect("tempdir");
    let host = receiver(dir.path(), &[11]);
    let peer = peer_of(11);
    let frame = sign_gossip(
        &signer_of(11),
        "t-ac6b",
        1,
        Vec::new(),
        vec![mk_handle(5, &peer, "exactly the summary")],
        vec![],
    );
    assert!(accepted(&host, frame, &peer).await);

    let provider =
        rustain::adapters::peer_context::PeerContextProvider::new(Arc::clone(&host.store))
            .with_now(|| 1_000);
    let bundle = provider
        .assemble("q", ContextBudget::new(4096))
        .await
        .expect("assembles");
    assert_eq!(bundle.entries.len(), 1);
    let entry = &bundle.entries[0];
    assert_eq!(&*entry.content, "exactly the summary");
    // The bound is constructor-enforced; asserting it here pins the assembled
    // side, not just the parse side.
    assert!(
        entry.content.len() <= rustain::domain::models::MAX_CONTEXT_SUMMARY_BYTES,
        "no body — only a bounded summary — may occupy the entry"
    );
}

// ── AC7 — membership gates advertisement (FR150, gate 7) ───────────────────

/// A `PeerTransport` that records what it was asked to gossip, and to whom.
/// The behavioural half of Rule 1 for the re-gossip producer: the fan-out is
/// asserted by what a real call sequence produces, ⛔ never by a source grep.
struct RecordingTransport {
    gossiped: Mutex<Vec<(PeerId, AgentEnvelope<serde_json::Value>)>>,
}

#[async_trait]
impl PeerTransport for RecordingTransport {
    fn local_address(&self) -> Result<rustain::domain::ports::PeerAddress, PeerTransportError> {
        Err(PeerTransportError::Closed)
    }
    async fn dial(&self, _peer: &PeerId) -> Result<(), PeerTransportError> {
        Ok(())
    }
    async fn send_to(
        &self,
        _peer: &PeerId,
        _envelope: AgentEnvelope<serde_json::Value>,
    ) -> Result<rustain::domain::models::FrameVerdict, PeerTransportError> {
        unreachable!("topic gossip never routes through the acknowledged sender")
    }
    fn inbound(&self) -> Result<mpsc::Receiver<InboundFrame>, PeerTransportError> {
        Err(PeerTransportError::InboundUnavailable)
    }
    async fn shutdown(&self) -> Result<(), PeerTransportError> {
        Ok(())
    }
    async fn gossip_topic(
        &self,
        peer: &PeerId,
        envelope: AgentEnvelope<serde_json::Value>,
    ) -> Result<Option<FeedPosition>, PeerTransportError> {
        self.gossiped.lock().await.push((peer.clone(), envelope));
        Ok(None)
    }
}

#[tokio::test]
async fn ac7_a_non_member_is_refused_and_never_advertised_the_hashes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let workspace = dir.path();
    let host = receiver(workspace, &[11, 12, 13]);
    let peer_a = peer_of(11);
    let peer_b = peer_of(12);
    let stranger = peer_of(13);

    // A founds the topic with one handle.
    let f1 = sign_gossip(
        &signer_of(11),
        "t-ac7",
        1,
        Vec::new(),
        vec![mk_handle(1, &peer_a, "a founding handle")],
        vec![],
    );
    assert!(accepted(&host, f1.clone(), &peer_a).await);

    // The non-member's frame into the established topic is refused by
    // authority — nothing is admitted, and the refusal is attributable.
    let stranger_frame = sign_gossip(
        &signer_of(13),
        "t-ac7",
        1,
        Vec::new(),
        vec![mk_handle(9, &stranger, "a stranger's handle")],
        vec![],
    );
    assert!(
        not_accepted(&host, stranger_frame, &stranger).await,
        "a non-member's contribution is refused"
    );
    assert_eq!(
        host.store.live_handles(1_000).await.len(),
        1,
        "nothing of the stranger's reached the log"
    );
    assert!(
        !host
            .store
            .is_member(&CorrelationId::new("t-ac7"), &stranger)
            .await,
        "the stranger holds no grant afterwards either"
    );

    // The advertisement half: grant B membership, then make A's next frame
    // arrive — the fan-out goes to B and to NOBODY else.
    host.store
        .grant_membership(&CorrelationId::new("t-ac7"), &peer_b)
        .await;
    let transport = Arc::new(RecordingTransport {
        gossiped: Mutex::new(Vec::new()),
    });
    // Bind the producer effects — journal at this workspace, the recording
    // transport, this host's own signer (seed 40) — exactly the once-only
    // binding the production listener makes.
    assert!(
        host.handler
            .bind_topic_effects(rustain::adapters::rap::TopicEffects {
                workspace: workspace.to_path_buf(),
                transport: Arc::clone(&transport) as Arc<dyn PeerTransport>,
                signer: signer_of(40),
            },)
    );

    let f2 = sign_gossip(
        &signer_of(11),
        "t-ac7",
        2,
        entry_hash(&f1.header).expect("chain"),
        vec![mk_handle(2, &peer_a, "a second handle")],
        vec![],
    );
    assert!(accepted(&host, f2, &peer_a).await);

    let gossiped = transport.gossiped.lock().await;
    let destinations: Vec<_> = gossiped.iter().map(|(peer, _)| peer.clone()).collect();
    assert_eq!(
        destinations,
        vec![peer_b.clone()],
        "the fan-out reaches exactly the members, and not the origin"
    );
    // The frame carries heads and ⛔ never another peer's handles.
    let body: TopicGossip =
        serde_json::from_value(gossiped[0].1.body.clone()).expect("gossip body");
    assert!(
        body.refs.is_empty(),
        "re-advertising another peer's signed handles is FR155, deferred"
    );
    assert!(!body.heads.is_empty(), "the observation is what travels");
}

// ── Review P6/P9 — front-door transactional and header-boundary pins ───────

/// Code-review P6: a journal failure after divergence detection must leave the
/// Topic store unchanged. The old order admitted B's handle before opening the
/// journal; ingress then rolled back the replay reservation, so a retry
/// double-chained B's feed. The production path now prepares → journals →
/// commits, so the failed frame is a true no-op and a later valid B frame lands
/// once.
#[tokio::test]
async fn review_journal_failure_leaves_topic_admission_uncommitted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let host = receiver(dir.path(), &[11, 12]);
    let peer_a = peer_of(11);
    let peer_b = peer_of(12);
    let f1 = sign_gossip(
        &signer_of(11),
        "t-journal",
        1,
        Vec::new(),
        vec![mk_handle(1, &peer_a, "A's handle")],
        vec![],
    );
    assert!(accepted(&host, f1.clone(), &peer_a).await);
    host.store
        .grant_membership(&CorrelationId::new("t-journal"), &peer_b)
        .await;

    // A regular file cannot be a journal workspace: opening `<file>/.rustain`
    // fails before any append, deterministically.
    let bad_workspace = dir.path().join("not-a-workspace");
    std::fs::write(&bad_workspace, "not a directory").expect("write regular file");
    let transport = Arc::new(RecordingTransport {
        gossiped: Mutex::new(Vec::new()),
    });
    assert!(
        host.handler
            .bind_topic_effects(rustain::adapters::rap::TopicEffects {
                workspace: bad_workspace,
                transport: transport as Arc<dyn PeerTransport>,
                signer: signer_of(40),
            },)
    );
    let divergent = sign_gossip(
        &signer_of(12),
        "t-journal",
        1,
        Vec::new(),
        vec![mk_handle(2, &peer_b, "B's rejected handle")],
        vec![TopicHead {
            topic: CorrelationId::new("t-journal"),
            issuer: peer_a.clone(),
            sequence: 1,
            head: vec![0xEEu8; 32],
        }],
    );
    assert!(
        not_accepted(&host, divergent, &peer_b).await,
        "the journal failure refuses the frame"
    );
    assert_eq!(
        host.store.live_handles(1_000).await.len(),
        1,
        "the failed frame left no B handle in the store"
    );

    // The replay reservation rolled back; a valid B frame at the same feed
    // position can land exactly once, proving the failed attempt did not leave
    // a hidden local feed step behind.
    let retry = sign_gossip(
        &signer_of(12),
        "t-journal",
        1,
        Vec::new(),
        vec![mk_handle(3, &peer_b, "B's accepted handle")],
        vec![],
    );
    assert!(accepted(&host, retry, &peer_b).await);
    assert_eq!(host.store.live_handles(1_000).await.len(), 2);
}

/// Code-review P9: TopicGossip branches before message translation, so its
/// signed header must be held to the message path's identifier and recipient
/// rules *before* the branch. An admitted peer cannot turn a frame-sized
/// correlation id into a permanent Topic key or target another peer's
/// namespace just because the frame carries replication traffic.
#[tokio::test]
async fn review_topic_gossip_obeys_shared_header_limits_and_namespace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let host = receiver(dir.path(), &[11]);
    let signer = signer_of(11);
    let peer = peer_of(11);
    let oversized_topic =
        "x".repeat(rustain::domain::services::transparency::MAX_PEER_ID_BYTES.saturating_add(1));
    let oversized = sign_gossip(
        &signer,
        &oversized_topic,
        1,
        Vec::new(),
        vec![mk_handle(1, &peer, "oversized topic")],
        vec![],
    );
    let refusal = terminal_refusal(&host, oversized, &peer).await;
    assert!(
        refusal.contains("identifier exceeds"),
        "the topic branch applies the shared identifier ceiling: {refusal}"
    );
    assert!(host.store.live_handles(1_000).await.is_empty());

    let victim = peer_of(12);
    let sender =
        AgentId::from_peer_path(&format!("{}/topic-gossip", peer.as_str())).expect("sender");
    let foreign_recipient =
        AgentId::from_peer_path(&format!("{}/topic-gossip-peer", victim.as_str()))
            .expect("foreign recipient");
    let body = serde_json::to_value(TopicGossip {
        refs: vec![mk_handle(2, &peer, "foreign namespace")],
        heads: vec![],
    })
    .expect("encodes");
    let foreign = signer
        .sign(
            sender,
            foreign_recipient,
            CorrelationId::new("t-namespace"),
            MessageKind::TopicGossip,
            1,
            61_000,
            "topic-foreign-recipient".into(),
            Vec::new(),
            body,
        )
        .expect("signs");
    let refusal = terminal_refusal(&host, foreign, &peer).await;
    assert!(
        refusal.contains("outside that peer's namespace"),
        "the topic branch applies the recipient rule: {refusal}"
    );
}
// ── Review D1/D3 + P1/P2 — the production share path, end to end ───────────

/// A transport that delivers gossip frames into a receiving host's production
/// inbound channel — the two halves of a real share, hermetically.
struct ChannelTransport {
    to: mpsc::Sender<InboundFrame>,
    from: PeerId,
}

#[async_trait]
impl PeerTransport for ChannelTransport {
    fn local_address(&self) -> Result<rustain::domain::ports::PeerAddress, PeerTransportError> {
        Err(PeerTransportError::Closed)
    }
    async fn dial(&self, _peer: &PeerId) -> Result<(), PeerTransportError> {
        Ok(())
    }
    async fn send_to(
        &self,
        _peer: &PeerId,
        _envelope: AgentEnvelope<serde_json::Value>,
    ) -> Result<rustain::domain::models::FrameVerdict, PeerTransportError> {
        unreachable!("topic gossip never routes through the acknowledged sender")
    }
    fn inbound(&self) -> Result<mpsc::Receiver<InboundFrame>, PeerTransportError> {
        Err(PeerTransportError::InboundUnavailable)
    }
    async fn shutdown(&self) -> Result<(), PeerTransportError> {
        Ok(())
    }
    async fn gossip_topic(
        &self,
        _peer: &PeerId,
        envelope: AgentEnvelope<serde_json::Value>,
    ) -> Result<Option<FeedPosition>, PeerTransportError> {
        self.to
            .send(InboundFrame {
                envelope,
                peer_id: self.from.clone(),
                responder: None,
            })
            .await
            .map_err(|error| PeerTransportError::Send(error.to_string()))?;
        Ok(None)
    }
}

/// The production share path against a real receiver (review D1/D3, P1/P2):
/// `VerifiedPeerFrameHandler::share_handle` grants the membership, signs the
/// frame, sends it, and records the **accepted** frame — so the sender's head
/// is the same function of the same frame the receiver chained, and ⛔ no
/// `PeerEquivocated` is journaled. The pre-review CLI path fabricated a header
/// for its local head and false-fired the detector on every share; this test
/// is the pin that defect never had.
#[tokio::test]
async fn review_a_share_lands_with_matching_heads_and_no_false_equivocation() {
    let sender_dir = tempfile::tempdir().expect("tempdir");
    let receiver_dir = tempfile::tempdir().expect("tempdir");
    let host = receiver(receiver_dir.path(), &[11]);
    let sender_peer = peer_of(11);
    let receiver_peer = peer_of(61);

    // The RECEIVER journals and re-gossips through the production wiring.
    let recording = Arc::new(RecordingTransport {
        gossiped: Mutex::new(Vec::new()),
    });
    assert!(
        host.handler
            .bind_topic_effects(rustain::adapters::rap::TopicEffects {
                workspace: receiver_dir.path().to_path_buf(),
                transport: recording as Arc<dyn PeerTransport>,
                signer: signer_of(61),
            },)
    );

    // The SENDER: the daemon-side handler shape — one store, effects bound with
    // a transport that delivers into the receiver's inbound channel.
    let node_tree = NodeTree::new();
    let bus = Arc::new(LocalMessageBus::new(
        node_tree.clone(),
        Arc::new(RelationshipDeliveryPolicy),
    )) as Arc<dyn AgentMessageBus>;
    let (domain_tx, _domain_rx) = mpsc::unbounded_channel();
    let sender_store = Arc::new(PeerTopicStore::new());
    let sender_handler = VerifiedPeerFrameHandler::new(
        node_tree,
        Arc::new(ArcSwap::from_pointee(bus)),
        domain_tx,
        Arc::new(AcceptingConsumer),
        Arc::new(AcceptingRecorder),
    )
    .with_topics(Arc::clone(&sender_store))
    .with_now(|| 1_000);
    let channel = ChannelTransport {
        to: host.sender.clone(),
        from: sender_peer.clone(),
    };
    assert!(
        sender_handler.bind_topic_effects(rustain::adapters::rap::TopicEffects {
            workspace: sender_dir.path().to_path_buf(),
            transport: Arc::new(channel) as Arc<dyn PeerTransport>,
            signer: signer_of(11),
        },)
    );

    let hash = ContentHash::from_bytes([7u8; 32]);
    let artifact = rustain::domain::models::EvidenceArtifact {
        id: ArtifactId::from(hash),
        kind: rustain::domain::models::ArtifactKind::Decision,
        producer: AgentId::parse("worker-1").expect("valid agent id"),
        content_hash: hash,
        authority: rustain::domain::models::CapabilityTokenId::nil(),
        provenance: Vec::new(),
        depends_on: Vec::new(),
        review: None,
        host: rustain::domain::models::HostBinding::new("host", "workspace"),
    };
    let topic = CorrelationId::new("t-share");

    sender_handler
        .share_handle(
            &receiver_peer,
            &artifact,
            &topic,
            ContextSummary::new("the event bus decision").expect("bounded"),
        )
        .await
        .expect("the share is written");
    // The channel delivered the frame; drive the receiver's production
    // admission path.
    assert!(
        host.ingress.accept_next().await.is_ok(),
        "the receiver admits the share frame"
    );

    // D1: the share act granted the addressee membership on the sender's store.
    assert!(
        sender_store.is_member(&topic, &receiver_peer).await,
        "the grant is part of the share act"
    );
    // D3: the sender retains its own log.
    let sender_handles = sender_store.live_handles(1_000).await;
    assert_eq!(sender_handles.len(), 1, "the sender keeps its own handle");
    // P1: both hosts compute the head from the SAME frame — they agree, and
    // nothing was journaled as a divergence.
    let receiver_handles = host.store.live_handles(1_000).await;
    assert_eq!(receiver_handles.len(), 1);
    assert_eq!(
        sender_handles, receiver_handles,
        "both hosts hold the same log"
    );
    let sender_holdings = sender_store.holdings(&topic).await;
    let receiver_holdings = host.store.holdings(&topic).await;
    assert_eq!(
        sender_holdings, receiver_holdings,
        "identical heads on both hosts"
    );
    let journal = rustain::infrastructure::subagent::node_journal::NodeJournal::open_workspace(
        receiver_dir.path(),
    )
    .await
    .expect("open journal");
    let entries = journal.load().await.expect("read journal");
    let equivocations = entries
        .iter()
        .filter(|entry| {
            matches!(
                &entry.record,
                rustain::domain::models::JournalRecord::Room(RoomEvent::PeerEquivocated { .. })
            )
        })
        .count();
    assert_eq!(
        equivocations, 0,
        "a clean share must never journal an equivocation"
    );
}

/// Review D4: a heads-only admission changes no advertised holdings, so it
/// triggers ⛔ no re-gossip — the on-change predicate is what breaks the
/// re-advertise cycle a three-member topic would otherwise spin forever.
#[tokio::test]
async fn review_a_heads_only_admission_does_not_regossip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let host = receiver(dir.path(), &[11, 12]);
    let peer_a = peer_of(11);
    let peer_b = peer_of(12);

    // A founds the topic with one handle (a refs admission — this one MAY fan
    // out, and with effects bound it would if there were other members).
    let f1 = sign_gossip(
        &signer_of(11),
        "t-d4",
        1,
        Vec::new(),
        vec![mk_handle(1, &peer_a, "founding handle")],
        vec![],
    );
    assert!(accepted(&host, f1.clone(), &peer_a).await);

    host.store
        .grant_membership(&CorrelationId::new("t-d4"), &peer_b)
        .await;
    let transport = Arc::new(RecordingTransport {
        gossiped: Mutex::new(Vec::new()),
    });
    assert!(
        host.handler
            .bind_topic_effects(rustain::adapters::rap::TopicEffects {
                workspace: dir.path().to_path_buf(),
                transport: Arc::clone(&transport) as Arc<dyn PeerTransport>,
                signer: signer_of(40),
            },)
    );

    // A heads-only frame from a member: observations only, no refs. The
    // admission changes nothing this host would advertise.
    let heads_only = sign_gossip(
        &signer_of(12),
        "t-d4",
        1,
        Vec::new(),
        vec![],
        vec![TopicHead {
            topic: CorrelationId::new("t-d4"),
            issuer: peer_a.clone(),
            sequence: 1,
            head: host
                .store
                .holdings(&CorrelationId::new("t-d4"))
                .await
                .pop()
                .expect("a held head")
                .head,
        }],
    );
    assert!(accepted(&host, heads_only, &peer_b).await);
    assert!(
        transport.gossiped.lock().await.is_empty(),
        "a heads-only admission must not re-advertise unchanged holdings"
    );

    // Positive control: a refs admission DOES fan out to the other member.
    let f2 = sign_gossip(
        &signer_of(11),
        "t-d4",
        2,
        entry_hash(&f1.header).expect("chain"),
        vec![mk_handle(2, &peer_a, "second handle")],
        vec![],
    );
    assert!(accepted(&host, f2, &peer_a).await);
    let gossiped = transport.gossiped.lock().await;
    assert_eq!(
        gossiped.len(),
        1,
        "a refs admission re-gossips to the other member"
    );
    assert_eq!(gossiped[0].0, peer_b);
}

// ── AC8 — the Topic converges with the direct path disabled (NFR72 (b)) ─────

/// Gate 10: two hosts, no direct path at all, a hermetic relay — the Topic
/// converges. Converged means **the replicated handle log and the locally
/// assembled bundle agree on both hosts**; ⛔ it does not mean the far agent
/// consumed the context (on a two-daemon federation run there is no
/// `ContextPort` injection at all — the AC5 scope boundary).
#[tokio::test]
async fn ac8_the_topic_converges_over_a_relay_with_no_direct_path() {
    let (_map, relay_url, relay_server) = iroh::test_utils::run_relay_server()
        .await
        .expect("hermetic relay");
    let canonical = canonical_relay_url(relay_url.as_str()).expect("canonical relay url");

    let server_dir = tempfile::tempdir().expect("tempdir");
    let relay_path = workspace_relay_config_path(server_dir.path());
    std::fs::create_dir_all(relay_path.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &relay_path,
        serde_json::json!({ "mode": "configured", "relays": [canonical] }).to_string(),
    )
    .expect("write relay.json");
    let mode = load_workspace_relay_config(&relay_path).mode();

    // The RECEIVING host: real endpoint, real ingress, real handler, real
    // journal — bound WITHOUT direct paths, which is the `clear_ip_transports`
    // seam (gated on p2p-test-utils AND debug_assertions), so "the direct path
    // is disabled" is a fact, not a race against loopback hole-punching.
    write_allowlist(server_dir.path(), &[62]);
    let server_transport = Arc::new(
        IrohPeerTransport::bind_without_direct_paths(
            signing_key(61).to_bytes(),
            HashMap::new(),
            &mode,
        )
        .await
        .expect("bind the receiving endpoint"),
    );
    let server_signer = signer_of(61);

    let node_tree = NodeTree::new();
    let bus = Arc::new(LocalMessageBus::new(
        node_tree.clone(),
        Arc::new(RelationshipDeliveryPolicy),
    )) as Arc<dyn AgentMessageBus>;
    let (domain_tx, mut domain_rx) = mpsc::unbounded_channel();
    let server_store = Arc::new(PeerTopicStore::new());
    let handler = Arc::new(
        VerifiedPeerFrameHandler::new(
            node_tree,
            Arc::new(ArcSwap::from_pointee(bus)),
            domain_tx,
            Arc::new(AcceptingConsumer),
            Arc::new(AcceptingRecorder),
        )
        .with_topics(Arc::clone(&server_store))
        .with_now(crate_now_ms),
    );
    // ⚑ The production producer wiring, exactly as `compose_p2p_listener` does
    // it: effects are bound after the transport exists.
    assert!(
        handler.bind_topic_effects(rustain::adapters::rap::TopicEffects {
            workspace: server_dir.path().to_path_buf(),
            transport: server_transport.clone() as Arc<dyn PeerTransport>,
            signer: server_signer.clone(),
        }),
        "first binding wins"
    );
    let ingress = Arc::new(
        IrohPeerIngress::new(
            Arc::clone(&server_transport),
            handler,
            server_dir.path().to_path_buf(),
        )
        .expect("ingress"),
    );
    let cancel = CancellationToken::new();
    let listener_cancel = cancel.clone();
    // Held alive for the listener's lifetime, ⛔ never read: a closed domain
    // channel refuses frames for a harness artifact, not a mechanism.
    let domain_keepalive = tokio::spawn(async move { while domain_rx.recv().await.is_some() {} });
    let listener = tokio::spawn(async move {
        let _ = ingress.run(listener_cancel).await;
    });

    // A `Connecting` relay is not reach: gate on a Connected session (18.4c-b
    // trap #2), on BOTH endpoints, read from the endpoints themselves.
    for (name, endpoint) in [("server", &server_transport)] {
        let url = tokio::time::timeout(DEADLINE, endpoint.await_connected_home_relay())
            .await
            .unwrap_or_else(|_| panic!("{name} must establish a Connected relay session"));
        assert_eq!(canonical_relay_url(&url), Some(canonical.clone()));
    }
    let server_address = tokio::time::timeout(DEADLINE, await_relay_address(&server_transport))
        .await
        .expect("the receiver registers with the configured relay");
    let relay_only_address = relay_only(&server_address);

    // The SENDING host: likewise bound without direct paths, with only the
    // relay-only address for the receiver in its dial map.
    let client = IrohPeerTransport::bind_without_direct_paths(
        signing_key(62).to_bytes(),
        HashMap::from([(peer_of(61), relay_only_address)]),
        &mode,
    )
    .await
    .expect("bind the sending endpoint");
    let url = tokio::time::timeout(DEADLINE, client.await_connected_home_relay())
        .await
        .expect("the client establishes a Connected relay session");
    assert_eq!(canonical_relay_url(&url), Some(canonical.clone()));

    // The client records its own contribution through the same production
    // local-head path the `peer share` verb uses, then advertises it.
    let client_store = PeerTopicStore::new();
    let client_signer = signer_of(62);
    let client_peer = peer_of(62);
    let topic = CorrelationId::new("t-ac8");
    let now = crate_now_ms();
    let handle = mk_handle_until(1, &client_peer, "relay-only finding", now + 3_600_000);
    let envelope = sign_gossip_at(
        &client_signer,
        "t-ac8",
        1,
        Vec::new(),
        vec![handle.clone()],
        vec![],
        now + 60_000,
    );
    let local_head = client_store
        .record_local(&envelope.header, &client_peer, vec![handle])
        .await
        .expect("local head");

    client
        .dial(&peer_of(61))
        .await
        .expect("dial over the relay");
    tokio::time::timeout(DEADLINE, client.gossip_topic(&peer_of(61), envelope))
        .await
        .expect("the advertisement is written")
        .expect("the advertisement is accepted by the transport");

    // Convergence: the receiver's log and bundle become the sender's. The wait
    // is on the EVENT (the store filling), with a deadline — ⛔ never a sleep.
    let converged = tokio::time::timeout(DEADLINE, async {
        loop {
            let handles = server_store.live_handles(crate_now_ms()).await;
            if handles.len() == 1 {
                break handles;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the topic converges over the relay within the deadline");

    // The convergence definition from P3, asserted: the replicated handle log
    // and the locally-assembled bundle agree on both hosts.
    let server_provider =
        rustain::adapters::peer_context::PeerContextProvider::new(Arc::clone(&server_store))
            .with_now(crate_now_ms);
    let server_bundle = server_provider
        .assemble("q", ContextBudget::new(4096))
        .await
        .expect("assembles");
    assert_eq!(converged.len(), 1);
    assert_eq!(converged[0].summary.as_str(), "relay-only finding");
    assert_eq!(server_bundle.entries.len(), 1);
    assert!(
        server_bundle.has_peer_origin(),
        "the converged entry carries its taint"
    );

    // Identical heads, computed independently on both hosts.
    let server_holdings = server_store.holdings(&topic).await;
    assert_eq!(server_holdings.len(), 1);
    assert_eq!(
        server_holdings[0].head, local_head.head,
        "identical head hashes on both hosts"
    );

    cancel.cancel();
    let _ = tokio::time::timeout(DEADLINE, listener).await;
    domain_keepalive.abort();
    let _ = client.shutdown().await;
    let _ = server_transport.shutdown().await;
    drop(relay_server);
}

fn crate_now_ms() -> i64 {
    rustain::domain::clock::Clock::wall_now_ms(&rustain::domain::clock::SystemClock::default())
}

/// Await an endpoint's first own-address that names a relay, through the same
/// production address watcher the reach re-publish is built on.
async fn await_relay_address(transport: &IrohPeerTransport) -> rustain::domain::ports::PeerAddress {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let watcher = transport.republish_address_on_change(cancel.clone(), move |address| {
        let _ = tx.send(address);
    });
    tokio::pin!(watcher);
    let deadline = tokio::time::sleep(DEADLINE);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            () = &mut watcher => panic!("the address watcher ended before a relay was established"),
            () = &mut deadline => panic!("no relay address was established within {DEADLINE:?}"),
            received = rx.recv() => {
                let address = received.expect("the watcher sink is alive");
                if describe_reach(&address).iter().any(|rendered| rendered.starts_with("relay ")) {
                    cancel.cancel();
                    return address;
                }
            }
        }
    }
}

/// Keep only the relay entries of an address bundle — a frame that arrives
/// cannot have taken a direct path, because none was offered.
fn relay_only(
    address: &rustain::domain::ports::PeerAddress,
) -> rustain::domain::ports::PeerAddress {
    let mut document: serde_json::Value =
        serde_json::from_slice(address.as_bytes()).expect("bundle decodes");
    let addrs = document["addrs"].as_array().expect("addrs").clone();
    let relays: Vec<serde_json::Value> = addrs
        .into_iter()
        .filter(|entry| entry.get("Relay").is_some())
        .collect();
    assert!(
        !relays.is_empty(),
        "positive control: the address must actually name a relay"
    );
    document["addrs"] = serde_json::Value::Array(relays);
    rustain::domain::ports::PeerAddress::from_bytes(document.to_string().into_bytes())
        .expect("relay-only bundle")
}

// ── Task 7 — the inherited deferred entries ──────────────────────────────────

/// `DF-18-4d-PREADMISSION-WORKER-SLOTS`, closed: a frame from a peer the
/// allowlist does not admit must not mint a worker slot.
///
/// ⚠ This drives **`run()`**, the dispatch loop where the slot map lives — ⛔
/// never `accept_next`, which services one frame itself and would make the
/// assertion vacuous about the very mechanism under test. The observable is the
/// one that cannot be faked: 64 stranger keys, then one admitted peer whose
/// handle must still land in the store.
#[tokio::test]
async fn task7_stranger_keys_cannot_pin_the_peer_worker_slots() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Only seed 11 is admitted.
    let host = receiver(dir.path(), &[11]);

    let cancel = CancellationToken::new();
    let listener = tokio::spawn(Arc::clone(&host.ingress).run(cancel.clone()));

    for stranger_seed in 20..=83u8 {
        let stranger = peer_of(stranger_seed);
        let frame = sign_gossip(
            &signer_of(stranger_seed),
            "t-slots",
            1,
            Vec::new(),
            vec![mk_handle(1, &stranger, "stranger claim")],
            vec![],
        );
        host.sender
            .send(InboundFrame {
                envelope: frame,
                peer_id: stranger,
                responder: None,
            })
            .await
            .expect("inbound channel open");
    }

    // The admitted peer's frame, behind all 64 strangers. Had each stranger
    // pinned a worker slot, every slot would be gone and this frame would be
    // dropped with a warning — which is the RED this test proves by going GREEN.
    let peer = peer_of(11);
    let admitted = sign_gossip(
        &signer_of(11),
        "t-slots",
        1,
        Vec::new(),
        vec![mk_handle(1, &peer, "admitted claim")],
        vec![],
    );
    host.sender
        .send(InboundFrame {
            envelope: admitted,
            peer_id: peer.clone(),
            responder: None,
        })
        .await
        .expect("inbound channel open");

    let landed = tokio::time::timeout(DEADLINE, async {
        loop {
            if host.store.live_handles(1_000).await.len() == 1 {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or(false);

    cancel.cancel();
    let _ = tokio::time::timeout(DEADLINE, listener).await;
    assert!(
        landed,
        "the admitted peer must be serviced after 64 strangers knocked"
    );
}

/// `DF-18-4d-RECIPIENT-NAMESPACE`, closed: a peer may not name a recipient
/// outside its own identity namespace.
#[tokio::test]
async fn task7_a_peer_cannot_address_another_peers_namespace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let host = receiver(dir.path(), &[11]);
    let signer = signer_of(11);
    let peer = peer_of(11);
    let victim = peer_of(12);

    // A PeerMessage frame (kind checked by the translate seam) whose recipient
    // is rooted at ANOTHER peer's id. The sender is bound correctly — only the
    // recipient escapes.
    let sender = AgentId::from_peer_path(&format!("{}/agent", peer.as_str())).expect("sender");
    let envelope = signer
        .sign(
            sender,
            AgentId::from_peer_path(&format!("{}/agent", victim.as_str()))
                .expect("victim-rooted recipient"),
            CorrelationId::new("t-ns"),
            MessageKind::PeerMessage,
            1,
            1_000 + 60_000,
            "ns-check".to_owned(),
            Vec::new(),
            serde_json::json!({"msg": "hello"}),
        )
        .expect("signs — the sign seam cannot see the recipient rule");

    let positive = AgentId::from_peer_path(&format!("{}/agent", peer.as_str())).expect("recipient");
    assert!(rustain::adapters::rap::recipient_rooted_at(
        &positive, &peer
    ));
    let escape = AgentId::from_peer_path(&format!("{}/agent", victim.as_str())).expect("escape");
    assert!(!rustain::adapters::rap::recipient_rooted_at(&escape, &peer));

    // ⚑ Attributable, ⛔ not merely refused: a frame refused for an unrelated
    // harness reason would satisfy a bare `not_accepted` — the false-green the
    // surviving-mutant sweep caught here.
    let refusal = terminal_refusal(&host, envelope, &peer).await;
    assert!(
        refusal.contains("outside that peer's namespace"),
        "the refusal must name the namespace rule, got: {refusal}"
    );
}

/// `DF-18-4a-SEQUENCE-CEILING` (the 18.4d review's unnamed wedge, now with an
/// id): a frame at `u64::MAX` must never reach the feed, because committing it
/// saturates `highest.saturating_add(1)` and wedges every later frame forever.
///
/// ⚠ Layering, stated: the sign seam refuses to *produce* the header (the test
/// below), so a correctly built frame can no longer carry the wedge. What this
/// front-door test pins is the other half — a **hand-crafted** frame carrying
/// the value dies at the door and leaves the feed untouched. The dedicated
/// `SequenceOutOfRange` arm inside `ReplayWindow::validate_candidate` is pinned
/// by the wire.rs unit test, which can reach the private guard.
#[tokio::test]
async fn task7_a_frame_at_u64_max_cannot_wedge_the_feed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let host = receiver(dir.path(), &[11]);
    let signer = signer_of(11);
    let peer = peer_of(11);

    // Positive control: a legal first frame lands.
    let first = sign_gossip(
        &signer,
        "t-max",
        1,
        Vec::new(),
        vec![mk_handle(1, &peer, "genesis")],
        vec![],
    );
    assert!(accepted(&host, first.clone(), &peer).await);

    // The wedge attempt: a legal frame whose header is rewritten to u64::MAX
    // after signing — the only shape such a frame can take now that the sign
    // seam refuses to mint it.
    let mut crafted = sign_gossip(
        &signer,
        "t-max",
        2,
        entry_hash(&first.header).expect("hash"),
        vec![mk_handle(2, &peer, "wedging claim")],
        vec![],
    );
    crafted.header.sequence = u64::MAX;
    assert!(
        not_accepted(&host, crafted, &peer).await,
        "a frame carrying u64::MAX never reaches the feed"
    );

    // And the feed still advances: the wedge attempt changed nothing.
    let next = sign_gossip(
        &signer,
        "t-max",
        2,
        entry_hash(&first.header).expect("hash"),
        vec![mk_handle(3, &peer, "after the wedge")],
        vec![],
    );
    assert!(
        accepted(&host, next, &peer).await,
        "the feed must still advance after the refused wedge"
    );
}

/// The signing half of the same ruling: `sign_envelope` now refuses to produce
/// the malformed header at all.
#[test]
fn task7_the_sign_seam_refuses_an_out_of_range_sequence_and_a_bad_prev_hash() {
    let signer = signer_of(41);
    let peer = peer_of(41);
    let sender = AgentId::from_peer_path(&format!("{}/agent", peer.as_str())).expect("sender");
    let recipient = AgentId::from_peer_path(&format!("{}/peer", peer.as_str())).expect("recipient");
    let sign = |sequence: u64, prev_hash: Vec<u8>| {
        signer.sign(
            sender.clone(),
            recipient.clone(),
            CorrelationId::new("t-sign"),
            MessageKind::TopicGossip,
            sequence,
            1_000,
            format!("n-{sequence}"),
            prev_hash,
            serde_json::json!({"heads": [], "refs": []}),
        )
    };
    assert!(matches!(
        sign(u64::MAX, Vec::new()),
        Err(rustain::adapters::rap::VerifyError::SequenceOutOfRange { .. })
    ));
    assert!(matches!(
        sign(0, Vec::new()),
        Err(rustain::adapters::rap::VerifyError::SequenceOutOfRange { .. })
    ));
    assert!(matches!(
        sign(1, vec![0u8; 31]),
        Err(rustain::adapters::rap::VerifyError::PrevHashWidth { .. })
    ));
    assert!(
        sign(1, Vec::new()).is_ok(),
        "positive control: a legal frame signs"
    );
}

// ── Task 8 — the lane registration ratchet (per-story copy) ─────────────────

/// A target wired into nothing compiles, passes locally, and is never run by
/// CI — a false green, not a workaround. This story's copy; ⛔ sibling stories'
/// copies do not cover this file.
#[test]
fn task8_conformance_18_4a_p2p_topic_is_named_by_a_lane_that_runs_it() {
    let ci = source(".github/workflows/ci.yml");
    assert!(
        ci.contains("--test conformance_18_4a_p2p_topic"),
        "this target must be named in the p2p lane"
    );
}

// ── Task 9 — the wording ceiling, as a TEST (ruling P16) ────────────────────

#[test]
fn task9_every_string_this_story_ships_stays_within_the_wording_ceiling() {
    let owned_modules = [
        "src/domain/models/context_ref.rs",
        "src/domain/services/topic.rs",
        "src/adapters/rap/topic.rs",
        "src/adapters/peer_context.rs",
        "src/adapters/composite_context_adapter.rs",
        "src/adapters/cli/peer/share.rs",
    ];
    let mut strings = Vec::new();
    for relative in owned_modules {
        let contents = source(relative);
        let production = contents.split("#[cfg(test)]").next().unwrap_or(&contents);
        strings.extend(rust_string_literals(production));
    }
    assert!(
        strings.len() >= 10,
        "positive control: the scan found the story's strings"
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
            "secure",
            "tier",
            "enterprise",
            "posture",
            "cannot read",
            "ciphertext only",
        ] {
            assert!(
                !lowered.contains(forbidden),
                "{forbidden:?} must not appear in an operator-facing string this story ships: {value:?}"
            );
        }
    }
}

/// Minimal Rust string-literal extractor, copied from the p2p harness so the
/// ceiling scan reads literals and ⛔ never comments.
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
        if bytes[index] == b'"' {
            let content_start = index + 1;
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
            index += 1;
            continue;
        }
        index += 1;
    }
    strings
}
