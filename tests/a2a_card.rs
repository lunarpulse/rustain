#![cfg(feature = "a2a")]

use rustain::adapters::a2a::card::decode_and_validate;
use rustain::adapters::a2a::error::A2aError;

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
