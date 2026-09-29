#![cfg(feature = "a2a")]

use std::sync::Arc;

use rustain::adapters::a2a::auth::A2aServerAuth;
use rustain::adapters::a2a::card::{MAX_CARD_BYTES, MAX_DISCLOSED_SKILLS, decode_and_validate};
use rustain::adapters::a2a::card_cache::SignedCardCache;
use rustain::adapters::a2a::error::A2aError;
use rustain::adapters::rap::IdentityKeyStore;
use rustain::domain::models::capability_id::CapabilityId;
use rustain::domain::models::capability_registry::{CapabilityRegistry, RegisteredCapability};
use rustain::domain::models::{SecretString, TrustTier};

#[test]
fn missing_skills_is_a_typed_malformed_card_error() {
    let error = decode_and_validate(r#"{"name":"No Skills"}"#)
        .expect_err("missing skills must not silently become an empty card");
    assert!(matches!(
        error,
        A2aError::MalformedCard { ref field } if field == "skills"
    ));
}

#[test]
fn empty_skills_is_valid_but_missing_skill_id_is_not() {
    let card = decode_and_validate(r#"{"name":"Empty","skills":[]}"#)
        .expect("the specification permits an empty skills array");
    assert!(card.skills.is_empty());

    let error =
        decode_and_validate(r#"{"name":"Broken","skills":[{"name":"Scan","description":"ok"}]}"#)
            .expect_err("skill id is required");
    assert!(matches!(
        error,
        A2aError::MalformedCard { ref field } if field == "skills[0].id"
    ));
}

#[test]
fn unknown_fields_and_missing_best_effort_metadata_are_accepted() {
    let card = decode_and_validate(
        r#"{
          "name":"Forward Compatible",
          "vendorExtension":{"future":true},
          "supportedInterfaces":[{"protocolBinding":"FUTURE"}],
          "skills":[{"id":"scan","name":"Scan","pricing":{"amount":"1"}}]
        }"#,
    )
    .expect("unknown fields, missing description, and missing tags are allowed");

    assert_eq!(card.name, "Forward Compatible");
    assert_eq!(card.skills[0].id, "scan");
    assert!(card.skills[0].description.is_none());
    assert!(card.skills[0].tags.is_none());
}

fn oversized_skill(index: usize) -> RegisteredCapability {
    RegisteredCapability {
        id: CapabilityId {
            protocol: "skill".into(),
            server: String::new(),
            tool: format!("oversized-{index:03}"),
        },
        protocol: "skill".into(),
        provider_id: "test".into(),
        name: format!("oversized-{index:03}"),
        // 64 of these are deliberately larger than the public-card budget.
        // They contain no private sentinel: this test isolates cap behavior.
        description: "x".repeat(2_048),
        input_schema: serde_json::json!({}),
        parallel_safe: true,
        trust: TrustTier::Verified,
    }
}

#[tokio::test]
async fn served_signed_cards_are_bounded_truncated_and_deterministic() {
    let registry = Arc::new(CapabilityRegistry::new(None));
    let mut registrations = Vec::with_capacity(MAX_DISCLOSED_SKILLS);
    for index in 0..MAX_DISCLOSED_SKILLS {
        registrations.push(
            registry
                .register(oversized_skill(index))
                .await
                .expect("register oversized test skill"),
        );
    }

    let key_dir = tempfile::tempdir().expect("temporary signing-key directory");
    let signer = IdentityKeyStore::new(key_dir.path())
        .load_or_generate()
        .expect("test signing identity");
    let auth = A2aServerAuth::ApiKey {
        keys: vec![SecretString::from("test-api-key")],
    };
    let endpoint = "https://a2a.example.test:8443";

    let first = SignedCardCache::new()
        .signed(&registry, &signer, endpoint, Some(&auth))
        .await
        .expect("build first served card");
    let second = SignedCardCache::new()
        .signed(&registry, &signer, endpoint, Some(&auth))
        .await
        .expect("build second served card");

    assert!(
        first.len() <= MAX_CARD_BYTES,
        "signed card was {} bytes, above {MAX_CARD_BYTES}",
        first.len()
    );
    assert_eq!(
        first.as_ref(),
        second.as_ref(),
        "independent builds over one registry must have stable served bytes"
    );

    let card: serde_json::Value =
        serde_json::from_str(first.as_ref()).expect("served card is JSON");
    let truncation = &card["x-rustain-truncated"];
    assert!(
        truncation.is_object(),
        "over-cap projection must signal truncation"
    );
    assert_eq!(
        truncation["totalSkills"],
        serde_json::json!(MAX_DISCLOSED_SKILLS)
    );
    assert!(
        truncation["disclosedSkills"]
            .as_u64()
            .expect("numeric disclosed skill count")
            < MAX_DISCLOSED_SKILLS as u64,
        "byte-budget trimming must drop at least one oversized skill"
    );
    decode_and_validate(first.as_ref()).expect("bounded signed card remains valid");

    // Keep the RAII registrations alive until both projections have completed.
    assert_eq!(registrations.len(), MAX_DISCLOSED_SKILLS);
}
