#![cfg(feature = "a2a")]

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signer, SigningKey};
use rustain::adapters::a2a::card::{ServedAgentCard, decode_and_validate};
use rustain::adapters::a2a::error::A2aError;
use rustain::adapters::a2a::jws::{sign_card, verify_card};
use rustain::adapters::rap::IdentityKeyStore;
use rustain::domain::models::capability_id::CapabilityId;
use rustain::domain::models::capability_registry::{CapabilityRegistry, RegisteredCapability};
use rustain::domain::models::{PinnedKey, PinnedKeyAlgorithm, TrustTier};
use sha2::{Digest, Sha256};
use std::sync::Arc;

fn test_signing_key() -> SigningKey {
    let fixture_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/a2a");
    let seed_bytes =
        std::fs::read(fixture_dir.join("TEST_ONLY_ed25519_seed.hex")).expect("read test seed");
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(fixture_dir.join("manifest.json")).expect("read fixture manifest"),
    )
    .expect("valid fixture manifest");
    assert_eq!(
        hex::encode(Sha256::digest(&seed_bytes)),
        manifest["TEST_ONLY_ed25519_seed.hex"]["sha256"]
            .as_str()
            .unwrap()
    );
    let seed = hex::decode(std::str::from_utf8(&seed_bytes).unwrap().trim()).unwrap();
    SigningKey::from_bytes(&seed.try_into().expect("32-byte test seed"))
}

fn pin_for(signing_key: &SigningKey, kid: Option<&str>) -> PinnedKey {
    PinnedKey::new(
        PinnedKeyAlgorithm::EdDsa,
        URL_SAFE_NO_PAD.encode(signing_key.verifying_key().to_bytes()),
        kid.map(str::to_owned),
    )
}

fn signed_card(signing_key: &SigningKey, kid: &str) -> String {
    let mut card = serde_json::json!({
        "name": "Signed — Agent",
        "skills": [{"id": "scan", "name": "Scan"}],
        "vendorExtension": {"future": true},
        "security": []
    });
    let protected = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&serde_json::json!({"alg":"EdDSA","kid":kid}))
            .expect("serialize protected header"),
    );
    let payload = serde_jcs::to_vec(&card).expect("JCS payload");
    let signing_input = format!("{protected}.{}", URL_SAFE_NO_PAD.encode(payload));
    let signature = signing_key.sign(signing_input.as_bytes());
    card.as_object_mut().expect("card object").insert(
        "signatures".to_owned(),
        serde_json::json!([{
            "protected": protected,
            "signature": URL_SAFE_NO_PAD.encode(signature.to_bytes())
        }]),
    );
    serde_json::to_string(&card).expect("serialize signed card")
}

#[test]
fn valid_card_verifies_and_tampering_is_refused() {
    let key = test_signing_key();
    let raw = signed_card(&key, "test-key");
    let pin = pin_for(&key, Some("test-key"));
    verify_card(&raw, &pin).expect("valid signature");

    let tampered = raw.replace("Signed — Agent", "Forged Agent");
    assert!(matches!(
        verify_card(&tampered, &pin),
        Err(A2aError::BadSignature)
    ));

    let mut forged: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let encoded = forged["signatures"][0]["signature"].as_str().unwrap();
    let mut signature = URL_SAFE_NO_PAD.decode(encoded).unwrap();
    signature[0] ^= 1;
    forged["signatures"][0]["signature"] =
        serde_json::Value::String(URL_SAFE_NO_PAD.encode(signature));
    assert!(matches!(
        verify_card(&forged.to_string(), &pin),
        Err(A2aError::BadSignature)
    ));
}

#[test]
fn stripping_wrong_key_wrong_algorithm_and_kid_mismatch_are_refused() {
    let key = test_signing_key();
    let raw = signed_card(&key, "test-key");
    let pin = pin_for(&key, Some("test-key"));

    let mut stripped: serde_json::Value = serde_json::from_str(&raw).unwrap();
    stripped.as_object_mut().unwrap().remove("signatures");
    assert!(matches!(
        verify_card(&stripped.to_string(), &pin),
        Err(A2aError::MissingSignatures)
    ));

    let wrong = SigningKey::from_bytes(&[8; 32]);
    assert!(matches!(
        verify_card(&raw, &pin_for(&wrong, Some("test-key"))),
        Err(A2aError::BadSignature)
    ));

    let mut wrong_alg: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let protected = wrong_alg["signatures"][0]["protected"]
        .as_str()
        .expect("protected header");
    let mut header: serde_json::Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(protected).unwrap()).unwrap();
    header["alg"] = serde_json::Value::String("ES256".to_owned());
    wrong_alg["signatures"][0]["protected"] =
        serde_json::Value::String(URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap()));
    assert!(matches!(
        verify_card(&wrong_alg.to_string(), &pin),
        Err(A2aError::UnsupportedAlgorithm { .. })
    ));

    assert!(matches!(
        verify_card(&raw, &pin_for(&key, Some("other-key"))),
        Err(A2aError::KeyIdMismatch { .. })
    ));
}

#[test]
fn a_garbage_rotation_entry_does_not_mask_a_later_valid_signature() {
    let key = test_signing_key();
    let raw = signed_card(&key, "test-key");
    let pin = pin_for(&key, Some("test-key"));
    let mut value: serde_json::Value = serde_json::from_str(&raw).unwrap();
    value["signatures"].as_array_mut().unwrap().insert(
        0,
        serde_json::json!({"protected":"garbage","signature":"garbage"}),
    );

    verify_card(&value.to_string(), &pin).expect("any valid rotation entry is sufficient");
}

#[tokio::test]
async fn production_card_signer_projects_the_live_registry_without_private_state() {
    let registry = Arc::new(CapabilityRegistry::new(None));
    let capability = RegisteredCapability {
        id: CapabilityId {
            protocol: "skill".into(),
            server: String::new(),
            tool: "review-code".into(),
        },
        protocol: "skill".into(),
        provider_id: "skill".into(),
        name: "review-code".into(),
        description: "Review a code change".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "x-workspace-root": "/private/project",
        }),
        parallel_safe: true,
        trust: TrustTier::Verified,
    };
    let _registration = registry.register(capability.clone()).await.unwrap();

    let key_dir = tempfile::tempdir().unwrap();
    let signer = IdentityKeyStore::new(key_dir.path())
        .load_or_generate()
        .unwrap();
    // Positive control (AC3a): the sentinel must genuinely live in the state
    // the signer reads from, otherwise its absence from the card proves
    // nothing. A local string asserted against itself is not a control.
    let held = registry.snapshot_consistent().await;
    let private_state = serde_json::to_string(&held[0].input_schema).unwrap();
    assert!(
        private_state.contains("/private/project"),
        "control: the workspace path must be present in live instance state"
    );
    let card = ServedAgentCard::from_registry(&registry, "http://127.0.0.1:8080").await;
    let raw = sign_card(&card, &signer).unwrap();
    let pin = PinnedKey::new(
        PinnedKeyAlgorithm::EdDsa,
        URL_SAFE_NO_PAD.encode(&signer.identity().public_key),
        Some(signer.identity().peer_id.to_string()),
    );

    verify_card(&raw, &pin).expect("production-signed card verifies");
    let decoded = decode_and_validate(&raw).expect("served card remains vanilla-parseable");
    assert_eq!(
        decoded
            .skills
            .iter()
            .map(|skill| skill.id.as_str())
            .collect::<Vec<_>>(),
        vec!["review-code"]
    );
    assert_eq!(decoded.capabilities.unwrap()["streaming"], false);
    assert!(!raw.contains("/private/project"));
    assert!(!raw.contains("oauth2"));

    registry.deregister(&capability.id).await.unwrap();
    let refreshed = ServedAgentCard::from_registry(&registry, "http://127.0.0.1:8080").await;
    let refreshed_raw = sign_card(&refreshed, &signer).unwrap();
    let refreshed_decoded = decode_and_validate(&refreshed_raw).unwrap();
    assert!(refreshed_decoded.skills.is_empty());
}
