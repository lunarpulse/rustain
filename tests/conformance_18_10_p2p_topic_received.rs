//! Story 18.10: receiver receipts through `share_handle` and the production
//! ingress `accept_next` path. No receipt is seeded or store admission bypassed.
#![cfg(all(feature = "p2p", feature = "p2p-test-utils"))]

use arc_swap::ArcSwap;
use async_trait::async_trait;
use base64::Engine as _;
use rustain::adapters::iroh::IrohPeerIngress;
use rustain::adapters::rap::{
    AgentSigner, PeerTopicStore, TopicEffects, TopicGossip, VerifiedPeerConsent,
    VerifiedPeerConsumer, VerifiedPeerFrameHandler, entry_hash,
};
use rustain::domain::models::{
    AgentEnvelope, AgentId, ArtifactId, ContentHash, ContextRef, ContextRefProvenance,
    ContextSummary, CorrelationId, FeedPosition, JournalEntry, JournalRecord, MessageKind, PeerId,
    RoomEvent, TopicHead,
};
use rustain::domain::ports::{
    AgentMessageBus, InboundFrame, PeerDeliveryRecord, PeerInteractionRecorder, PeerTransport,
    PeerTransportError, RelationshipDeliveryPolicy,
};
use rustain::domain::services::transparency::{
    TransparencyFilter, TransparencyKind, transparency_row,
};
use rustain::infrastructure::subagent::node_journal::NodeJournal;
use rustain::infrastructure::subagent::{LocalMessageBus, NodeTree};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

fn signer(seed: u8) -> AgentSigner {
    AgentSigner::from_signing_key(ed25519_dalek::SigningKey::from_bytes(&[seed; 32]))
}
fn peer(seed: u8) -> PeerId {
    signer(seed).identity().peer_id.clone()
}
fn mk_handle(content: u8, issuer: &PeerId) -> ContextRef {
    let hash = ContentHash::from_bytes([content; 32]);
    ContextRef {
        artifact: ArtifactId::from(hash),
        content_hash: hash,
        producer: AgentId::parse("producer").unwrap(),
        issuer: issuer.clone(),
        summary: ContextSummary::new("event bus decision").unwrap(),
        provenance: ContextRefProvenance::Authored,
        not_after: 61_000,
    }
}
fn sign_gossip(
    seed: u8,
    topic: &str,
    sequence: u64,
    previous: Vec<u8>,
    refs: Vec<ContextRef>,
    heads: Vec<TopicHead>,
) -> AgentEnvelope<serde_json::Value> {
    let signer = signer(seed);
    let pid = signer.identity().peer_id.as_str();
    signer
        .sign(
            AgentId::from_peer_path(&format!("{pid}/topic-gossip")).unwrap(),
            AgentId::from_peer_path(&format!("{pid}/topic-gossip-peer")).unwrap(),
            CorrelationId::new(topic),
            MessageKind::TopicGossip,
            String::new(),
            sequence,
            61_000,
            format!("{topic}-{sequence}"),
            previous,
            serde_json::to_value(TopicGossip { refs, heads }).unwrap(),
        )
        .unwrap()
}
struct AcceptingConsumer;
#[async_trait]
impl VerifiedPeerConsumer for AcceptingConsumer {
    async fn consent(
        &self,
        _: &AgentId,
        _: &rustain::domain::models::AgentMessage,
        _: &PeerId,
    ) -> Result<VerifiedPeerConsent, String> {
        Ok(VerifiedPeerConsent::Accept)
    }
    async fn ingest(
        &self,
        _: &AgentId,
        _: rustain::domain::models::AgentMessage,
        _: &PeerId,
    ) -> Result<(), String> {
        Ok(())
    }
}
struct AcceptingRecorder;
#[async_trait]
impl PeerInteractionRecorder for AcceptingRecorder {
    async fn record_peer_delivery(&self, _: PeerDeliveryRecord) -> Result<(), String> {
        Ok(())
    }
    async fn record_transport_refusal(
        &self,
        _: rustain::domain::ports::TransportRefusalRecord,
    ) -> Result<(), String> {
        Ok(())
    }
}
fn handler() -> (
    VerifiedPeerFrameHandler,
    mpsc::UnboundedReceiver<rustain::domain::events::AppEvent>,
) {
    let tree = NodeTree::new();
    let bus = Arc::new(LocalMessageBus::new(
        tree.clone(),
        Arc::new(RelationshipDeliveryPolicy),
    )) as Arc<dyn AgentMessageBus>;
    let (tx, rx) = mpsc::unbounded_channel();
    (
        VerifiedPeerFrameHandler::new(
            tree,
            Arc::new(ArcSwap::from_pointee(bus)),
            tx,
            Arc::new(AcceptingConsumer),
            Arc::new(AcceptingRecorder),
        )
        .with_now(|| 1_000),
        rx,
    )
}
struct Receiver {
    ingress: IrohPeerIngress,
    sender: mpsc::Sender<InboundFrame>,
    store: Arc<PeerTopicStore>,
    _events: mpsc::UnboundedReceiver<rustain::domain::events::AppEvent>,
}
fn receiver(workspace: &Path, journal_root: &Path) -> Receiver {
    std::fs::create_dir_all(workspace.join(".rustain")).unwrap();
    let agents: serde_json::Map<String, serde_json::Value> = [11, 12].into_iter().map(|seed| {
        let key = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
        (format!("client-{seed}"), serde_json::json!({"pinnedKey": {"alg":"EdDSA",
            "x": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key.verifying_key().to_bytes())}}))
    }).collect();
    std::fs::write(
        workspace.join(".rustain/p2p.json"),
        serde_json::to_vec(&serde_json::json!({"listen":true,"agents":agents})).unwrap(),
    )
    .unwrap();
    let (handler, events) = handler();
    let store = Arc::new(PeerTopicStore::new());
    let handler = Arc::new(handler.with_topics(store.clone()));
    assert!(handler.bind_topic_effects(TopicEffects {
        workspace: journal_root.to_path_buf(),
        transport: Arc::new(RecordingTransport::default()),
        signer: signer(61)
    }));
    let (sender, inbound) = mpsc::channel(16);
    Receiver {
        ingress: IrohPeerIngress::with_inbound(inbound, handler, workspace.to_path_buf(), || 1_000),
        sender,
        store,
        _events: events,
    }
}
async fn deliver(
    host: &Receiver,
    frame: AgentEnvelope<serde_json::Value>,
    seed: u8,
) -> Result<(), String> {
    host.sender
        .send(InboundFrame {
            envelope: frame,
            peer_id: peer(seed),
            responder: None,
        })
        .await
        .unwrap();
    host.ingress
        .accept_next()
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}
#[derive(Default)]
struct RecordingTransport {
    frames: Mutex<Vec<AgentEnvelope<serde_json::Value>>>,
}
#[async_trait]
impl PeerTransport for RecordingTransport {
    fn local_address(&self) -> Result<rustain::domain::ports::PeerAddress, PeerTransportError> {
        Err(PeerTransportError::Closed)
    }
    async fn dial(&self, _: &PeerId) -> Result<(), PeerTransportError> {
        Ok(())
    }
    async fn send_to(
        &self,
        _: &PeerId,
        _: AgentEnvelope<serde_json::Value>,
    ) -> Result<rustain::domain::models::FrameVerdict, PeerTransportError> {
        unreachable!("topic path")
    }
    fn inbound(&self) -> Result<mpsc::Receiver<InboundFrame>, PeerTransportError> {
        Err(PeerTransportError::InboundUnavailable)
    }
    async fn shutdown(&self) -> Result<(), PeerTransportError> {
        Ok(())
    }
    async fn gossip_topic(
        &self,
        _: &PeerId,
        frame: AgentEnvelope<serde_json::Value>,
    ) -> Result<Option<FeedPosition>, PeerTransportError> {
        self.frames.lock().await.push(frame);
        Ok(None)
    }
}
struct ChannelTransport {
    to: mpsc::Sender<InboundFrame>,
}
#[async_trait]
impl PeerTransport for ChannelTransport {
    fn local_address(&self) -> Result<rustain::domain::ports::PeerAddress, PeerTransportError> {
        Err(PeerTransportError::Closed)
    }
    async fn dial(&self, _: &PeerId) -> Result<(), PeerTransportError> {
        Ok(())
    }
    async fn send_to(
        &self,
        _: &PeerId,
        _: AgentEnvelope<serde_json::Value>,
    ) -> Result<rustain::domain::models::FrameVerdict, PeerTransportError> {
        unreachable!("topic path")
    }
    fn inbound(&self) -> Result<mpsc::Receiver<InboundFrame>, PeerTransportError> {
        Err(PeerTransportError::InboundUnavailable)
    }
    async fn shutdown(&self) -> Result<(), PeerTransportError> {
        Ok(())
    }
    async fn gossip_topic(
        &self,
        _: &PeerId,
        frame: AgentEnvelope<serde_json::Value>,
    ) -> Result<Option<FeedPosition>, PeerTransportError> {
        self.to
            .send(InboundFrame {
                envelope: frame,
                peer_id: peer(11),
                responder: None,
            })
            .await
            .map_err(|error| PeerTransportError::Send(error.to_string()))?;
        Ok(None)
    }
}
async fn entries(workspace: &Path) -> Vec<JournalEntry> {
    NodeJournal::open_workspace(workspace)
        .await
        .unwrap()
        .load()
        .await
        .unwrap()
}
fn receipts(entries: &[JournalEntry]) -> Vec<&JournalEntry> {
    entries
        .iter()
        .filter(|entry| {
            matches!(
                entry.record,
                JournalRecord::Room(RoomEvent::PeerTopicHandlesReceived { .. })
            )
        })
        .collect()
}
fn artifact(content: u8) -> rustain::domain::models::EvidenceArtifact {
    let hash = ContentHash::from_bytes([content; 32]);
    rustain::domain::models::EvidenceArtifact {
        id: ArtifactId::from(hash),
        kind: rustain::domain::models::ArtifactKind::Decision,
        producer: AgentId::parse("producer").unwrap(),
        content_hash: hash,
        authority: rustain::domain::models::CapabilityTokenId::nil(),
        provenance: vec![],
        depends_on: vec![],
        review: None,
        host: rustain::domain::models::HostBinding::new("host", "workspace"),
    }
}
async fn share(host: &Receiver, sender_root: &Path, count: u8) {
    let (sender, _events) = handler();
    assert!(sender.bind_topic_effects(TopicEffects {
        workspace: sender_root.to_path_buf(),
        transport: Arc::new(ChannelTransport {
            to: host.sender.clone()
        }),
        signer: signer(11)
    }));
    for content in 1..=count {
        sender
            .share_handle(
                &peer(61),
                &artifact(content),
                &CorrelationId::new("arch-review"),
                ContextSummary::new("event bus decision").unwrap(),
            )
            .await
            .unwrap();
        host.ingress.accept_next().await.expect("share admitted");
    }
}

#[tokio::test]
async fn a_clean_share_journals_one_topic_received_row() {
    let root = tempfile::tempdir().unwrap();
    let sender_root = tempfile::tempdir().unwrap();
    let host = receiver(root.path(), root.path());
    assert!(receipts(&entries(root.path()).await).is_empty());
    share(&host, sender_root.path(), 2).await;
    let all = entries(root.path()).await;
    let rows = receipts(&all);
    assert_eq!(rows.len(), 2);
    for (index, entry) in rows.iter().enumerate() {
        assert!(
            matches!(&entry.record, JournalRecord::Room(RoomEvent::PeerTopicHandlesReceived {
            peer: Some(sender), topic, sequence, artifacts
        }) if sender == &peer(11) && topic == "arch-review" && *sequence == index as u64 + 1
            && artifacts == &vec![artifact(index as u8 + 1).id.to_string()])
        );
        let row = transparency_row(entry).expect("receipt renders");
        assert_eq!(row.kind, TransparencyKind::PeerTopicReceived);
        assert!(
            TransparencyFilter::parse("kind=topic-received")
                .unwrap()
                .matches(&row)
        );
        assert!(!row.summary.contains("event bus decision"));
    }
    assert!(!all.iter().any(|entry| matches!(
        entry.record,
        JournalRecord::Room(RoomEvent::PeerEquivocated { .. })
    )));
    // A fresh signer's feed can carry two handles in deliberately reverse lexical order.
    let refs = vec![mk_handle(9, &peer(12)), mk_handle(3, &peer(12))];
    let ids: Vec<_> = refs
        .iter()
        .map(|handle| handle.artifact.to_string())
        .collect();
    let frame = sign_gossip(12, "two-handles", 1, vec![], refs, vec![]);
    deliver(&host, frame, 12).await.unwrap();
    let all = entries(root.path()).await;
    assert_eq!(receipts(&all).len(), 3);
    assert!(
        matches!(&all.last().unwrap().record, JournalRecord::Room(RoomEvent::PeerTopicHandlesReceived {
        sequence: 1, artifacts, ..
    }) if artifacts == &ids)
    );
}

#[tokio::test]
async fn heads_only_and_refused_frames_write_no_row() {
    let root = tempfile::tempdir().unwrap();
    let host = receiver(root.path(), root.path());
    let first = sign_gossip(
        11,
        "topic",
        1,
        vec![],
        vec![mk_handle(1, &peer(11))],
        vec![],
    );
    deliver(&host, first.clone(), 11).await.unwrap();
    let held = host
        .store
        .holdings(&CorrelationId::new("topic"))
        .await
        .pop()
        .unwrap();
    let agreeing = sign_gossip(
        11,
        "topic",
        2,
        entry_hash(&first.header).unwrap(),
        vec![],
        vec![held.clone()],
    );
    deliver(&host, agreeing.clone(), 11).await.unwrap();
    assert_eq!(entries(root.path()).await.len(), 1);
    let divergent = sign_gossip(
        11,
        "topic",
        3,
        entry_hash(&agreeing.header).unwrap(),
        vec![],
        vec![TopicHead {
            head: vec![0xee; 32],
            ..held
        }],
    );
    deliver(&host, divergent, 11).await.unwrap();
    let all = entries(root.path()).await;
    assert_eq!(receipts(&all).len(), 1);
    assert_eq!(
        all.iter()
            .filter(|entry| matches!(
                entry.record,
                JournalRecord::Room(RoomEvent::PeerEquivocated { .. })
            ))
            .count(),
        1
    );
    let mut expired = mk_handle(2, &peer(12));
    expired.not_after = 999;
    let oversized = "x".repeat(rustain::domain::services::transparency::MAX_PEER_ID_BYTES + 1);
    let mut cases = vec![
        (
            sign_gossip(
                12,
                "topic",
                1,
                vec![],
                vec![mk_handle(2, &peer(12))],
                vec![],
            ),
            "holds no membership grant",
        ),
        (
            sign_gossip(12, "expired", 1, vec![], vec![expired], vec![]),
            "expired",
        ),
        (
            sign_gossip(
                12,
                "issuer",
                1,
                vec![],
                vec![mk_handle(2, &peer(11))],
                vec![],
            ),
            "issuer",
        ),
        (
            sign_gossip(
                12,
                &oversized,
                1,
                vec![],
                vec![mk_handle(2, &peer(12))],
                vec![],
            ),
            "identifier exceeds",
        ),
    ];
    let sender = signer(12);
    let foreign = sender
        .sign(
            AgentId::from_peer_path(&format!("{}/topic-gossip", peer(12))).unwrap(),
            AgentId::from_peer_path(&format!("{}/topic-gossip-peer", peer(11))).unwrap(),
            CorrelationId::new("foreign"),
            MessageKind::TopicGossip,
            String::new(),
            1,
            61_000,
            "foreign-recipient".to_owned(),
            vec![],
            serde_json::to_value(TopicGossip {
                refs: vec![mk_handle(2, &peer(12))],
                heads: vec![],
            })
            .unwrap(),
        )
        .unwrap();
    cases.push((foreign, "outside that peer's namespace"));
    let before = host.store.live_handles(1_000).await;
    for (frame, rule) in cases {
        let refusal = deliver(&host, frame, 12).await.expect_err("refused");
        assert!(refusal.contains(rule), "{rule}: {refusal}");
        assert_eq!(host.store.live_handles(1_000).await, before);
        assert_eq!(receipts(&entries(root.path()).await).len(), 1);
    }
}

#[tokio::test]
async fn a_journal_failure_refuses_and_a_repaired_root_lands_once() {
    let root = tempfile::tempdir().unwrap();
    let bad = root.path().join("journal-root");
    std::fs::write(&bad, "not a directory").unwrap();
    let host = receiver(root.path(), &bad);
    let first = sign_gossip(
        11,
        "topic",
        1,
        vec![],
        vec![mk_handle(1, &peer(11))],
        vec![],
    );
    let refusal = deliver(&host, first.clone(), 11).await.unwrap_err();
    assert!(refusal.contains("could not be recorded"), "{refusal}");
    assert!(host.store.live_handles(1_000).await.is_empty());
    assert!(!bad.join(".rustain/rooms").exists());
    std::fs::remove_file(&bad).unwrap();
    std::fs::create_dir_all(&bad).unwrap();
    assert!(entries(&bad).await.is_empty());
    deliver(&host, first, 11).await.unwrap();
    assert_eq!(receipts(&entries(&bad).await).len(), 1);
    host.store
        .grant_membership(&CorrelationId::new("topic"), &peer(12))
        .await;
    let divergent = sign_gossip(
        12,
        "topic",
        1,
        vec![],
        vec![mk_handle(2, &peer(12))],
        vec![TopicHead {
            topic: CorrelationId::new("topic"),
            issuer: peer(11),
            sequence: 1,
            head: vec![0xee; 32],
        }],
    );
    deliver(&host, divergent, 12).await.unwrap();
    let all = entries(&bad).await;
    assert_eq!(all.len(), 3);
    assert!(matches!(
        all[1].record,
        JournalRecord::Room(RoomEvent::PeerEquivocated { .. })
    ));
    assert!(matches!(
        all[2].record,
        JournalRecord::Room(RoomEvent::PeerTopicHandlesReceived { .. })
    ));
    assert_eq!(all[2].seq, all[1].seq + 1);
    assert_eq!(host.store.live_handles(1_000).await.len(), 2);
}
fn cli_rows(workspace: &Path, data: &Path, config: &Path) -> Vec<serde_json::Value> {
    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("rustain"))
        .args(["team", "log", "--filter", "kind=topic-received", "--json"])
        .current_dir(workspace)
        .env("RUSTAIN_DATA_DIR", data)
        .env("RUSTAIN_CONFIG_DIR", config)
        .output()
        .expect("real team log binary");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["rows"]
        .as_array()
        .unwrap()
        .clone()
}
#[tokio::test]
async fn team_log_cli_reads_the_receivers_row() {
    let root = tempfile::tempdir().unwrap();
    let sender = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let host = receiver(root.path(), root.path());
    assert!(cli_rows(root.path(), data.path(), config.path()).is_empty());
    share(&host, sender.path(), 1).await;
    let rows = cli_rows(root.path(), data.path(), config.path());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["kind"], "topic-received");
    assert_eq!(rows[0]["peer"], peer(11).as_str());
    assert_eq!(rows[0]["task"], "arch-review");
    assert!(
        rows[0]["summary"]
            .as_str()
            .unwrap()
            .contains(&artifact(1).id.to_string())
    );
}

/// Review P1: prepare → journal → commit runs under the store's admission gate,
/// so another writer cannot change the pre-state between the decision and its
/// commit. The paused clock only auto-advances once the frame is parked.
#[tokio::test(start_paused = true)]
async fn a_topic_admission_waits_for_the_store_admission_gate() {
    let root = tempfile::tempdir().unwrap();
    let host = receiver(root.path(), root.path());
    let frame = sign_gossip(
        11,
        "topic",
        1,
        vec![],
        vec![mk_handle(1, &peer(11))],
        vec![],
    );
    let gate = host.store.admission_gate().await;
    let admission = deliver(&host, frame, 11);
    tokio::pin!(admission);
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(1), &mut admission)
            .await
            .is_err(),
        "a topic frame must not prepare or commit while another admission holds the gate"
    );
    assert!(host.store.live_handles(1_000).await.is_empty());
    assert!(receipts(&entries(root.path()).await).is_empty());
    drop(gate);
    admission.await.unwrap();
    assert_eq!(host.store.live_handles(1_000).await.len(), 1);
    assert_eq!(receipts(&entries(root.path()).await).len(), 1);
}
