//! Story 17.1a — Task 5 harness seam.
//!
//! This is the deterministic same-crate harness for the signed peer boundary:
//! fixed signing keys, length-prefixed frame capture, explicit arming mutations,
//! and a recording `AgentTransport` seam. It deliberately avoids sleeps, wall
//! clock, and network listeners; Epic 18 owns remote transport.

use rustain::adapters::daemon::protocol::{ClientFrame, read_frame, write_frame};
use rustain::adapters::rap::{AgentSigner, ReplayWindow, VerifyError, verify_envelope};
use rustain::domain::models::{AgentEnvelope, AgentId, CorrelationId, MessageKind};
use rustain::domain::ports::{AgentTransport, AgentTransportError};
use serde_json::Value;
use tokio::sync::{Mutex, broadcast};

fn signed_peer_envelope(sequence: u64, not_after: i64, nonce: &str) -> AgentEnvelope<Value> {
    let signer = AgentSigner::from_signing_key(ed25519_dalek::SigningKey::from_bytes(&[11u8; 32]));
    let sender = AgentId::from_peer_path(&format!("{}/peer-a", signer.identity().peer_id.as_str()))
        .expect("peer-rooted sender");
    signer
        .sign(
            sender,
            AgentId::parse("peer-b").expect("valid recipient"),
            CorrelationId::new("harness-corr"),
            MessageKind::PeerMessage,
            sequence,
            not_after,
            nonce.to_string(),
            Vec::new(),
            serde_json::json!({"op":"harness", "sequence": sequence}),
        )
        .expect("sign envelope")
}

#[derive(Clone, Copy)]
enum HarnessArm {
    None,
    FlipSigBit,
    ExpireAt(i64),
    RejectNonce(&'static str),
}

struct SignedPeerHarness {
    arm: HarnessArm,
    replay: ReplayWindow,
}

impl SignedPeerHarness {
    fn new() -> Self {
        Self {
            arm: HarnessArm::None,
            replay: ReplayWindow::default(),
        }
    }

    fn arm(&mut self, arm: HarnessArm) {
        self.arm = arm;
    }

    async fn capture_frame_bytes(&self, envelope: AgentEnvelope<Value>) -> ClientFrame {
        let (mut write_half, mut read_half) = tokio::io::duplex(64 * 1024);
        let frame = ClientFrame::PeerEnvelope(Box::new(envelope));
        let writer = tokio::spawn(async move {
            write_frame(&mut write_half, &frame)
                .await
                .expect("write signed peer frame");
        });
        let received = read_frame::<_, ClientFrame>(&mut read_half)
            .await
            .expect("read signed peer frame")
            .expect("peer frame present");
        writer.await.expect("writer task joins");
        received
    }

    async fn verify(
        &mut self,
        mut envelope: AgentEnvelope<Value>,
        now_unix: i64,
    ) -> Result<(), VerifyError> {
        match self.arm {
            HarnessArm::None => {}
            HarnessArm::FlipSigBit => envelope.signature.0[0] ^= 0x80,
            HarnessArm::ExpireAt(expired_now) => {
                return verify_envelope(&envelope, expired_now, Some(&mut self.replay));
            }
            HarnessArm::RejectNonce(nonce) if envelope.header.nonce == nonce => {
                envelope.header.nonce.push_str("-mutated-by-arm");
            }
            HarnessArm::RejectNonce(_) => {}
        }
        verify_envelope(&envelope, now_unix, Some(&mut self.replay))
    }
}

#[derive(Debug)]
struct RecordingSignedPeer {
    tx: broadcast::Sender<AgentEnvelope<Value>>,
    sent: Mutex<Vec<u64>>,
}

impl RecordingSignedPeer {
    fn new() -> Self {
        let (tx, _) = broadcast::channel(8);
        Self {
            tx,
            sent: Mutex::new(Vec::new()),
        }
    }

    async fn sent_sequences(&self) -> Vec<u64> {
        self.sent.lock().await.clone()
    }
}

#[async_trait::async_trait]
impl AgentTransport for RecordingSignedPeer {
    async fn send(&self, envelope: AgentEnvelope<Value>) -> Result<(), AgentTransportError> {
        self.sent.lock().await.push(envelope.header.sequence);
        let _ = self.tx.send(envelope);
        Ok(())
    }

    fn subscribe(&self) -> Result<broadcast::Receiver<AgentEnvelope<Value>>, AgentTransportError> {
        Ok(self.tx.subscribe())
    }
}

#[tokio::test]
async fn harness_captures_signed_frame_and_verifies_positive_control() {
    let mut harness = SignedPeerHarness::new();
    let envelope = signed_peer_envelope(1, 2_000, "ok");
    let frame = harness.capture_frame_bytes(envelope).await;
    let envelope = match frame {
        ClientFrame::PeerEnvelope(inner) => *inner,
        other => panic!("expected peer envelope frame, got {other:?}"),
    };
    harness
        .verify(envelope, 1_000)
        .await
        .expect("valid envelope verifies");
}

#[tokio::test]
async fn arming_mutations_reject_without_recording_transport_side_effect() {
    let mut harness = SignedPeerHarness::new();
    let recording = RecordingSignedPeer::new();

    let accepted = signed_peer_envelope(1, 2_000, "accepted");
    harness
        .verify(accepted.clone(), 1_000)
        .await
        .expect("accepted envelope verifies");
    recording.send(accepted).await.expect("record accepted");

    harness.arm(HarnessArm::FlipSigBit);
    assert!(matches!(
        harness
            .verify(signed_peer_envelope(2, 2_000, "sig"), 1_000)
            .await,
        Err(VerifyError::BadSignature)
    ));

    harness.arm(HarnessArm::ExpireAt(2_001));
    assert!(matches!(
        harness
            .verify(signed_peer_envelope(3, 2_000, "ttl"), 1_000)
            .await,
        Err(VerifyError::Expired { .. })
    ));

    harness.arm(HarnessArm::RejectNonce("reject-me"));
    assert!(matches!(
        harness
            .verify(signed_peer_envelope(4, 2_000, "reject-me"), 1_000)
            .await,
        Err(VerifyError::BadSignature)
    ));

    assert_eq!(
        recording.sent_sequences().await,
        vec![1],
        "rejected armed frames must not be recorded as transport side effects"
    );
}

#[tokio::test]
async fn recording_signed_peer_subscribe_receives_sent_envelope() {
    let recording = RecordingSignedPeer::new();
    let mut rx = recording.subscribe().expect("subscribe");
    let envelope = signed_peer_envelope(9, 2_000, "record");
    recording.send(envelope.clone()).await.expect("send");
    assert_eq!(rx.recv().await.expect("receive").header.sequence, 9);
    assert_eq!(recording.sent_sequences().await, vec![9]);
}
