//! Story 17.4b (AC1, AC2) — end-to-end A2A task-lifecycle over real HTTP.
//!
//! Unlike the driver unit tests (scripted in-memory transport), this drives the
//! full stack — `A2aProvider::invoke` → `A2aDelegationRuntime` → `TaskClient` →
//! `reqwest` POST → JSON-RPC demux → poll loop → `NodeState` projection — against
//! a `wiremock` server whose responses are the committed real-peer **cassette**
//! (`tests/fixtures/a2a/CASSETTE_completed_arc_jsonrpc.json`, seeded from the
//! Task 0b spike captures). The cassette is served with a **strict sequential
//! cursor**: `message/send` then two ordered `tasks/get` (working → completed);
//! a lenient matcher that hid the `working → completed` transition would leave a
//! `tasks/get` interaction unconsumed and fail the server's `.expect(1)` verify.

#![cfg(feature = "a2a")]

use std::sync::Arc;
use std::time::Duration;

use rustain::adapters::a2a::client::A2aClientAdapter;
use rustain::adapters::a2a::driver::A2aDelegationRuntime;
use rustain::adapters::a2a::provider::A2aProvider;
use rustain::domain::events::AppEvent;
use rustain::domain::models::{A2aPeerSource, A2aPeerSpec, CapabilityId, NodeState, RedactedUrl};
use rustain::domain::ports::CapabilityProvider;
use rustain::infrastructure::subagent::NodeTree;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;

fn cassette() -> serde_json::Value {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/a2a/CASSETTE_completed_arc_jsonrpc.json"
    ))
    .expect("read cassette");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("cassette is valid JSON");

    // No-secrets gate on the committed cassette (Task 12) — scan the served
    // payload (the `interactions`), not the human-readable provenance metadata.
    let payload = serde_json::to_string(&parsed["interactions"])
        .unwrap()
        .to_lowercase();
    for needle in [
        "-----begin",
        "api_key",
        "apikey",
        "bearer ",
        "sk-",
        "authorization",
    ] {
        assert!(
            !payload.contains(needle),
            "cassette interactions must not embed secrets (found {needle:?})"
        );
    }
    parsed
}

fn jsonrpc_ok(result: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "jsonrpc": "2.0", "id": 1, "result": result })
}

#[tokio::test]
async fn cassette_drives_message_send_then_ordered_tasks_get_to_completed() {
    let cassette = cassette();
    let interactions = cassette["interactions"].as_array().unwrap();
    assert_eq!(interactions[0]["method"], "message/send");
    assert_eq!(interactions[1]["method"], "tasks/get");
    assert_eq!(interactions[2]["method"], "tasks/get");

    let server = MockServer::start().await;

    // AgentCard discovery → resolves the JSON-RPC endpoint to the mock's /jsonrpc.
    let card = format!(
        r#"{{"name":"Cassette Peer","skills":[{{"id":"summarize","name":"Summarize"}}],
            "url":"{}/jsonrpc","preferredTransport":"JSONRPC"}}"#,
        server.uri()
    );
    Mock::given(method("GET"))
        .and(path("/.well-known/agent-card.json"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(card, "application/json"))
        .mount(&server)
        .await;

    // message/send → the submitted task (interaction 0). The response id echoes
    // the request id (correlation round-trip) — the JsonRpcRequest counter starts
    // at 1 for message/send, 2 and 3 for the two tasks/get.
    Mock::given(method("POST"))
        .and(path("/jsonrpc"))
        .and(body_string_contains(r#""method":"message/send""#))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(with_id(1, &interactions[0]["result"])),
        )
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;

    // First tasks/get → working (id 2). Higher priority so it is consumed first.
    Mock::given(method("POST"))
        .and(path("/jsonrpc"))
        .and(body_string_contains(r#""method":"tasks/get""#))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(with_id(2, &interactions[1]["result"])),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;

    // Second tasks/get → completed (id 3). Lower priority; matches once working
    // is satisfied. A lenient matcher hiding this transition would leave it
    // unconsumed → `.expect(1)` fails on server drop.
    Mock::given(method("POST"))
        .and(path("/jsonrpc"))
        .and(body_string_contains(r#""method":"tasks/get""#))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(with_id(3, &interactions[2]["result"])),
        )
        .up_to_n_times(1)
        .with_priority(2)
        .expect(1)
        .mount(&server)
        .await;

    let peer = A2aPeerSpec {
        id: "cassette-peer".to_owned(),
        url: RedactedUrl::from(server.uri()),
        pinned_key: None,
        source: A2aPeerSource::Workspace,
    };
    let client = Arc::new(A2aClientAdapter::new(&peer, None).unwrap());
    client.refresh_agent_card(&peer).await.unwrap();

    let tree = NodeTree::new();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    let runtime = Arc::new(A2aDelegationRuntime::new(tree.clone(), None, tx));
    let provider = A2aProvider::new(vec![(peer.clone(), client)]);
    provider.set_delegation_runtime(runtime);

    let id = CapabilityId {
        protocol: "a2a".to_owned(),
        server: "cassette-peer".to_owned(),
        tool: "summarize".to_owned(),
    };
    let result = provider
        .invoke(
            &id,
            serde_json::json!({ "message": "summarize the corpus" }),
            CancellationToken::new(),
        )
        .await
        .expect("delegation drives to completion over HTTP");

    assert!(!result.is_error);
    assert!(
        result.content.contains("141 parseable agent cards"),
        "the completed artifact text must round-trip: {}",
        result.content
    );

    // The peer node reached Completed via the live projection.
    let entry = tree
        .list()
        .await
        .into_iter()
        .find(|e| e.subagent_type == "a2a-peer")
        .expect("peer node materialized");
    assert_eq!(entry.current_status, NodeState::Completed);

    // A RemoteEnvelopeAccepted domain event was emitted — awaited via the shared
    // correlation-keyed helper, never a fire-and-assert sleep (AC2).
    let accepted = common::expect_event_matching(
        &mut rx,
        |event| {
            matches!(event, AppEvent::DomainEvent(payload)
                if format!("{payload:?}").contains("RemoteEnvelopeAccepted"))
        },
        Duration::from_secs(5),
    )
    .await;
    assert!(accepted.is_some(), "RemoteEnvelopeAccepted must be emitted");
}

fn with_id(id: u64, result: &serde_json::Value) -> serde_json::Value {
    let mut envelope = jsonrpc_ok(result);
    envelope["id"] = serde_json::json!(id);
    envelope
}
