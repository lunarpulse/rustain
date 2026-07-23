#![cfg(feature = "a2a")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rustain::adapters::a2a::card::{AgentCardView, decode_and_validate};
use rustain::adapters::a2a::endpoint::resolve_jsonrpc_endpoint;
use rustain::adapters::a2a::jws::verify_card;
use rustain::domain::models::{PinnedKey, PinnedKeyAlgorithm};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const EXPECTED_FIXTURES: &[&str] = &[
    "CASSETTE_completed_arc_jsonrpc.json",
    "CORPUS_141_live_cards_2026-07-17.json",
    "FIXTURE_moltrust_v1.0_agent-card.json",
    "FIXTURE_moltrust_v1.0_jwks.json",
    "FIXTURE_planets_hybrid-v1.0-card.json",
    "FIXTURE_planets_v0.3_jwks.json",
    "README.md",
    "REPRODUCE_TEST_SIGNATURE.sh",
    "REVERIFY_REAL_SIGNATURES.sh",
    "TEST_ONLY_ed25519_seed.hex",
];

#[derive(Debug, Deserialize)]
struct FixtureManifestEntry {
    sha256: String,
    #[serde(default)]
    provenance: Option<FixtureProvenance>,
}

#[derive(Debug, Deserialize)]
struct FixtureProvenance {
    original_filename: String,
    capture_date: String,
    fetch_path: String,
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/a2a")
}

fn manifest() -> BTreeMap<String, FixtureManifestEntry> {
    serde_json::from_slice(
        &std::fs::read(fixture_dir().join("manifest.json")).expect("read A2A fixture manifest"),
    )
    .expect("valid A2A fixture manifest")
}

fn load_fixture(name: &str) -> Vec<u8> {
    let bytes = std::fs::read(fixture_dir().join(name)).expect("read pinned A2A fixture");
    let expected = manifest()
        .remove(name)
        .unwrap_or_else(|| panic!("fixture {name} is absent from manifest"));
    assert_eq!(
        digest(&bytes),
        expected.sha256,
        "fixture {name} hash drifted"
    );
    bytes
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn pin_from_jwks(name: &str, kid: &str) -> PinnedKey {
    let jwks: serde_json::Value = serde_json::from_slice(&load_fixture(name)).unwrap();
    let key = jwks["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|key| key["kid"].as_str() == Some(kid))
        .unwrap_or_else(|| panic!("JWKS {name} has no key {kid:?}"));
    assert_eq!(key["kty"], "OKP");
    assert_eq!(key["crv"], "Ed25519");
    PinnedKey::new(
        PinnedKeyAlgorithm::EdDsa,
        key["x"].as_str().unwrap().to_owned(),
        Some(kid.to_owned()),
    )
}

#[test]
fn manifest_covers_every_pinned_artifact_and_detects_a_byte_flip() {
    let manifest = manifest();
    let names: Vec<_> = manifest.keys().map(String::as_str).collect();
    assert_eq!(names, EXPECTED_FIXTURES);
    let mut actual: Vec<String> = std::fs::read_dir(fixture_dir())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name != "manifest.json")
        .collect();
    actual.sort();
    assert_eq!(
        actual,
        EXPECTED_FIXTURES
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>()
    );
    let hybrid_provenance = manifest["FIXTURE_planets_hybrid-v1.0-card.json"]
        .provenance
        .as_ref()
        .expect("hybrid fixture provenance");
    assert_eq!(
        hybrid_provenance.original_filename,
        "FIXTURE_planets_v0.3_agent-card.json"
    );
    assert_eq!(hybrid_provenance.capture_date, "2026-07-17");
    assert_eq!(
        hybrid_provenance.fetch_path,
        "https://planets-agent.ai/.well-known/agent.json"
    );
    for name in EXPECTED_FIXTURES {
        load_fixture(name);
    }

    let mut mutated = load_fixture("FIXTURE_moltrust_v1.0_agent-card.json");
    mutated[0] ^= 1;
    assert_ne!(
        digest(&mutated),
        manifest["FIXTURE_moltrust_v1.0_agent-card.json"].sha256,
        "integrity test must reject a controlled byte flip"
    );
}

#[test]
fn both_captured_real_card_shapes_verify_offline() {
    let cases = [
        (
            "FIXTURE_moltrust_v1.0_agent-card.json",
            "FIXTURE_moltrust_v1.0_jwks.json",
        ),
        (
            "FIXTURE_planets_hybrid-v1.0-card.json",
            "FIXTURE_planets_v0.3_jwks.json",
        ),
    ];
    for (card_name, jwks_name) in cases {
        let bytes = load_fixture(card_name);
        let raw = std::str::from_utf8(&bytes).unwrap();
        let card: serde_json::Value = serde_json::from_str(raw).unwrap();
        let protected = card["signatures"][0]["protected"].as_str().unwrap();
        let header: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(protected).unwrap()).unwrap();
        let kid = header["kid"].as_str().unwrap();
        verify_card(raw, &pin_from_jwks(jwks_name, kid))
            .unwrap_or_else(|error| panic!("{card_name} failed strict JCS verification: {error}"));
        decode_and_validate(raw)
            .unwrap_or_else(|error| panic!("{card_name} failed production decode: {error}"));
    }
}

#[test]
fn pinned_corpus_meets_the_134_of_141_decoder_threshold() {
    let corpus: serde_json::Value =
        serde_json::from_slice(&load_fixture("CORPUS_141_live_cards_2026-07-17.json")).unwrap();
    let entries = corpus.as_array().unwrap();
    assert_eq!(entries.len(), 141);
    let decoded = entries
        .iter()
        .filter(|entry| {
            entry
                .get("card")
                .and_then(|card| serde_json::to_string(card).ok())
                .is_some_and(|raw| decode_and_validate(&raw).is_ok())
        })
        .count();
    assert!(decoded >= 134, "decoded only {decoded}/141 pinned cards");
}

#[test]
fn corpus_endpoint_resolution_meets_the_138_of_141_threshold() {
    // AC1 (Ruling 2): endpoint resolution proven against the committed corpus at
    // >= 138/141. The 3 known non-resolvers publish no JSON-RPC binding (two have
    // no url at all; one is HTTP+JSON-only) and MUST refuse cleanly — never a
    // fallback guess. Both reach numbers must be stated wherever reach is claimed:
    // 98% resolve an endpoint, but only ~80% are speakable by the JSON-RPC dialect.
    let corpus: serde_json::Value =
        serde_json::from_slice(&load_fixture("CORPUS_141_live_cards_2026-07-17.json")).unwrap();
    let entries = corpus.as_array().unwrap();
    assert_eq!(entries.len(), 141);

    let mut resolved = 0usize;
    let mut refused = 0usize;
    for entry in entries {
        // Endpoint resolution never consults `skills`; strip them so a card whose
        // skills fail the strict decoder (7/141) still measures the resolver.
        let mut card_json = entry.get("card").unwrap().clone();
        card_json["skills"] = serde_json::json!([]);
        let card: AgentCardView = serde_json::from_str(&card_json.to_string()).unwrap();
        match resolve_jsonrpc_endpoint(&card) {
            Ok(endpoint) => {
                assert!(
                    endpoint.url().starts_with("http://") || endpoint.url().starts_with("https://"),
                    "resolved endpoint must carry a scheme: {}",
                    endpoint.url()
                );
                resolved += 1;
            }
            Err(_) => refused += 1,
        }
    }
    assert_eq!(resolved + refused, 141);
    assert!(
        resolved >= 138,
        "endpoint resolution regressed to {resolved}/141 (must be >= 138)"
    );
    assert_eq!(refused, 3, "exactly the 3 known non-resolvers must refuse");
}

#[test]
fn committed_test_key_is_deterministic_and_obviously_non_secret() {
    let seed = String::from_utf8(load_fixture("TEST_ONLY_ed25519_seed.hex")).unwrap();
    assert_eq!(seed.trim(), "07".repeat(32));
    let bytes = hex::decode(seed.trim()).unwrap();
    assert_eq!(bytes.len(), 32);
    assert_eq!(
        URL_SAFE_NO_PAD.encode(bytes),
        "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc"
    );
}
