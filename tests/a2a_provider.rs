#![cfg(feature = "a2a")]

use std::sync::Arc;

use rustain::adapters::a2a::client::A2aClientAdapter;
use rustain::adapters::a2a::provider::A2aProvider;
use rustain::domain::models::{A2aPeerSource, A2aPeerSpec, CapabilityId, RedactedUrl, TrustTier};
use rustain::domain::ports::CapabilityProvider;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn spec(url: String) -> A2aPeerSpec {
    A2aPeerSpec {
        id: "security-peer".to_owned(),
        url: RedactedUrl::from(url),
        pinned_key: None,
        source: A2aPeerSource::Workspace,
    }
}

#[tokio::test]
async fn cold_cache_discovers_nothing_without_network_io() {
    let peer = spec("http://127.0.0.1:9".to_owned());
    let client = Arc::new(A2aClientAdapter::new(&peer, None).expect("loopback client"));
    let provider = A2aProvider::new(vec![(peer, client)]);

    assert!(
        provider
            .discover()
            .await
            .expect("cold discovery")
            .is_empty()
    );
}

#[tokio::test]
async fn cached_skills_project_to_tier_stamped_a2a_capabilities() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/agent-card.json"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{
              "name":"Security Peer",
              "skills":[{
                "id":"scan",
                "name":"Security Scan",
                "description":"Scans a repository",
                "tags":["security"]
              }]
            }"#,
            "application/json",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let peer = spec(server.uri());
    let client = Arc::new(A2aClientAdapter::new(&peer, None).expect("client"));
    client.refresh_agent_card(&peer).await.expect("prime cache");
    let provider = A2aProvider::new(vec![(peer, client)]);
    drop(server);

    let capabilities = provider.discover().await.expect("pure cached discovery");
    assert_eq!(capabilities.len(), 1);
    let capability = &capabilities[0];
    assert_eq!(
        capability.id,
        CapabilityId {
            protocol: "a2a".to_owned(),
            server: "security-peer".to_owned(),
            tool: "scan".to_owned(),
        }
    );
    assert_eq!(capability.name, "Security Scan");
    assert_eq!(capability.description, "Scans a repository");
    assert_eq!(capability.trust, TrustTier::Unverified);
    assert_eq!(
        capability.input_schema["required"],
        serde_json::json!(["message"])
    );
    assert!(!capability.parallel_safe);
}

#[tokio::test]
async fn invocation_is_an_explicit_story_17_4b_refusal() {
    let provider = A2aProvider::new(Vec::new());
    let error = provider
        .invoke(
            &CapabilityId {
                protocol: "a2a".to_owned(),
                server: "peer".to_owned(),
                tool: "scan".to_owned(),
            },
            serde_json::json!({"message":"scan"}),
            CancellationToken::new(),
        )
        .await
        .expect_err("17.4a is discovery-only");

    assert!(error.to_string().contains("17.4b"));
}
