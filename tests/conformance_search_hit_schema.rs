#![cfg(feature = "meta-search")]

//! Phase B conformance — `SearchHit` schema lock-down per ADR-09-02 v2
//! §LLM-Only Payload + Mary amendment A2.
//!
//! Failure of any test here is the structural-countermeasure-failed signal:
//! a future maintainer has added a field to `SearchHit` that re-opens the
//! 2-stage discovery violation.

use rustain::domain::models::{capability_kind::CapabilityKind, search_hit::SearchHit};

#[test]
fn test_search_hit_serialization_field_set_locked() {
    // Construct a hit with ALL fields populated.
    let hit = SearchHit {
        name: "mcp__postgres__query".into(),
        kind: CapabilityKind::Tool,
        terse: "Run SQL against the configured PostgreSQL instance.".into(),
        score: 12.7,
        provider: Some("postgres".into()),
        matched_terms: Some(vec!["sql".into(), "postgres".into()]),
    };
    let v: serde_json::Value = serde_json::to_value(&hit).unwrap();
    let obj = v.as_object().expect("SearchHit serializes to JSON object");
    let mut keys: Vec<&str> = obj.keys().map(|s| s.as_str()).collect();
    keys.sort();
    assert_eq!(
        keys,
        vec!["kind", "matched_terms", "name", "provider", "score", "terse"],
        "SearchHit serialized field set MUST be exactly {{name, kind, terse, score, provider?, matched_terms?}} \
         per ADR-09-02 v2 §LLM-Only Payload + Mary amendment A2. \
         Adding fields silently re-opens the 2-stage discovery violation. \
         If you need a new field, re-open ADR-09-02 v3."
    );
}

#[test]
fn test_search_hit_omits_none_optionals_from_payload() {
    let hit = SearchHit::minimal("Bash", CapabilityKind::Tool, "Execute shell commands.", 8.4);
    let v: serde_json::Value = serde_json::to_value(&hit).unwrap();
    let obj = v.as_object().unwrap();
    let mut keys: Vec<&str> = obj.keys().map(|s| s.as_str()).collect();
    keys.sort();
    assert_eq!(
        keys,
        vec!["kind", "name", "score", "terse"],
        "Minimal SearchHit (provider=None, matched_terms=None) MUST serialize to exactly 4 keys"
    );
}

#[test]
fn test_search_hit_struct_field_count_is_six() {
    // Use Reflect-style check via serde introspection on the deserialization
    // surface: deserializing a JSON object with an UNEXPECTED field must
    // succeed (serde silently drops unknown fields by default), but the
    // RESULTING SearchHit's serialization must NOT contain the unknown field.
    // The combined invariant locks the on-wire field set.
    let json = serde_json::json!({
        "name": "test",
        "kind": "tool",
        "terse": "t",
        "score": 1.0,
        "description": "FORBIDDEN — should be silently dropped on deser",
        "input_schema": {"FORBIDDEN": true},
        "category": "FORBIDDEN",
    });
    let hit: SearchHit = serde_json::from_value(json).expect("deser succeeds, unknown fields silently dropped");
    let reserialized = serde_json::to_value(&hit).unwrap();
    let keys: Vec<&str> = reserialized.as_object().unwrap().keys().map(|s| s.as_str()).collect();
    assert!(!keys.contains(&"description"), "description MUST NOT round-trip — Mary A2 lock-down");
    assert!(!keys.contains(&"input_schema"), "input_schema MUST NOT round-trip");
    assert!(!keys.contains(&"category"), "category MUST NOT round-trip");
}

#[test]
fn test_search_hit_source_file_grep_for_forbidden_field_names() {
    // Grep src/domain/models/search_hit.rs for forbidden field tokens.
    // This catches the case where a maintainer adds a field but forgets to
    // run the round-trip test (e.g. behind a feature flag).
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/domain/models/search_hit.rs")
    ).unwrap();
    for forbidden in &["description:", "input_schema:", "parameters:", "version:", "category:", "icon:"] {
        assert!(
            !src.contains(forbidden),
            "src/domain/models/search_hit.rs contains forbidden field declaration '{}'. \
             Per ADR-09-02 v2 §LLM-Only Payload + Mary amendment A2, adding {} to SearchHit \
             re-opens the 2-stage discovery violation. Re-open ADR-09-02 v3 if you need this field.",
            forbidden, forbidden
        );
    }
}
