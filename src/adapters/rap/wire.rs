use std::collections::{HashMap, HashSet, VecDeque};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::domain::models::{
    AgentEnvelope, AgentEnvelopeHeader, AgentId, CorrelationId, Ed25519Sig, MessageKind, PeerId,
    PeerIdentity,
};

pub const RAP_DOMAIN: &[u8] = b"RAP/1\0";

/// Domain-separation tag for Attach challenge proofs. Every Attach proof signs
/// a transcript rooted here so it cannot be confused with an envelope signature
/// or any other signed artifact in the RAP domain.
pub const ATTACH_PROOF_DOMAIN: &[u8] = b"RAP/1\0attach-proof\0";

#[derive(Clone)]
pub struct AgentSigner {
    key: SigningKey,
    identity: PeerIdentity,
}

impl std::fmt::Debug for AgentSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentSigner")
            .field("peer_id", &self.identity.peer_id)
            .field("key", &"<redacted>")
            .finish()
    }
}

impl AgentSigner {
    pub fn from_signing_key(key: SigningKey) -> Self {
        let public_key = key.verifying_key().to_bytes().to_vec();
        let identity = PeerIdentity::from_public_key(public_key)
            .expect("ed25519 verifying keys are always 32 bytes");
        Self { key, identity }
    }

    pub fn identity(&self) -> &PeerIdentity {
        &self.identity
    }

    pub fn sign<T: Serialize>(
        &self,
        sender: AgentId,
        recipient: AgentId,
        correlation_id: CorrelationId,
        kind: MessageKind,
        sequence: u64,
        not_after: i64,
        nonce: String,
        prev_hash: Vec<u8>,
        body: T,
    ) -> Result<AgentEnvelope<T>, VerifyError> {
        sign_envelope(
            &self.key,
            self.identity.clone(),
            sender,
            recipient,
            correlation_id,
            kind,
            sequence,
            not_after,
            nonce,
            prev_hash,
            body,
        )
    }

    /// Build an Attach challenge proof by signing the domain-separated transcript.
    ///
    /// The transcript binds the signer's PeerId (derived from the key) together
    /// with the negotiation primitives (`nonce`, `protocol_version`, `tier_tag`,
    /// `read_only`), so a proof cannot be replayed across tiers, versions, or
    /// read-only grants, and cannot be claimed by a different peer. Independent
    /// of daemon protocol types: the caller maps its frame fields to these
    /// primitives.
    pub fn attach_proof(
        &self,
        nonce: &[u8],
        protocol_version: u32,
        tier_tag: &str,
        read_only: bool,
    ) -> Ed25519Sig {
        let transcript = attach_proof_transcript(
            &self.identity.peer_id,
            nonce,
            protocol_version,
            tier_tag,
            read_only,
        );
        let sig = self.key.sign(&transcript);
        Ed25519Sig(sig.to_bytes().to_vec())
    }
}

pub fn sign_envelope<T: Serialize>(
    key: &SigningKey,
    signer: PeerIdentity,
    sender: AgentId,
    recipient: AgentId,
    correlation_id: CorrelationId,
    kind: MessageKind,
    sequence: u64,
    not_after: i64,
    nonce: String,
    prev_hash: Vec<u8>,
    body: T,
) -> Result<AgentEnvelope<T>, VerifyError> {
    signer.verify_binding()?;
    // Peer-path invariant (signing side): refuse to sign an envelope whose
    // sender is not rooted at the signer's own PeerId.
    if !sender_bound_to_signer(&sender, &signer.peer_id) {
        return Err(VerifyError::SenderSignerMismatch);
    }
    let payload = canonical_json(&body)?;
    let content_hash = Sha256::digest(&payload).to_vec();
    let header = AgentEnvelopeHeader {
        sender,
        recipient,
        correlation_id,
        kind,
        sequence,
        not_after,
        nonce,
        content_hash,
        prev_hash,
    };
    let signing_bytes = signing_bytes(&header, &payload)?;
    let signature = key.sign(&signing_bytes);
    Ok(AgentEnvelope::new(
        header,
        body,
        signer,
        Ed25519Sig(signature.to_bytes().to_vec()),
    ))
}

pub fn verify_envelope<T: Serialize>(
    envelope: &AgentEnvelope<T>,
    now_unix: i64,
    replay: Option<&mut ReplayWindow>,
) -> Result<(), VerifyError> {
    verify_envelope_crypto(envelope, now_unix)?;
    if let Some(replay) = replay {
        replay.accept(
            &envelope.signer.peer_id,
            envelope.header.sequence,
            &envelope.header.prev_hash,
            &envelope.header.nonce,
            entry_hash(&envelope.header)?,
        )?;
    }
    Ok(())
}

/// Verify and reserve a replay/feed position without committing it.
///
/// The caller performs semantic delivery without holding the replay lock, then
/// calls [`ReplayWindow::commit`] on success or [`ReplayWindow::rollback`] on
/// failure. A pending reservation rejects concurrent frames for the same peer.
pub fn verify_envelope_reserved<T: Serialize>(
    envelope: &AgentEnvelope<T>,
    now_unix: i64,
    replay: &mut ReplayWindow,
) -> Result<ReplayReservation, VerifyError> {
    verify_envelope_crypto(envelope, now_unix)?;
    replay.reserve(
        &envelope.signer.peer_id,
        envelope.header.sequence,
        &envelope.header.prev_hash,
        &envelope.header.nonce,
        entry_hash(&envelope.header)?,
    )
}

fn verify_envelope_crypto<T: Serialize>(
    envelope: &AgentEnvelope<T>,
    now_unix: i64,
) -> Result<(), VerifyError> {
    envelope.signer.verify_binding()?;
    if !sender_bound_to_signer(&envelope.header.sender, &envelope.signer.peer_id) {
        return Err(VerifyError::SenderSignerMismatch);
    }
    if now_unix > envelope.header.not_after {
        return Err(VerifyError::Expired {
            now_unix,
            not_after: envelope.header.not_after,
        });
    }
    let payload = canonical_json(&envelope.body)?;
    let content_hash = Sha256::digest(&payload).to_vec();
    if content_hash != envelope.header.content_hash {
        return Err(VerifyError::ContentHashMismatch);
    }
    let public_key: [u8; 32] = envelope
        .signer
        .public_key
        .as_slice()
        .try_into()
        .map_err(|_| VerifyError::InvalidPublicKeyLength(envelope.signer.public_key.len()))?;
    let verifying_key = VerifyingKey::from_bytes(&public_key)
        .map_err(|e| VerifyError::InvalidPublicKey(e.to_string()))?;
    let signature: [u8; 64] =
        envelope.signature.as_bytes().try_into().map_err(|_| {
            VerifyError::InvalidSignatureLength(envelope.signature.as_bytes().len())
        })?;
    let signature = Signature::from_bytes(&signature);
    let signing_bytes = signing_bytes(&envelope.header, &payload)?;
    verifying_key
        .verify(&signing_bytes, &signature)
        .map_err(|_| VerifyError::BadSignature)?;
    Ok(())
}

const MAX_NONCES_PER_PEER: usize = 1_024;

#[derive(Clone, Debug)]
struct PeerFeed {
    highest: u64,
    head: Vec<u8>,
    nonces: HashSet<String>,
    nonce_order: VecDeque<String>,
}

/// A verified replay/feed update held pending until semantic ingest succeeds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplayReservation {
    peer_id: PeerId,
    sequence: u64,
    nonce: String,
    entry_hash: Vec<u8>,
}

/// Single-process verification state. Committed feed state is separate from
/// pending reservations so no replay lock is held across asynchronous ingest.
#[derive(Clone, Debug, Default)]
pub struct ReplayWindow {
    feeds: HashMap<PeerId, PeerFeed>,
    pending: HashMap<PeerId, ReplayReservation>,
}

impl ReplayWindow {
    fn validate_candidate(
        &self,
        peer_id: &PeerId,
        sequence: u64,
        prev_hash: &[u8],
        nonce: &str,
    ) -> Result<(), VerifyError> {
        if self.pending.contains_key(peer_id) {
            return Err(VerifyError::ReplayPending {
                peer_id: peer_id.to_string(),
            });
        }
        if let Some(feed) = self.feeds.get(peer_id) {
            if sequence <= feed.highest {
                return Err(VerifyError::Replay {
                    peer_id: peer_id.to_string(),
                    sequence,
                    highest: feed.highest,
                });
            }
            if prev_hash != feed.head {
                return Err(VerifyError::FeedForkOrGap {
                    peer_id: peer_id.to_string(),
                });
            }
            if feed.nonces.contains(nonce) {
                return Err(VerifyError::NonceReplay {
                    peer_id: peer_id.to_string(),
                    nonce: nonce.to_owned(),
                });
            }
        } else if !prev_hash.is_empty() {
            return Err(VerifyError::FeedForkOrGap {
                peer_id: peer_id.to_string(),
            });
        }
        Ok(())
    }

    fn reserve(
        &mut self,
        peer_id: &PeerId,
        sequence: u64,
        prev_hash: &[u8],
        nonce: &str,
        entry_hash: Vec<u8>,
    ) -> Result<ReplayReservation, VerifyError> {
        self.validate_candidate(peer_id, sequence, prev_hash, nonce)?;
        let reservation = ReplayReservation {
            peer_id: peer_id.clone(),
            sequence,
            nonce: nonce.to_owned(),
            entry_hash,
        };
        self.pending.insert(peer_id.clone(), reservation.clone());
        Ok(reservation)
    }

    pub fn commit(&mut self, reservation: ReplayReservation) -> bool {
        if self.pending.get(&reservation.peer_id) != Some(&reservation) {
            return false;
        }
        self.pending.remove(&reservation.peer_id);
        let feed = self
            .feeds
            .entry(reservation.peer_id)
            .or_insert_with(|| PeerFeed {
                highest: 0,
                head: Vec::new(),
                nonces: HashSet::new(),
                nonce_order: VecDeque::new(),
            });
        feed.highest = reservation.sequence;
        feed.head = reservation.entry_hash;
        feed.nonces.insert(reservation.nonce.clone());
        feed.nonce_order.push_back(reservation.nonce);
        if feed.nonce_order.len() > MAX_NONCES_PER_PEER {
            if let Some(expired) = feed.nonce_order.pop_front() {
                feed.nonces.remove(&expired);
            }
        }
        true
    }

    pub fn rollback(&mut self, reservation: &ReplayReservation) -> bool {
        if self.pending.get(&reservation.peer_id) != Some(reservation) {
            return false;
        }
        self.pending.remove(&reservation.peer_id);
        true
    }

    fn accept(
        &mut self,
        peer_id: &PeerId,
        sequence: u64,
        prev_hash: &[u8],
        nonce: &str,
        entry_hash: Vec<u8>,
    ) -> Result<(), VerifyError> {
        let reservation = self.reserve(peer_id, sequence, prev_hash, nonce, entry_hash)?;
        debug_assert!(self.commit(reservation));
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum VerifyError {
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
    #[error(transparent)]
    PeerIdentity(#[from] crate::domain::models::PeerIdentityError),
    #[error("invalid public key length: {0}")]
    InvalidPublicKeyLength(usize),
    #[error("invalid ed25519 public key: {0}")]
    InvalidPublicKey(String),
    #[error("invalid signature length: {0}")]
    InvalidSignatureLength(usize),
    #[error("bad signature")]
    BadSignature,
    #[error("content hash mismatch")]
    ContentHashMismatch,
    #[error("envelope sender is not rooted at the signer peer id (spoof rejected)")]
    SenderSignerMismatch,
    #[error("expired envelope: now={now_unix} not_after={not_after}")]
    Expired { now_unix: i64, not_after: i64 },
    #[error("replayed envelope from {peer_id}: sequence {sequence} <= highest {highest}")]
    Replay {
        peer_id: String,
        sequence: u64,
        highest: u64,
    },
    #[error("peer feed already has a delivery pending from {peer_id}")]
    ReplayPending { peer_id: String },
    #[error("peer feed fork or gap from {peer_id}")]
    FeedForkOrGap { peer_id: String },
    #[error("reused nonce from {peer_id}: {nonce}")]
    NonceReplay { peer_id: String, nonce: String },
}

/// SHA-256 of the canonical signed header. This is the per-sender feed entry
/// identity and the only valid predecessor for the next `prev_hash`.
pub fn entry_hash(header: &AgentEnvelopeHeader) -> Result<Vec<u8>, VerifyError> {
    Ok(Sha256::digest(canonical_json(header)?).to_vec())
}

fn signing_bytes(header: &AgentEnvelopeHeader, payload: &[u8]) -> Result<Vec<u8>, VerifyError> {
    let header_hash = entry_hash(header)?;
    let payload_hash = Sha256::digest(payload);
    let mut out = Vec::with_capacity(RAP_DOMAIN.len() + 64);
    out.extend_from_slice(RAP_DOMAIN);
    out.extend_from_slice(&header_hash);
    out.extend_from_slice(&payload_hash);
    Ok(out)
}

/// Deterministic canonical JSON encoding for signing and verification.
///
/// Routes every value through `serde_json::Value` first so that typed structs
/// and `Value` bodies with identical content produce byte-identical output:
/// `serde_json` (compiled without `preserve_order`) backs `Map` with `BTreeMap`,
/// so object keys are emitted in sorted order regardless of struct declaration
/// order or map insertion order. This eliminates the generic-serde byte
/// ambiguity that let a typed body and its `Value` twin hash differently.
/// Any future non-scalar header field must retain this explicit
/// `serde_json::Value` normalization or introduce a versioned canonical form;
/// direct `serde_json::to_vec(header)` is not a signing contract.
fn canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>, VerifyError> {
    let normalized = serde_json::to_value(value)?;
    Ok(serde_json::to_vec(&normalized)?)
}

/// Peer-path invariant: the envelope `sender` must be rooted at the signer's
/// PeerId — either the bare peer id or a peer path `<peer_id>/<child>[/...]`.
/// This prevents a peer from signing an envelope that claims another peer's
/// agent as its origin (sender spoofing). Checked in both signing and
/// verification so the invariant cannot be bypassed by crafting a raw signature.
fn sender_bound_to_signer(sender: &AgentId, peer_id: &PeerId) -> bool {
    let s = sender.as_str();
    let pid = peer_id.as_str();
    s == pid || {
        let prefix = format!("{pid}/");
        s.starts_with(&prefix)
    }
}

/// Deterministic domain-separated transcript for an Attach challenge proof.
///
/// Binds the peer id, server-issued challenge nonce, negotiated protocol
/// version, connection tier tag, and read-only flag into one unambiguous byte
/// string. All variable-length fields are length-prefixed (u64 BE) so that no
/// two distinct parameter sets can produce the same transcript. Daemon-protocol
/// independent: the caller supplies primitives, not frame types.
pub fn attach_proof_transcript(
    peer_id: &PeerId,
    nonce: &[u8],
    protocol_version: u32,
    tier_tag: &str,
    read_only: bool,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(ATTACH_PROOF_DOMAIN);
    attach_push_length_prefixed(&mut out, peer_id.as_str().as_bytes());
    attach_push_length_prefixed(&mut out, nonce);
    out.extend_from_slice(&protocol_version.to_be_bytes());
    attach_push_length_prefixed(&mut out, tier_tag.as_bytes());
    out.push(if read_only { 1 } else { 0 });
    out
}

/// Verify an Attach challenge proof against a peer identity.
///
/// Checks (in order): the identity's PeerId matches its public key
/// (`verify_binding`); the PeerId matches the transcript; the Ed25519 signature
/// is valid over the domain-separated transcript. Fails closed on any mismatch.
pub fn verify_attach_proof(
    identity: &PeerIdentity,
    proof: &Ed25519Sig,
    nonce: &[u8],
    protocol_version: u32,
    tier_tag: &str,
    read_only: bool,
) -> Result<(), VerifyError> {
    identity.verify_binding()?;
    let transcript = attach_proof_transcript(
        &identity.peer_id,
        nonce,
        protocol_version,
        tier_tag,
        read_only,
    );
    let public_key: [u8; 32] = identity
        .public_key
        .as_slice()
        .try_into()
        .map_err(|_| VerifyError::InvalidPublicKeyLength(identity.public_key.len()))?;
    let verifying_key = VerifyingKey::from_bytes(&public_key)
        .map_err(|e| VerifyError::InvalidPublicKey(e.to_string()))?;
    let signature: [u8; 64] = proof
        .as_bytes()
        .try_into()
        .map_err(|_| VerifyError::InvalidSignatureLength(proof.as_bytes().len()))?;
    verifying_key
        .verify(&transcript, &Signature::from_bytes(&signature))
        .map_err(|_| VerifyError::BadSignature)?;
    Ok(())
}

fn attach_push_length_prefixed(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    out.extend_from_slice(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signer(seed: u8) -> AgentSigner {
        AgentSigner::from_signing_key(SigningKey::from_bytes(&[seed; 32]))
    }

    /// Construct a sender AgentId rooted at the signer's PeerId (peer-path invariant).
    fn peer_sender(signer: &AgentSigner) -> AgentId {
        AgentId::from_peer_path(&format!("{}/agent", signer.identity().peer_id.as_str()))
            .expect("peer-rooted sender")
    }

    fn envelope(sequence: u64, not_after: i64) -> AgentEnvelope<Value> {
        let signer = signer(7);
        signer
            .sign(
                peer_sender(&signer),
                AgentId::parse("recipient").unwrap(),
                CorrelationId::new("corr"),
                MessageKind::PeerMessage,
                sequence,
                not_after,
                "nonce".to_string(),
                Vec::new(),
                serde_json::json!({"msg":"hello"}),
            )
            .unwrap()
    }

    #[test]
    fn signed_envelope_verifies_and_rejects_tampered_payload() {
        let mut env = envelope(1, 2_000);
        verify_envelope(&env, 1_000, Some(&mut ReplayWindow::default())).unwrap();
        env.body = serde_json::json!({"msg":"tampered"});
        assert!(matches!(
            verify_envelope(&env, 1_000, None),
            Err(VerifyError::ContentHashMismatch)
        ));
    }

    #[test]
    fn signed_header_binds_sequence_and_ttl() {
        let mut env = envelope(1, 2_000);
        env.header.sequence = 2;
        assert!(matches!(
            verify_envelope(&env, 1_000, None),
            Err(VerifyError::BadSignature)
        ));
        let mut env = envelope(1, 2_000);
        env.header.not_after = 3_000;
        assert!(matches!(
            verify_envelope(&env, 1_000, None),
            Err(VerifyError::BadSignature)
        ));
    }

    #[test]
    fn verify_rejects_replay_and_expiry() {
        let env = envelope(3, 2_000);
        let mut replay = ReplayWindow::default();
        verify_envelope(&env, 1_000, Some(&mut replay)).unwrap();
        assert!(matches!(
            verify_envelope(&env, 1_000, Some(&mut replay)),
            Err(VerifyError::Replay { .. })
        ));
        assert!(matches!(
            verify_envelope(&envelope(4, 2_000), 2_001, None),
            Err(VerifyError::Expired { .. })
        ));
    }

    #[test]
    fn reserved_replay_state_rolls_back_on_failed_ingest_and_commits_once() {
        let env = envelope(1, 2_000);
        let mut replay = ReplayWindow::default();
        let reservation = verify_envelope_reserved(&env, 1_000, &mut replay)
            .expect("cryptographic verification reserves feed position");
        assert!(matches!(
            verify_envelope_reserved(&env, 1_000, &mut replay),
            Err(VerifyError::ReplayPending { .. })
        ));
        assert!(replay.rollback(&reservation));

        let retry = verify_envelope_reserved(&env, 1_000, &mut replay)
            .expect("rolled-back delivery remains retriable");
        assert!(replay.commit(retry));
        assert!(matches!(
            verify_envelope_reserved(&env, 1_000, &mut replay),
            Err(VerifyError::Replay { .. })
        ));
    }
    #[test]
    fn verify_rejects_nonce_reuse_even_when_sequence_advances() {
        let signer = signer(7);
        let first = signer
            .sign(
                peer_sender(&signer),
                AgentId::parse("recipient").unwrap(),
                CorrelationId::new("corr"),
                MessageKind::PeerMessage,
                1,
                2_000,
                "reused-nonce".to_string(),
                Vec::new(),
                serde_json::json!({"msg":"first"}),
            )
            .unwrap();
        let second = signer
            .sign(
                peer_sender(&signer),
                AgentId::parse("recipient").unwrap(),
                CorrelationId::new("corr"),
                MessageKind::PeerMessage,
                2,
                2_000,
                "reused-nonce".to_string(),
                entry_hash(&first.header).unwrap(),
                serde_json::json!({"msg":"second"}),
            )
            .unwrap();
        let mut replay = ReplayWindow::default();
        verify_envelope(&first, 1_000, Some(&mut replay)).unwrap();
        assert!(matches!(
            verify_envelope(&second, 1_000, Some(&mut replay)),
            Err(VerifyError::NonceReplay { .. })
        ));
    }

    #[test]
    fn verify_accepts_linked_feed_and_rejects_fork_gap_without_consuming_state() {
        let signer = signer(7);
        let first = signer
            .sign(
                peer_sender(&signer),
                AgentId::parse("recipient").unwrap(),
                CorrelationId::new("corr"),
                MessageKind::PeerMessage,
                1,
                2_000,
                "nonce-1".to_string(),
                Vec::new(),
                serde_json::json!({"entry":1}),
            )
            .unwrap();
        let second = signer
            .sign(
                peer_sender(&signer),
                AgentId::parse("recipient").unwrap(),
                CorrelationId::new("corr"),
                MessageKind::PeerMessage,
                2,
                2_000,
                "nonce-2".to_string(),
                entry_hash(&first.header).unwrap(),
                serde_json::json!({"entry":2}),
            )
            .unwrap();
        let fork = signer
            .sign(
                peer_sender(&signer),
                AgentId::parse("recipient").unwrap(),
                CorrelationId::new("corr"),
                MessageKind::PeerMessage,
                3,
                2_000,
                "nonce-fork".to_string(),
                entry_hash(&first.header).unwrap(),
                serde_json::json!({"entry":"fork"}),
            )
            .unwrap();
        let third = signer
            .sign(
                peer_sender(&signer),
                AgentId::parse("recipient").unwrap(),
                CorrelationId::new("corr"),
                MessageKind::PeerMessage,
                3,
                2_000,
                "nonce-3".to_string(),
                entry_hash(&second.header).unwrap(),
                serde_json::json!({"entry":3}),
            )
            .unwrap();

        let mut replay = ReplayWindow::default();
        verify_envelope(&first, 1_000, Some(&mut replay)).unwrap();
        verify_envelope(&second, 1_000, Some(&mut replay)).unwrap();
        assert!(matches!(
            verify_envelope(&fork, 1_000, Some(&mut replay)),
            Err(VerifyError::FeedForkOrGap { .. })
        ));
        verify_envelope(&third, 1_000, Some(&mut replay))
            .expect("rejected fork must not advance the trusted feed head");
    }

    #[test]
    fn prev_hash_is_signed_and_header_canonicalization_is_stable() {
        let env = envelope(1, 2_000);
        let same_header = env.header.clone();
        assert_eq!(
            canonical_json(&env.header).unwrap(),
            canonical_json(&same_header).unwrap(),
            "equal scalar headers must have byte-stable canonical encoding"
        );
        assert_eq!(
            entry_hash(&env.header).unwrap(),
            entry_hash(&same_header).unwrap()
        );

        let mut tampered = env;
        tampered.header.prev_hash = vec![0xAA];
        assert!(matches!(
            verify_envelope(&tampered, 1_000, None),
            Err(VerifyError::BadSignature)
        ));
    }

    #[test]
    fn verify_rejects_pubkey_peer_id_mismatch_before_signature() {
        let mut env = envelope(1, 2_000);
        env.signer.public_key[0] ^= 0x55;
        assert!(matches!(
            verify_envelope(&env, 1_000, None),
            Err(VerifyError::PeerIdentity(_))
        ));
    }

    #[test]
    fn sign_refuses_sender_not_rooted_at_signer() {
        // Signing side of the peer-path invariant: the signer refuses to mint
        // an envelope whose sender belongs to a different peer.
        let s = signer(7);
        let impostor_sender = peer_sender(&signer(99));
        let result = s.sign(
            impostor_sender,
            AgentId::parse("recipient").unwrap(),
            CorrelationId::new("corr"),
            MessageKind::PeerMessage,
            1,
            2_000,
            "nonce".to_string(),
            Vec::new(),
            serde_json::json!({}),
        );
        assert!(matches!(result, Err(VerifyError::SenderSignerMismatch)));
    }

    #[test]
    fn verify_rejects_cross_peer_sender_spoof() {
        // Verification side: a sender rooted at a different peer is rejected
        // by the binding check BEFORE the cryptographic signature check.
        let s = signer(7);
        let mut env = s
            .sign(
                peer_sender(&s),
                AgentId::parse("recipient").unwrap(),
                CorrelationId::new("corr"),
                MessageKind::PeerMessage,
                1,
                2_000,
                "nonce".to_string(),
                Vec::new(),
                serde_json::json!({"msg": "x"}),
            )
            .unwrap();
        verify_envelope(&env, 1_000, None).expect("control verifies");
        env.header.sender = peer_sender(&signer(99));
        assert!(matches!(
            verify_envelope(&env, 1_000, None),
            Err(VerifyError::SenderSignerMismatch)
        ));
    }

    #[derive(Serialize)]
    struct TypedBody {
        // Declaration order is deliberately NOT sorted (z before a).
        z: u64,
        a: String,
    }

    #[test]
    fn typed_and_value_bodies_cross_verify_under_canonical_encoding() {
        // Sign with a typed struct; verify after a serde round-trip through
        // AgentEnvelope<Value>. Without canonical encoding the typed struct
        // serializes its fields in declaration order (z, a) while the Value
        // twin sorts keys (a, z) → different bytes → content-hash mismatch.
        // Canonical JSON (Value round-trip) makes both produce identical bytes.
        let signer = signer(7);
        let env = signer
            .sign(
                peer_sender(&signer),
                AgentId::parse("recipient").unwrap(),
                CorrelationId::new("corr"),
                MessageKind::PeerMessage,
                1,
                2_000,
                "nonce".to_string(),
                Vec::new(),
                TypedBody {
                    z: 1,
                    a: "hi".into(),
                },
            )
            .unwrap();
        let json = serde_json::to_string(&env).unwrap();
        let value_env: AgentEnvelope<Value> = serde_json::from_str(&json).unwrap();
        verify_envelope(&value_env, 1_000, None)
            .expect("typed and Value bodies must cross-verify under canonical encoding");
    }

    #[test]
    fn attach_proof_round_trips_and_rejects_tamper() {
        let s = signer(7);
        let nonce = b"server-challenge-nonce";
        let proof = s.attach_proof(nonce, 3, "peer", false);
        verify_attach_proof(s.identity(), &proof, nonce, 3, "peer", false)
            .expect("valid attach proof verifies");
        // Tamper the tier tag.
        assert!(matches!(
            verify_attach_proof(s.identity(), &proof, nonce, 3, "trusted-local", false),
            Err(VerifyError::BadSignature)
        ));
        // Tamper the nonce.
        assert!(matches!(
            verify_attach_proof(s.identity(), &proof, b"other", 3, "peer", false),
            Err(VerifyError::BadSignature)
        ));
        // Wrong identity (different key) cannot claim the proof.
        let other = signer(99);
        assert!(matches!(
            verify_attach_proof(other.identity(), &proof, nonce, 3, "peer", false),
            Err(VerifyError::BadSignature)
        ));
        // read_only flag is bound into the transcript.
        assert!(matches!(
            verify_attach_proof(s.identity(), &proof, nonce, 3, "peer", true),
            Err(VerifyError::BadSignature)
        ));
    }

    /// NFR70b — Ed25519 signature verification + canonical hash + replay
    /// check must complete in < 5 ms per envelope on the local path (release).
    /// Debug-mode crypto is ~5× slower; the bound is relaxed there.
    #[test]
    fn nfr70b_verify_envelope_completes_under_budget() {
        let env = envelope(1, 2_000_000_000);
        let mut replay = ReplayWindow::default();
        // Warm: first verify primes any lazy crypto state.
        verify_envelope(&env, 1_000_000_000, Some(&mut replay)).unwrap();
        let mut replay2 = ReplayWindow::default();
        let start = std::time::Instant::now();
        verify_envelope(&env, 1_000_000_000, Some(&mut replay2)).unwrap();
        let elapsed = start.elapsed();
        // NFR70b: < 5ms in release; relax to 50ms in debug (crypto ~5× slower).
        #[cfg(debug_assertions)]
        let budget_ms = 50;
        #[cfg(not(debug_assertions))]
        let budget_ms = 5;
        assert!(
            elapsed.as_millis() < budget_ms,
            "NFR70b: verify took {elapsed:?}, must be < {budget_ms}ms"
        );
    }
}
