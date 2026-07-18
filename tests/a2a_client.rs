#![cfg(feature = "a2a")]

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signer, SigningKey};
use rustain::adapters::a2a::client::A2aClientAdapter;
use rustain::adapters::a2a::error::A2aError;
use rustain::domain::models::{
    A2aPeerSource, A2aPeerSpec, PinnedKey, PinnedKeyAlgorithm, RedactedUrl, TrustTier,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn spec(url: String, pinned_key: Option<PinnedKey>) -> A2aPeerSpec {
    A2aPeerSpec {
        id: "remote-peer".to_owned(),
        url: RedactedUrl::from(url),
        pinned_key,
        source: A2aPeerSource::Workspace,
    }
}

fn signed_card(key: &SigningKey, kid: &str) -> String {
    let mut card = serde_json::json!({
        "name":"Remote Peer",
        "skills":[{"id":"scan","name":"Scan"}]
    });
    let protected = URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&serde_json::json!({"alg":"EdDSA","kid":kid})).unwrap());
    let payload = serde_jcs::to_vec(&card).unwrap();
    let input = format!("{protected}.{}", URL_SAFE_NO_PAD.encode(payload));
    let signature = key.sign(input.as_bytes());
    card["signatures"] = serde_json::json!([{
        "protected": protected,
        "signature": URL_SAFE_NO_PAD.encode(signature.to_bytes())
    }]);
    card.to_string()
}

fn pin(key: &SigningKey, kid: &str) -> PinnedKey {
    PinnedKey::new(
        PinnedKeyAlgorithm::EdDsa,
        URL_SAFE_NO_PAD.encode(key.verifying_key().to_bytes()),
        Some(kid.to_owned()),
    )
}

#[tokio::test]
async fn fetches_exact_well_known_path_and_caches_unverified_card() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/agent-card.json"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"name":"Remote","skills":[{"id":"scan","name":"Scan"}]}"#,
            "application/json",
        ))
        .expect(1)
        .mount(&server)
        .await;

    let peer = spec(server.uri(), None);
    let client = A2aClientAdapter::new(&peer, None).expect("loopback HTTP is allowed");
    client.refresh_agent_card(&peer).await.expect("fetch card");
    let (card, trust) = client.cached_card().await.expect("cached card");
    assert_eq!(card.name, "Remote");
    assert_eq!(trust, TrustTier::Unverified);
}

#[tokio::test]
async fn refresh_recomputes_tier_after_the_pin_is_removed() {
    let key = SigningKey::from_bytes(&[11; 32]);
    let raw = signed_card(&key, "peer-key");
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/agent-card.json"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(raw, "application/json"))
        .expect(2)
        .mount(&server)
        .await;

    let verified = spec(server.uri(), Some(pin(&key, "peer-key")));
    let client = A2aClientAdapter::new(&verified, None).expect("client");
    client
        .refresh_agent_card(&verified)
        .await
        .expect("verified refresh");
    assert_eq!(client.cached_card().await.unwrap().1, TrustTier::Verified);

    let unverified = spec(server.uri(), None);
    client
        .refresh_agent_card(&unverified)
        .await
        .expect("unverified refresh");
    assert_eq!(
        client.cached_card().await.unwrap().1,
        TrustTier::Unverified,
        "trust must come from current config, not cached card state"
    );
}

#[tokio::test]
async fn pinned_unsigned_and_html_responses_clear_the_cache() {
    let key = SigningKey::from_bytes(&[12; 32]);
    let unsigned_server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(r#"{"name":"Unsigned","skills":[]}"#, "application/json"),
        )
        .mount(&unsigned_server)
        .await;
    let pinned = spec(unsigned_server.uri(), Some(pin(&key, "peer-key")));
    let client = A2aClientAdapter::new(&pinned, None).expect("client");
    assert!(matches!(
        client.refresh_agent_card(&pinned).await,
        Err(A2aError::MissingSignatures)
    ));
    assert!(client.cached_card().await.is_none());

    let html_server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("<html>soft 404</html>", "text/html"))
        .mount(&html_server)
        .await;
    let unverified = spec(html_server.uri(), None);
    let client = A2aClientAdapter::new(&unverified, None).expect("client");
    assert!(matches!(
        client.refresh_agent_card(&unverified).await,
        Err(A2aError::UnexpectedContentType { .. })
    ));
    assert!(client.cached_card().await.is_none());
}

#[test]
fn plain_http_is_allowed_only_for_loopback_authorities() {
    let public_http = spec("http://example.com".to_owned(), None);
    assert!(matches!(
        A2aClientAdapter::new(&public_http, None),
        Err(A2aError::UnsafeUrl { .. })
    ));

    let loopback = spec("http://127.0.0.1:9999".to_owned(), None);
    A2aClientAdapter::new(&loopback, None).expect("loopback manual-test server is allowed");
}

#[tokio::test]
async fn redirects_are_revalidated_before_the_next_request() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/agent-card.json"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", "http://example.com/forbidden-card"),
        )
        .mount(&server)
        .await;
    let peer = spec(server.uri(), None);
    let client = A2aClientAdapter::new(&peer, None).expect("loopback client");

    assert!(matches!(
        client.refresh_agent_card(&peer).await,
        Err(A2aError::UnsafeUrl { .. })
    ));
}

#[tokio::test]
async fn oversized_card_body_is_refused_before_decode() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(vec![b'x'; 1024 * 1024 + 1], "application/json"),
        )
        .mount(&server)
        .await;
    let peer = spec(server.uri(), None);
    let client = A2aClientAdapter::new(&peer, None).expect("loopback client");

    assert!(matches!(
        client.refresh_agent_card(&peer).await,
        Err(A2aError::BodyTooLarge { .. })
    ));
}

#[test]
fn unusable_pinned_key_is_a_loud_construction_error() {
    let peer = spec(
        "https://peer.example".to_owned(),
        Some(PinnedKey::new(
            PinnedKeyAlgorithm::EdDsa,
            "not-a-32-byte-ed25519-key".to_owned(),
            None,
        )),
    );

    assert!(matches!(
        A2aClientAdapter::new(&peer, None),
        Err(A2aError::InvalidPinnedKey)
    ));
}
