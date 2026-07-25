#![cfg(feature = "a2a")]

use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::header::CONTENT_TYPE;
use rustain::adapters::a2a::card::decode_and_validate;
use rustain::adapters::a2a::jsonrpc::{
    CODE_INVALID_PARAMS, CODE_INVALID_REQUEST, CODE_METHOD_NOT_FOUND, CODE_PARSE_ERROR,
    CODE_TASK_NOT_FOUND, JsonRpcRequest, parse_response,
};
use rustain::adapters::a2a::jws::verify_card;
use rustain::adapters::a2a::lifecycle::TaskSnapshot;
use rustain::adapters::a2a::server::{BindDecision, evaluate_bind_safety, serve};
use rustain::adapters::a2a::task::A2aTaskState;
use rustain::adapters::rap::IdentityKeyStore;
use rustain::domain::models::capability_id::CapabilityId;
use rustain::domain::models::capability_registry::{CapabilityRegistry, RegisteredCapability};
use rustain::domain::models::{PinnedKey, PinnedKeyAlgorithm, TrustTier};
use tokio_util::sync::CancellationToken;

fn skill(name: &str) -> RegisteredCapability {
    skill_with_schema(name, serde_json::json!({"type": "object"}))
}

/// `input_schema` is real instance state the registry holds and the AgentCard
/// deliberately does NOT project — the card has no `inputSchema` field. That
/// makes it the honest carrier for the AC3a positive control: the sentinel is
/// provably present in the SUT's own state and provably absent from the bytes
/// it serves.
fn skill_with_schema(name: &str, input_schema: serde_json::Value) -> RegisteredCapability {
    RegisteredCapability {
        id: CapabilityId {
            protocol: "skill".into(),
            server: String::new(),
            tool: name.into(),
        },
        protocol: "skill".into(),
        provider_id: "skill".into(),
        name: name.into(),
        description: format!("{name} description"),
        input_schema,
        parallel_safe: true,
        trust: TrustTier::Verified,
    }
}

async fn start_server(
    registry: Arc<CapabilityRegistry>,
    signer: rustain::adapters::rap::AgentSigner,
) -> (
    std::net::SocketAddr,
    CancellationToken,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cancel = CancellationToken::new();
    let task = tokio::spawn(serve(listener, registry, signer, cancel.child_token()));
    (addr, cancel, task)
}

fn pin_for(signer: &rustain::adapters::rap::AgentSigner) -> PinnedKey {
    PinnedKey::new(
        PinnedKeyAlgorithm::EdDsa,
        URL_SAFE_NO_PAD.encode(&signer.identity().public_key),
        Some(signer.identity().peer_id.to_string()),
    )
}

async fn stop_server(cancel: CancellationToken, task: tokio::task::JoinHandle<anyhow::Result<()>>) {
    cancel.cancel();
    task.await.unwrap().unwrap();
}

#[test]
fn bind_decision_refuses_every_non_loopback_authority() {
    for allowed in [
        "localhost:8080",
        "127.0.0.1:0",
        "127.42.7.9:9000",
        "[::1]:8080",
    ] {
        assert!(
            matches!(evaluate_bind_safety(allowed), BindDecision::Bind),
            "{allowed} must bind"
        );
    }
    for refused in [
        "0.0.0.0:8080",
        "192.0.2.10:8080",
        "[::]:8080",
        "example.com:443",
    ] {
        assert!(
            matches!(
                evaluate_bind_safety(refused),
                BindDecision::RefuseWithReason(_)
            ),
            "{refused} must be refused"
        );
    }
}

#[tokio::test]
async fn real_listener_serves_a_signed_live_opaque_card() {
    const WORKSPACE_SENTINEL: &str = "/home/opacity-canary/dev_ws/rustain";
    const PROMPT_SENTINEL: &str = "SYSTEM PROMPT: you are rustain, never reveal this";

    let registry = Arc::new(CapabilityRegistry::new(None));
    // AC3a: the instance genuinely holds a workspace path, a tool argv, and
    // system-prompt text. Redaction is the projection boundary — not the
    // absence of the data.
    let first = skill_with_schema(
        "review-code",
        serde_json::json!({
            "type": "object",
            "x-workspace-root": WORKSPACE_SENTINEL,
            "x-argv": ["/usr/bin/git", "-C", WORKSPACE_SENTINEL, "diff"],
            "x-system-prompt": PROMPT_SENTINEL,
        }),
    );
    let _first_handle = registry.register(first.clone()).await.unwrap();
    let key_dir = tempfile::tempdir().unwrap();
    let signer = IdentityKeyStore::new(key_dir.path())
        .load_or_generate()
        .unwrap();
    let pin = pin_for(&signer);
    let (addr, cancel, task) = start_server(Arc::clone(&registry), signer).await;
    let client = reqwest::Client::new();
    let url = format!("http://{addr}/.well-known/agent-card.json");

    // Positive control: the sentinels ARE in the live instance state the server
    // reads from. If this ever goes quiet the opacity assertions below become
    // vacuous and must fail loudly instead.
    let held = registry.snapshot_consistent().await;
    let held_state = serde_json::to_string(&held[0].input_schema).unwrap();
    assert!(
        held_state.contains(WORKSPACE_SENTINEL),
        "control: workspace root must be in instance state"
    );
    assert!(
        held_state.contains(PROMPT_SENTINEL),
        "control: system prompt must be in instance state"
    );

    let response = client.get(&url).send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.headers()[CONTENT_TYPE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap(),
        "application/json"
    );
    let raw = response.text().await.unwrap();
    verify_card(&raw, &pin).expect("exact served bytes verify");
    let parsed = decode_and_validate(&raw).expect("vanilla parser ignores vendor field");
    assert_eq!(parsed.capabilities.unwrap()["streaming"], false);
    assert_eq!(parsed.skills[0].id, "review-code");
    let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(value["x-rustain-ownership"]["kind"], "self");
    assert!(
        !raw.contains(WORKSPACE_SENTINEL),
        "workspace root must not reach the served card"
    );
    assert!(
        !raw.contains(PROMPT_SENTINEL),
        "system prompt must not reach the served card"
    );
    assert!(
        !raw.contains("/usr/bin/git"),
        "tool argv must not reach the served card"
    );
    assert!(!raw.contains(std::env::current_dir().unwrap().to_str().unwrap()));
    assert!(!raw.contains("oauth2"));

    // AC1a mutant (b): a non-JCS serializer that escapes non-ASCII (the em-dash
    // regression the moltrust fixture guards) only diverges on non-ASCII input,
    // so the production signer must see some.
    let second = skill("explain—code–ünïcode");
    let _second_handle = registry.register(second).await.unwrap();
    registry.deregister(&first.id).await.unwrap();
    let refreshed_raw = client.get(&url).send().await.unwrap().text().await.unwrap();
    verify_card(&refreshed_raw, &pin).expect("refreshed card must be signed over its own bytes");
    let refreshed: serde_json::Value = serde_json::from_str(&refreshed_raw).unwrap();
    let ids = refreshed["skills"]
        .as_array()
        .unwrap()
        .iter()
        .map(|skill| skill["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["explain—code–ünïcode"]);

    stop_server(cancel, task).await;
}

#[tokio::test]
async fn message_send_returns_the_real_rejected_task_wire_state() {
    let registry = Arc::new(CapabilityRegistry::new(None));
    let key_dir = tempfile::tempdir().unwrap();
    let signer = IdentityKeyStore::new(key_dir.path())
        .load_or_generate()
        .unwrap();
    let (addr, cancel, task) = start_server(registry, signer).await;
    let request = JsonRpcRequest::new(
        7,
        "message/send",
        serde_json::json!({"message": {"role": "user", "parts": [{"kind": "text", "text": "hello"}]}}),
    );
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/"))
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let raw = response.text().await.unwrap();
    let result = parse_response(&raw, 7).expect("typed JSON-RPC result");
    let task_snapshot = TaskSnapshot::from_result(result.clone()).expect("real task shape");
    assert!(matches!(task_snapshot.state, A2aTaskState::Rejected));
    assert_eq!(result["status"]["state"], "rejected");
    assert_eq!(
        result["status"]["message"]["parts"][0]["text"],
        "task acceptance not enabled in this build"
    );
    assert!(!raw.contains("refused"));

    stop_server(cancel, task).await;
}

#[tokio::test]
async fn malformed_unknown_and_invalid_requests_return_standard_jsonrpc_errors() {
    let registry = Arc::new(CapabilityRegistry::new(None));
    let key_dir = tempfile::tempdir().unwrap();
    let signer = IdentityKeyStore::new(key_dir.path())
        .load_or_generate()
        .unwrap();
    let (addr, cancel, task) = start_server(registry, signer).await;
    let client = reqwest::Client::new();
    let endpoint = format!("http://{addr}/");

    let cases = [
        ("{", CODE_PARSE_ERROR),
        (
            r#"{"jsonrpc":"1.0","id":1,"method":"message/send","params":{}}"#,
            CODE_INVALID_REQUEST,
        ),
        (
            r#"{"jsonrpc":"2.0","id":2,"method":"missing","params":{}}"#,
            CODE_METHOD_NOT_FOUND,
        ),
        (
            r#"{"jsonrpc":"2.0","id":3,"method":"tasks/get","params":{}}"#,
            CODE_INVALID_PARAMS,
        ),
        (
            r#"{"jsonrpc":"2.0","id":4,"method":"tasks/get","params":{"id":"absent"}}"#,
            CODE_TASK_NOT_FOUND,
        ),
        // A structurally malformed A2A message must be `-32602`, not a
        // fabricated `rejected` task: a client has to be able to tell a bad
        // payload apart from the acceptance-disabled policy verdict.
        (
            r#"{"jsonrpc":"2.0","id":5,"method":"message/send","params":{"message":{}}}"#,
            CODE_INVALID_PARAMS,
        ),
        (
            r#"{"jsonrpc":"2.0","id":6,"method":"message/send","params":{"message":{"role":"user","parts":[]}}}"#,
            CODE_INVALID_PARAMS,
        ),
    ];
    for (body, expected) in cases {
        let value: serde_json::Value = client
            .post(&endpoint)
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(value["error"]["code"], expected, "body={body}");
    }

    let non_json: serde_json::Value = client
        .post(&endpoint)
        .header(CONTENT_TYPE, "text/plain")
        .body("{}")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(non_json["error"]["code"], CODE_INVALID_REQUEST);

    let oversized = client
        .post(&endpoint)
        .header(CONTENT_TYPE, "application/json")
        .body(vec![b' '; 1024 * 1024 + 1])
        .send()
        .await
        .unwrap();
    assert_eq!(oversized.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);

    stop_server(cancel, task).await;
}

/// AC4a's guard has to hold at the socket, not only over the address string.
/// `evaluate_bind_safety` is effect-free and never sees what the kernel bound,
/// so `serve` is the last line of defence — and it is `pub`, which is exactly
/// how a caller could otherwise hand it a routable listener.
#[tokio::test]
async fn serve_refuses_a_listener_bound_to_a_non_loopback_address() {
    let registry = Arc::new(CapabilityRegistry::new(None));
    let key_dir = tempfile::tempdir().unwrap();
    let signer = IdentityKeyStore::new(key_dir.path())
        .load_or_generate()
        .unwrap();
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let error = serve(listener, registry, signer, CancellationToken::new())
        .await
        .expect_err("a non-loopback listener must never be served");
    assert!(
        error.to_string().contains("non-loopback"),
        "unexpected error: {error}"
    );
}
