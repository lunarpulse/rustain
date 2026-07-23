//! Story 17.1a — AC2 / DD-2 / DD-7: the peer-tier `Attach` and the signed
//! `PeerEnvelope` are NEW `ClientFrame` variants.
//!
//! The existing `protocol::tests::every_client_frame_round_trips_through_json`
//! sample omits both — it carries only `tier: TrustedLocal` and no
//! `PeerEnvelope` — so the negotiated peer tier and the signed envelope have
//! **zero** round-trip coverage. This pins:
//!
//! 1. `Attach { tier: Peer }` survives JSON with the tier preserved (DD-7
//!    negotiation is meaningful only if the tier bit is stable on the wire).
//! 2. A signed `PeerEnvelope` survives both serde **and** the real
//!    length-prefixed framing, preserving the exact `signature`,
//!    `content_hash`, and `sequence` a peer must re-verify before dispatch
//!    (AC3). A regression in the `Box<AgentEnvelope<Value>>` serde shape, the
//!    camelCase rename, or the framing of the new variant turns these RED.

use rustain::adapters::daemon::protocol::{
    ClientFrame, ConnectionTier, PROTOCOL_VERSION, read_frame, write_frame,
};
use rustain::adapters::rap::AgentSigner;
use rustain::domain::models::{AgentEnvelope, AgentId, CorrelationId, Ed25519Sig, MessageKind};

fn test_signer() -> AgentSigner {
    AgentSigner::from_signing_key(ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]))
}

fn sample_peer_envelope() -> AgentEnvelope<serde_json::Value> {
    // Fixed seed → deterministic key; no /dev/urandom, no wall clock.
    let signer = AgentSigner::from_signing_key(ed25519_dalek::SigningKey::from_bytes(&[3u8; 32]));
    let sender = AgentId::from_peer_path(&format!("{}/peer-a", signer.identity().peer_id.as_str()))
        .expect("peer-rooted sender");
    signer
        .sign(
            sender,
            AgentId::parse("peer-b").expect("valid recipient"),
            CorrelationId::new("peer-corr"),
            MessageKind::PeerMessage,
            42,
            9_999,
            "peer-nonce".to_string(),
            Vec::new(),
            serde_json::json!({"task": "sum", "args": [1, 2]}),
        )
        .expect("envelope must sign")
}

fn proof_attach(tier: ConnectionTier, read_only_ok: bool) -> ClientFrame {
    let signer = test_signer();
    let nonce = vec![0xAA; 32];
    let proof = signer.attach_proof(&nonce, PROTOCOL_VERSION, tier.proof_tag(), read_only_ok);
    ClientFrame::Attach {
        protocol_version: PROTOCOL_VERSION,
        read_only_ok,
        tier,
        challenge_nonce: nonce,
        identity: signer.identity().clone(),
        proof: Ed25519Sig(proof.0.to_vec()),
    }
}

#[test]
fn attach_peer_tier_round_trips_through_json() {
    let frame = proof_attach(ConnectionTier::Peer, false);
    let bytes = serde_json::to_vec(&frame).expect("serialize");
    let back: ClientFrame = serde_json::from_slice(&bytes).expect("deserialize");
    match back {
        ClientFrame::Attach {
            tier,
            protocol_version,
            read_only_ok,
            ..
        } => {
            assert_eq!(tier, ConnectionTier::Peer, "peer tier must survive serde");
            assert_eq!(protocol_version, PROTOCOL_VERSION);
            assert!(!read_only_ok);
        }
        _ => panic!("expected Attach after round-trip, got {back:?}"),
    }
}

#[test]
fn trusted_local_tier_still_round_trips() {
    // Regression guard: the pre-existing local-attach path is unchanged by the
    // tier addition (`#[serde(default)]` keeps old clients working). If the
    // default flips, local TUI attach silently degrades to peer mode.
    let frame = proof_attach(ConnectionTier::TrustedLocal, true);
    let bytes = serde_json::to_vec(&frame).expect("serialize");
    let back: ClientFrame = serde_json::from_slice(&bytes).expect("deserialize");
    match back {
        ClientFrame::Attach { tier, .. } => {
            assert_eq!(tier, ConnectionTier::TrustedLocal);
        }
        _ => panic!("expected Attach"),
    }
}

#[test]
fn peer_envelope_frame_round_trips_preserving_signed_fields() {
    let envelope = sample_peer_envelope();
    let original_sig = envelope.signature.as_bytes().to_vec();
    let original_hash = envelope.header.content_hash.clone();
    let original_sequence = envelope.header.sequence;
    let original_nonce = envelope.header.nonce.clone();

    let frame = ClientFrame::PeerEnvelope(Box::new(envelope));
    let bytes = serde_json::to_vec(&frame).expect("serialize");
    let back: ClientFrame = serde_json::from_slice(&bytes).expect("deserialize");

    let inner = match back {
        ClientFrame::PeerEnvelope(inner) => *inner,
        _ => panic!("expected PeerEnvelope after round-trip"),
    };
    assert_eq!(
        inner.signature.as_bytes(),
        original_sig,
        "signature bytes must be byte-identical"
    );
    assert_eq!(
        inner.header.content_hash, original_hash,
        "content_hash must be byte-identical"
    );
    assert_eq!(
        inner.header.sequence, original_sequence,
        "sequence must be preserved"
    );
    assert_eq!(
        inner.header.nonce, original_nonce,
        "nonce must be preserved"
    );
}

/// The signed envelope must also survive the REAL length-prefixed framing
/// (`u32` BE + serde_json body), not just raw serde — that is the path a peer
/// process actually reads. In-memory duplex, no real socket.
#[tokio::test]
async fn peer_envelope_round_trips_over_length_prefixed_framing() {
    let envelope = sample_peer_envelope();
    let original_sig = envelope.signature.as_bytes().to_vec();

    let (mut a, mut b) = tokio::io::duplex(64 * 1024);
    let frame = ClientFrame::PeerEnvelope(Box::new(envelope));
    let writer = tokio::spawn(async move {
        write_frame(&mut a, &frame).await.expect("write_frame");
        drop(a); // clean EOF
    });

    let received: ClientFrame = match read_frame::<_, ClientFrame>(&mut b)
        .await
        .expect("read_frame")
    {
        Some(f) => f,
        None => panic!("clean EOF before the PeerEnvelope arrived"),
    };
    writer.await.unwrap();

    match received {
        ClientFrame::PeerEnvelope(inner) => {
            assert_eq!(inner.signature.as_bytes(), original_sig);
        }
        _ => panic!("expected PeerEnvelope over framing"),
    }
}
