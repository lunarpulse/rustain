use ed25519_dalek::Signer as _;
use rustain::adapters::rap::{AgentSigner, RAP_DOMAIN, VerifyError, entry_hash, verify_envelope};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

use rustain::domain::models::{
    AgentEnvelope, AgentEnvelopeHeader, AgentId, CorrelationId, Ed25519Sig,
    InteractionPolicySnapshot, MessageKind, SemanticMessageRecognition, SemanticMessageType,
    semantic_message_type_metadata,
};

#[test]
fn semantic_message_type_registry_resolves_the_closed_vocabulary() {
    let cases = [
        ("consultation", SemanticMessageType::Consultation),
        ("story_assignment", SemanticMessageType::StoryAssignment),
        ("design_update", SemanticMessageType::DesignUpdate),
        ("status_request", SemanticMessageType::StatusRequest),
        ("bug_report", SemanticMessageType::BugReport),
        ("scope_change", SemanticMessageType::ScopeChange),
        (
            "architecture_update",
            SemanticMessageType::ArchitectureUpdate,
        ),
        ("retro_request", SemanticMessageType::RetroRequest),
    ];

    let mut tokens = BTreeSet::new();
    for (token, expected) in cases {
        assert_eq!(SemanticMessageType::parse(token), expected);
        let metadata = semantic_message_type_metadata(expected);
        assert_eq!(metadata.token, token);
        assert_eq!(metadata.operator_noun, token);
        assert_eq!(metadata.recognition, SemanticMessageRecognition::Known);
        assert!(metadata.policy.notification);
        assert!(metadata.policy.response_mode);
        assert!(metadata.policy.auto_response);
        assert!(tokens.insert(metadata.token), "duplicate token: {token}");
    }
    assert_eq!(tokens.len(), 8);
}

#[test]
fn absent_empty_and_unrecognised_tokens_resolve_to_unknown_metadata() {
    for token in [None, Some(""), Some("newer_peer_type")] {
        let resolved = SemanticMessageType::parse_optional(token);
        assert_eq!(resolved, SemanticMessageType::Unknown);
        let metadata = semantic_message_type_metadata(resolved);
        assert_eq!(metadata.token, "");
        assert_eq!(metadata.operator_noun, "unknown type");
        assert_eq!(metadata.recognition, SemanticMessageRecognition::Unknown);
        assert!(!metadata.policy.notification);
        assert!(!metadata.policy.response_mode);
        assert!(!metadata.policy.auto_response);
    }
}

fn signed_fixture(message_type: &str) -> rustain::domain::models::AgentEnvelope<serde_json::Value> {
    let signer = AgentSigner::from_signing_key(ed25519_dalek::SigningKey::from_bytes(&[7; 32]));
    let sender = AgentId::from_peer_path(&format!(
        "{}/baseline-sender",
        signer.identity().peer_id.as_str()
    ))
    .expect("peer-rooted sender");
    signer
        .sign(
            sender,
            AgentId::parse("baseline-recipient").expect("valid recipient"),
            CorrelationId::new("baseline-correlation"),
            MessageKind::PeerMessage,
            message_type.to_owned(),
            1,
            2_000_000_000,
            "baseline-nonce".to_owned(),
            Vec::new(),
            serde_json::json!({"alpha": 1, "nested": {"z": true}}),
        )
        .expect("fixture must sign")
}

#[test]
fn signed_header_type_is_additive_and_content_hash_remains_body_only() {
    let envelope = signed_fixture("consultation");
    assert_eq!(
        hex::encode(&envelope.header.content_hash),
        "315197c4f9e0bd665ebd4cf1bf7a042c856fac9cab5bf67025b7431a56f8c49a",
        "the semantic type must not enter the body-only content hash"
    );
    assert_ne!(
        hex::encode(entry_hash(&envelope.header).expect("canonical header")),
        "9f067e62115c460c919d0addc3d0e8ddf91322b48f38f58823497d5e57ac27ab",
        "the semantic type must enter the signed feed-entry header hash"
    );
}

#[test]
fn absent_signed_header_type_decodes_to_empty_and_serializes_explicitly() {
    let mut value = serde_json::to_value(signed_fixture("").header).expect("serializable header");
    assert_eq!(value["messageType"], "");
    value
        .as_object_mut()
        .expect("header object")
        .remove("messageType");

    let decoded: AgentEnvelopeHeader =
        serde_json::from_value(value).expect("an older header without the field must decode");
    assert_eq!(decoded.message_type, "");
    assert_eq!(
        serde_json::to_value(decoded).expect("serializable decoded header")["messageType"],
        "",
        "the defaulted field must not be omitted during canonical re-serialization"
    );
}

#[test]
fn unknown_signed_header_member_is_rejected_cryptographically_after_decode() {
    let envelope = signed_fixture("consultation");
    let mut header = serde_json::to_value(&envelope.header).expect("serializable header");
    header
        .as_object_mut()
        .expect("header object")
        .insert("futureMember".to_owned(), serde_json::json!(true));

    let header_bytes = serde_json::to_vec(&header).expect("canonical header");
    let payload_bytes = serde_json::to_vec(&envelope.body).expect("canonical payload");
    let mut signing_bytes = Vec::with_capacity(RAP_DOMAIN.len() + 64);
    signing_bytes.extend_from_slice(RAP_DOMAIN);
    signing_bytes.extend_from_slice(&Sha256::digest(header_bytes));
    signing_bytes.extend_from_slice(&Sha256::digest(payload_bytes));
    let signature = ed25519_dalek::SigningKey::from_bytes(&[7; 32]).sign(&signing_bytes);

    let mut value = serde_json::to_value(envelope).expect("serializable envelope");
    value["header"] = header;
    value["signature"] =
        serde_json::to_value(Ed25519Sig(signature.to_bytes().to_vec())).expect("signature value");
    let decoded: AgentEnvelope<serde_json::Value> =
        serde_json::from_value(value).expect("unknown member is not a serde-level refusal");

    assert!(
        matches!(
            verify_envelope(&decoded, 1_000, None),
            Err(VerifyError::BadSignature)
        ),
        "dropping an unknown signed member must invalidate canonical verification"
    );
}

#[test]
fn legacy_persisted_policy_snapshot_defaults_to_unknown_and_rewrites_explicitly() {
    let mut value =
        serde_json::to_value(InteractionPolicySnapshot::default()).expect("serializable snapshot");
    value
        .as_object_mut()
        .expect("snapshot object")
        .remove("message_type");

    let decoded: InteractionPolicySnapshot =
        serde_json::from_value(value).expect("legacy persisted snapshot must decode");
    assert_eq!(decoded.message_type, SemanticMessageType::Unknown);
    assert_eq!(
        serde_json::to_value(decoded).expect("serializable decoded snapshot")["message_type"],
        "unknown",
        "a migrated persisted row must carry the explicit Unknown domain value"
    );
}

#[test]
fn retract_path_cannot_reach_semantic_type_through_chat_message() {
    let source = std::fs::read_to_string("src/domain/models/conversation.rs")
        .expect("read conversation model");
    let fields = source
        .split_once("pub struct ChatMessage {")
        .expect("ChatMessage declaration")
        .1
        .split_once("\n}")
        .expect("ChatMessage closing brace")
        .0;

    for forbidden_field in ["message_type", "semantic_type", "semantic_message_type"] {
        assert!(
            !fields.contains(forbidden_field),
            "ChatMessage gained a semantic-type field `{forbidden_field}`; the retract guard \
             must remain structurally unable to consult peer-selected message semantics"
        );
    }
}
