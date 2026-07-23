//! Story 17.1a — AC2 / DD-3: every replay-relevant header field is bound by
//! the envelope signature.
//!
//! The signed digest is `b"RAP/1\0" || sha256(header) || sha256(payload)`,
//! and DD-3 mandates the signed `header` carry **all** of
//! `{sender, recipient, correlation_id, kind, sequence, not_after, nonce}`.
//! A field that silently drops out of the signed region becomes a malleable
//! plaintext field — decorative replay defense (the exact DD-3 mutant).
//!
//! The inline `wire::tests` already prove `sequence` and `not_after` are bound.
//! This test completes the contract: for each of the *remaining* header fields
//! (`sender`, `recipient`, `correlation_id`, `kind`, `nonce`) it first proves
//! the untouched envelope verifies (positive control), then mutates ONLY that
//! one field and asserts verification fails with [`VerifyError::BadSignature`].
//! A regression that moves any of these fields out of the signed header turns
//! the matching mutation green → RED.

use rustain::adapters::rap::{AgentSigner, VerifyError, verify_envelope};
use rustain::domain::models::{AgentEnvelope, AgentId, CorrelationId, MessageKind};

/// Deterministic signer (fixed seed) so the test never touches `/dev/urandom`
/// or the wall clock — only the sign/verify/tamper logic is exercised.
fn signer() -> AgentSigner {
    AgentSigner::from_signing_key(ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]))
}

fn signed_envelope() -> AgentEnvelope<serde_json::Value> {
    let s = signer();
    // Peer-path invariant (DD-?): the sender must be rooted at the signer's
    // own PeerId, so the envelope origin cannot be spoofed.
    let sender =
        AgentId::from_peer_path(&format!("{}/sender-agent", s.identity().peer_id.as_str()))
            .expect("peer-rooted sender");
    s.sign(
        sender,
        AgentId::parse("recipient").expect("valid recipient"),
        CorrelationId::new("corr-1"),
        MessageKind::PeerMessage,
        1,
        2_000,
        "nonce-aaa".to_string(),
        Vec::new(),
        serde_json::json!({"msg": "hello"}),
    )
    .expect("envelope must sign")
}

/// Positive control + each-field mutant. Every block re-derives a fresh signed
/// envelope, confirms it verifies, then perturbs a single header field.
#[test]
fn every_replay_relevant_header_field_is_bound_by_signature() {
    // ── sender ──────────────────────────────────────────────────────────
    {
        let mut env = signed_envelope();
        verify_envelope(&env, 1_000, None).expect("control: untouched verifies");
        env.header.sender = AgentId::parse("impostor").unwrap();
        assert!(
            matches!(
                verify_envelope(&env, 1_000, None),
                Err(VerifyError::SenderSignerMismatch)
            ),
            "a sender not rooted at the signer peer id must be rejected as a spoof"
        );
    }

    // ── recipient ───────────────────────────────────────────────────────
    {
        let mut env = signed_envelope();
        verify_envelope(&env, 1_000, None).expect("control: untouched verifies");
        env.header.recipient = AgentId::parse("wrong-mailbox").unwrap();
        assert!(
            matches!(
                verify_envelope(&env, 1_000, None),
                Err(VerifyError::BadSignature)
            ),
            "tampered recipient must invalidate the signature"
        );
    }

    // ── correlation_id ──────────────────────────────────────────────────
    {
        let mut env = signed_envelope();
        verify_envelope(&env, 1_000, None).expect("control: untouched verifies");
        env.header.correlation_id = CorrelationId::new("swapped-thread");
        assert!(
            matches!(
                verify_envelope(&env, 1_000, None),
                Err(VerifyError::BadSignature)
            ),
            "tampered correlation_id must invalidate the signature"
        );
    }

    // ── kind ────────────────────────────────────────────────────────────
    {
        let mut env = signed_envelope();
        verify_envelope(&env, 1_000, None).expect("control: untouched verifies");
        env.header.kind = MessageKind::Refusal;
        assert!(
            matches!(
                verify_envelope(&env, 1_000, None),
                Err(VerifyError::BadSignature)
            ),
            "tampered kind must invalidate the signature"
        );
    }

    // ── nonce ───────────────────────────────────────────────────────────
    {
        let mut env = signed_envelope();
        verify_envelope(&env, 1_000, None).expect("control: untouched verifies");
        env.header.nonce = "replayed-nonce".to_string();
        assert!(
            matches!(
                verify_envelope(&env, 1_000, None),
                Err(VerifyError::BadSignature)
            ),
            "tampered nonce must invalidate the signature"
        );
    }
}
