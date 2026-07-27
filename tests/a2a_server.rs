#![cfg(feature = "a2a")]

use std::sync::Arc;
use std::time::Duration;

use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::a2a::auth::A2aServerSecurity;
use rustain::adapters::a2a::card_cache::SignedCardCache;
use rustain::adapters::a2a::transparency::{InboundOutcome, TransparencySink};

use arc_swap::ArcSwap;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::header::CONTENT_TYPE;
use rustain::adapters::a2a::auth::{BindDecision, BindEvidence, evaluate_bind_safety};
use rustain::adapters::a2a::card::decode_and_validate;
use rustain::adapters::a2a::jsonrpc::{
    CODE_INVALID_PARAMS, CODE_INVALID_REQUEST, CODE_METHOD_NOT_FOUND, CODE_PARSE_ERROR,
    CODE_TASK_NOT_FOUND, JsonRpcRequest, parse_response,
};
use rustain::adapters::a2a::jws::verify_card;
use rustain::adapters::a2a::lifecycle::TaskSnapshot;
use rustain::adapters::a2a::server::{ServeConfig, serve};
use rustain::adapters::a2a::task::A2aTaskState;
use rustain::adapters::rap::{
    IdentityKeyStore, VerifiedPeerConsent, VerifiedPeerConsumer, VerifiedPeerFrameHandler,
};
use rustain::domain::models::capability_id::CapabilityId;
use rustain::domain::models::capability_registry::{CapabilityRegistry, RegisteredCapability};
use rustain::domain::models::{
    AgentEnvelope, AgentEnvelopeHeader, AgentId, AgentMessage, CorrelationId, Ed25519Sig,
    MessageKind, NodeState, PeerId, PeerIdentity,
};
use rustain::domain::models::{PinnedKey, PinnedKeyAlgorithm, TrustTier};
use rustain::domain::ports::{
    AgentMessageBus, DeliveryPolicy, InboundApprovalTicket, InboundPeerError, InboundPeerRuntime,
    InboundPeerTask, PeerInteractionRecorder, RelationshipDeliveryPolicy, RoomJournal,
};
use rustain::domain::services::transparency::{TransparencyKind, fold_transparency};
use rustain::infrastructure::agent_message_bus::LocalMessageBus;
use rustain::infrastructure::subagent::{NodeJournal, NodeRoomJournal, NodeTree};
use tokio::sync::Notify;
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
    // Story 18.1a's posture, still exercised end to end: loopback, plaintext,
    // no execution core. Story 18.1b's execution keystones live in
    // `tests/a2a_server_exec.rs`, which composes a real core behind the same
    // `serve`.
    let config =
        ServeConfig::discovery_only(registry, signer, std::env::current_dir().expect("cwd"));
    let task = tokio::spawn(serve(listener, config, cancel.child_token()));
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

/// Runtime whose terminal transition is test-controlled, so the listener must
/// serve a real `working` poll before it can disclose the completed result.
struct DisclosureRuntime {
    complete: Arc<Notify>,
    completed: Arc<Notify>,
    result: String,
}

#[async_trait::async_trait]
impl InboundPeerRuntime for DisclosureRuntime {
    async fn start(
        &self,
        _task: InboundPeerTask,
        _cancel: CancellationToken,
    ) -> Result<tokio::sync::watch::Receiver<NodeState>, InboundPeerError> {
        let (state_tx, state_rx) = tokio::sync::watch::channel(NodeState::Running);
        let complete = self.complete.clone();
        let completed = self.completed.clone();
        tokio::spawn(async move {
            complete.notified().await;
            let _ = state_tx.send(NodeState::Completed);
            completed.notify_one();
        });
        Ok(state_rx)
    }

    async fn request_admission_approval(
        &self,
        _peer_id: &PeerId,
        _summary: &str,
    ) -> Result<InboundApprovalTicket, InboundPeerError> {
        Err(InboundPeerError::unavailable(
            "approval is not used by this runtime",
        ))
    }

    async fn take_result_text(&self, _node_id: &AgentId) -> Option<String> {
        Some(self.result.clone())
    }

    async fn disclosure_forbidden_fragments(&self) -> Vec<String> {
        Vec::new()
    }

    async fn reconcile_orphaned_tasks(&self, _subagent_type: &str) -> Vec<AgentId> {
        Vec::new()
    }
}

async fn rpc(
    client: &reqwest::Client,
    endpoint: &str,
    id: u64,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let response = client
        .post(endpoint)
        .json(&JsonRpcRequest::new(id, method, params))
        .send()
        .await
        .expect("real listener response");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    response.json().await.expect("JSON-RPC response")
}

struct PeerAcceptingConsumer;

#[async_trait::async_trait]
impl VerifiedPeerConsumer for PeerAcceptingConsumer {
    async fn consent(
        &self,
        _recipient: &AgentId,
        _content: &AgentMessage,
        _peer_id: &PeerId,
    ) -> Result<VerifiedPeerConsent, String> {
        Ok(VerifiedPeerConsent::Accept)
    }

    async fn ingest(
        &self,
        _recipient: &AgentId,
        _content: AgentMessage,
        _peer_id: &PeerId,
    ) -> Result<(), String> {
        Ok(())
    }
}

struct PeerDecliningConsumer;

#[async_trait::async_trait]
impl VerifiedPeerConsumer for PeerDecliningConsumer {
    async fn consent(
        &self,
        _recipient: &AgentId,
        _content: &AgentMessage,
        _peer_id: &PeerId,
    ) -> Result<VerifiedPeerConsent, String> {
        Ok(VerifiedPeerConsent::Decline)
    }

    async fn ingest(
        &self,
        _recipient: &AgentId,
        _content: AgentMessage,
        _peer_id: &PeerId,
    ) -> Result<(), String> {
        panic!("declined content must not reach ingest")
    }
}

fn peer_delivery_envelope(correlation_id: &str) -> AgentEnvelope<serde_json::Value> {
    AgentEnvelope::new(
        AgentEnvelopeHeader {
            sender: AgentId::parse("peer-agent").expect("valid sender"),
            recipient: AgentId::parse("local-peer-session").expect("valid recipient"),
            correlation_id: CorrelationId::new(correlation_id),
            kind: MessageKind::PeerMessage,
            sequence: 1,
            not_after: i64::MAX,
            nonce: "nonce".to_owned(),
            content_hash: vec![1],
            prev_hash: vec![2],
        },
        serde_json::json!("hello"),
        PeerIdentity::from_public_key(vec![7; 32]).expect("peer identity"),
        Ed25519Sig(vec![]),
    )
}

/// The decision core over the address STRING. Non-loopback is no longer a flat
/// refusal (Story 18.1b): it binds if — and only if — the whole TLS + API-key +
/// signed-identity unit is present.
#[test]
fn bind_decision_gates_non_loopback_on_the_whole_security_unit() {
    let none = BindEvidence::default();
    let full = BindEvidence {
        tls: true,
        api_key_auth: true,
        signed_identity: true,
    };
    for allowed in [
        "localhost:8080",
        "127.0.0.1:0",
        "127.42.7.9:9000",
        "[::1]:8080",
    ] {
        assert!(
            matches!(evaluate_bind_safety(allowed, none), BindDecision::Bind),
            "{allowed} must bind on loopback with no evidence at all"
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
                evaluate_bind_safety(refused, none),
                BindDecision::RefuseWithReason(_)
            ),
            "{refused} must be refused without TLS + auth"
        );
        assert!(
            matches!(evaluate_bind_safety(refused, full), BindDecision::Bind),
            "{refused} must bind once the whole unit is configured"
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

/// Story 18.1a proved the `rejected` wire channel; Story 18.1b swaps the reason
/// from a build-capability statement to a **policy verdict**. The channel
/// assertion is unchanged — the reason is what moved.
#[tokio::test]
async fn message_send_on_a_discovery_only_listener_returns_a_policy_rejection() {
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
    let reason = result["status"]["message"]["parts"][0]["text"]
        .as_str()
        .expect("a refusal carries a human-readable reason");
    assert!(reason.contains("discovery only"), "reason={reason}");
    // The reason must tell the operator how to enable execution, not just that
    // it is off.
    assert!(reason.contains("--serve-a2a"), "reason={reason}");
    // A2A has no `refused` state; the policy decline is `rejected`.
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
        (
            r#"{"jsonrpc":"2.0","id":7,"method":"message/send","params":{"message":{"role":"user","parts":[{"kind":"text","text":7}]}}}"#,
            CODE_INVALID_PARAMS,
        ),
        (
            r#"{"jsonrpc":"2.0","id":8,"method":"message/send","params":{"message":{"role":"user","parts":[{"kind":"file","file":{}}]}}}"#,
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

    let oversized_message_id = "x".repeat(257);
    let oversized_id: serde_json::Value = client
        .post(&endpoint)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "message/send",
            "params": {
                "message": {
                    "messageId": oversized_message_id,
                    "role": "user",
                    "parts": [{ "kind": "text", "text": "hello" }],
                }
            }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(oversized_id["error"]["code"], CODE_INVALID_PARAMS);

    let oversized_fallback_id = "y".repeat(257);
    let oversized_fallback: serde_json::Value = client
        .post(&endpoint)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": oversized_fallback_id,
            "method": "message/send",
            "params": {
                "message": {
                    "role": "user",
                    "parts": [{ "kind": "text", "text": "hello" }],
                }
            }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        oversized_fallback["error"]["code"], CODE_INVALID_PARAMS,
        "a fallback task id derived from a call id is bounded too"
    );

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

/// **[K3b-c]** — the R3 keystone. The guard has to hold at the SOCKET, not only
/// over the address string: `evaluate_bind_safety` is effect-free and never sees
/// what the kernel bound, and `serve` is `pub`, which is exactly how a caller
/// could hand it a routable listener it bound itself.
///
/// This test does not route through the CLI. It is the only proof that Story
/// 18.1a's last line of defence survived Story 18.1b — which *conditions* it on
/// TLS + auth evidence rather than deleting it. Deleting the `ensure!` turns
/// this RED; leaving it unconditional turns every non-loopback keystone RED.
#[tokio::test]
async fn serve_refuses_a_self_bound_non_loopback_listener_without_tls_and_auth() {
    let registry = Arc::new(CapabilityRegistry::new(None));
    let key_dir = tempfile::tempdir().unwrap();
    let signer = IdentityKeyStore::new(key_dir.path())
        .load_or_generate()
        .unwrap();
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let config =
        ServeConfig::discovery_only(registry, signer, std::env::current_dir().expect("cwd"));
    let error = serve(listener, config, CancellationToken::new())
        .await
        .expect_err("a non-loopback listener without TLS + auth must never be served");
    let message = error.to_string();
    assert!(
        message.contains("non-loopback"),
        "unexpected error: {message}"
    );
    assert!(message.contains("TLS"), "unexpected error: {message}");
}

/// **[K4] AC4 differential.** A real listener must distinguish a poll that
/// merely asks about work from a response that actually hands result text back.
/// The journal is real: the fold observes exactly the same durable records that
/// `/team log`, the CLI, and the panel will render.
#[tokio::test]
async fn ac4_working_poll_records_status_query_without_disclosure_but_completed_fetch_records_both()
{
    const TASK_ID: &str = "disclosure-task";
    const RESULT: &str = "completed peer-visible result";

    let workspace = tempfile::tempdir().expect("workspace");
    let key_dir = tempfile::tempdir().expect("identity directory");
    let journal = Arc::new(
        NodeJournal::open_workspace(workspace.path())
            .await
            .expect("open real node journal"),
    );
    let (domain_tx, _domain_rx) =
        tokio::sync::mpsc::unbounded_channel::<rustain::domain::events::AppEvent>();
    let room: Arc<dyn RoomJournal> =
        Arc::new(NodeRoomJournal::new(journal.clone(), Some(domain_tx)));
    let complete = Arc::new(Notify::new());
    let completed_signal = Arc::new(Notify::new());
    let runtime: Arc<dyn InboundPeerRuntime> = Arc::new(DisclosureRuntime {
        complete: complete.clone(),
        completed: completed_signal.clone(),
        result: RESULT.to_owned(),
    });
    let signer = IdentityKeyStore::new(key_dir.path())
        .load_or_generate()
        .expect("identity");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind listener");
    let endpoint = format!(
        "http://{}/",
        listener.local_addr().expect("listener address")
    );
    let cancel = CancellationToken::new();
    let http = tokio::spawn(serve(
        listener,
        ServeConfig {
            registry: Arc::new(CapabilityRegistry::new(None)),
            signer,
            security: A2aServerSecurity::default(),
            runtime: Some(runtime),
            transparency: Arc::new(TransparencySink::new(room)),
            policy: A2aAdmissionPolicy::Allow,
            workspace: workspace.path().to_path_buf(),
            advertised_host: None,
            cards: Arc::new(SignedCardCache::new()),
        },
        cancel.child_token(),
    ));
    let client = reqwest::Client::new();

    let accepted = rpc(
        &client,
        &endpoint,
        1,
        "message/send",
        serde_json::json!({
            "message": {
                "messageId": TASK_ID,
                "role": "user",
                "parts": [{ "kind": "text", "text": "perform the task" }]
            }
        }),
    )
    .await;
    assert!(
        matches!(
            accepted["result"]["status"]["state"].as_str(),
            Some("submitted" | "working")
        ),
        "message/send must enter the real task lifecycle: {accepted}"
    );

    let working = rpc(
        &client,
        &endpoint,
        2,
        "tasks/get",
        serde_json::json!({ "id": TASK_ID }),
    )
    .await;
    assert_eq!(working["result"]["status"]["state"], "working");
    assert!(
        working["result"]["status"].get("message").is_none(),
        "a working poll must hand no text back: {working}"
    );
    let working_rows = fold_transparency(&journal.load().await.expect("load journal"));
    assert_eq!(
        working_rows
            .iter()
            .filter(|row| {
                row.kind == TransparencyKind::StatusQueried && row.task.as_deref() == Some(TASK_ID)
            })
            .count(),
        1,
        "positive control: the first working poll remains an 18.2 status-query record"
    );
    assert!(
        working_rows.iter().all(|row| {
            !(row.kind == TransparencyKind::Disclosed && row.task.as_deref() == Some(TASK_ID))
        }),
        "a working poll must not fabricate a disclosure row"
    );

    complete.notify_one();
    let completed = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let response = rpc(
                &client,
                &endpoint,
                3,
                "tasks/get",
                serde_json::json!({ "id": TASK_ID }),
            )
            .await;
            if response["result"]["status"]["state"].as_str() == Some("completed") {
                break response;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the gated runtime must complete");
    assert_eq!(
        completed["result"]["status"]["message"]["parts"][0]["text"], RESULT,
        "the completed fetch must actually carry the result text"
    );
    completed_signal.notified().await;

    let repeated = rpc(
        &client,
        &endpoint,
        4,
        "tasks/get",
        serde_json::json!({ "id": TASK_ID }),
    )
    .await;
    assert_eq!(
        repeated["result"]["status"]["message"]["parts"][0]["text"], RESULT,
        "repeated fetch still returns the immutable result"
    );

    let rows = fold_transparency(&journal.load().await.expect("load journal"));
    assert_eq!(
        rows.iter()
            .filter(|row| {
                row.kind == TransparencyKind::StatusQueried && row.task.as_deref() == Some(TASK_ID)
            })
            .count(),
        1,
        "the completed fetch must preserve the status-query positive control"
    );
    let disclosure = rows
        .iter()
        .find(|row| row.kind == TransparencyKind::Disclosed && row.task.as_deref() == Some(TASK_ID))
        .expect("the completed result must produce a distinct disclosure row");
    assert_eq!(disclosure.direction.label(), "outbound");
    assert_eq!(
        disclosure.summary,
        format!("disclosed result to peer ({} bytes)", RESULT.len())
    );
    assert_eq!(
        rows.iter()
            .filter(|row| {
                row.kind == TransparencyKind::Disclosed && row.task.as_deref() == Some(TASK_ID)
            })
            .count(),
        1,
        "repeated fetches must not append duplicate disclosures"
    );
    let status_peer = rows
        .iter()
        .find(|row| {
            row.kind == TransparencyKind::StatusQueried && row.task.as_deref() == Some(TASK_ID)
        })
        .expect("status query row")
        .peer
        .clone();
    assert_eq!(
        disclosure.peer, status_peer,
        "disclosure must identify the authenticated remote principal, not the local task node"
    );

    const CANCEL_TASK_ID: &str = "cancel-result-task";
    let _submitted = rpc(
        &client,
        &endpoint,
        5,
        "message/send",
        serde_json::json!({
            "message": {
                "messageId": CANCEL_TASK_ID,
                "role": "user",
                "parts": [{ "kind": "text", "text": "complete before cancellation" }]
            }
        }),
    )
    .await;
    let completed_wait = completed_signal.notified();
    complete.notify_one();
    tokio::time::timeout(Duration::from_secs(2), completed_wait)
        .await
        .expect("second runtime completion");
    tokio::time::sleep(Duration::from_millis(20)).await;
    let cancelled = rpc(
        &client,
        &endpoint,
        6,
        "tasks/cancel",
        serde_json::json!({ "id": CANCEL_TASK_ID }),
    )
    .await;
    assert_eq!(
        cancelled["result"]["status"]["message"]["parts"][0]["text"], RESULT,
        "tasks/cancel must not bypass disclosure when it returns completed content"
    );
    let cancel_rows = fold_transparency(&journal.load().await.expect("load journal"));
    assert_eq!(
        cancel_rows
            .iter()
            .filter(|row| {
                row.kind == TransparencyKind::Disclosed
                    && row.task.as_deref() == Some(CANCEL_TASK_ID)
            })
            .count(),
        1,
        "tasks/cancel result content must be journaled exactly once"
    );

    cancel.cancel();
    http.await
        .expect("server task")
        .expect("server shuts down cleanly");
}

/// **[K5] AC5 double-divergence keystone.** The real RAP peer front door writes
/// through `TransparencySink` to a real `NodeJournal`; raw file bytes prove the
/// flock/fsync writer ran, and the production fold proves the records reach the
/// existing team-log projection. An established inbound-A2A row is the positive
/// control in the same journal.
#[tokio::test]
async fn ac5_peer_deliveries_are_durable_and_fold_into_transparency() {
    let workspace = tempfile::tempdir().expect("real journal workspace");
    let journal = Arc::new(
        NodeJournal::open_workspace(workspace.path())
            .await
            .expect("open real NodeJournal"),
    );
    let (domain_tx, _domain_rx) =
        tokio::sync::mpsc::unbounded_channel::<rustain::domain::events::AppEvent>();
    let room: Arc<dyn RoomJournal> = Arc::new(NodeRoomJournal::new(
        journal.clone(),
        Some(domain_tx.clone()),
    ));
    let sink = Arc::new(TransparencySink::new(room));
    let remote_peer = PeerId::from_public_key(&[7; 32]).expect("peer id");

    sink.record(InboundOutcome::Accepted {
        peer: remote_peer,
        node: AgentId::parse("a2a-in/p-control/t-inbound").expect("control node"),
        task_id: "inbound-a2a-control".to_owned(),
    })
    .await
    .expect("record unchanged inbound-A2A positive control");
    let recorder: Arc<dyn PeerInteractionRecorder> = sink;

    let accepting_tree = NodeTree::new();
    let accepting_bus = Arc::new(LocalMessageBus::new(
        accepting_tree.clone(),
        Arc::new(RelationshipDeliveryPolicy) as Arc<dyn DeliveryPolicy>,
    )) as Arc<dyn AgentMessageBus>;
    let accepting_handler = VerifiedPeerFrameHandler::new(
        accepting_tree,
        Arc::new(ArcSwap::from_pointee(accepting_bus)),
        domain_tx.clone(),
        Arc::new(PeerAcceptingConsumer),
        recorder.clone(),
    );
    let accepted = peer_delivery_envelope("peer-accept");
    let accepted_peer = accepted.signer.peer_id.clone();
    accepting_handler
        .handle_verified_peer_frame(accepted, accepted_peer)
        .await
        .expect("accepted peer delivery must journal before its receipt");

    let refusing_tree = NodeTree::new();
    let refusing_bus = Arc::new(LocalMessageBus::new(
        refusing_tree.clone(),
        Arc::new(RelationshipDeliveryPolicy) as Arc<dyn DeliveryPolicy>,
    )) as Arc<dyn AgentMessageBus>;
    let refusing_handler = VerifiedPeerFrameHandler::new(
        refusing_tree,
        Arc::new(ArcSwap::from_pointee(refusing_bus)),
        domain_tx,
        Arc::new(PeerDecliningConsumer),
        recorder,
    );
    let refused = peer_delivery_envelope("peer-refusal");
    let refused_peer = refused.signer.peer_id.clone();
    assert!(
        refusing_handler
            .handle_verified_peer_frame(refused, refused_peer)
            .await
            .is_err(),
        "a consent refusal must remain sender-visible"
    );

    let rows = fold_transparency(&journal.load().await.expect("load real journal"));
    assert!(
        rows.iter().any(|row| {
            row.kind == TransparencyKind::Accepted
                && row.direction.label() == "inbound"
                && row.task.as_deref() == Some("inbound-a2a-control")
        }),
        "positive control: the established inbound-A2A row must remain unchanged"
    );
    assert!(
        rows.iter().any(|row| {
            row.kind == TransparencyKind::Accepted && row.task.as_deref() == Some("peer-accept")
        }),
        "the accepted peer delivery must fold into the existing team-log row type"
    );
    assert!(
        rows.iter().any(|row| {
            row.kind == TransparencyKind::Rejected && row.task.as_deref() == Some("peer-refusal")
        }),
        "the AC1 consent refusal must fold into the existing team-log row type"
    );
    let raw = std::fs::read_to_string(journal.path()).expect("read fsynced journal file");
    assert!(
        raw.contains(r#""event":"remote_envelope_accepted""#),
        "the real journal file must contain accepted records: {raw}"
    );
    assert!(
        raw.contains(r#""event":"remote_envelope_rejected""#),
        "the real journal file must contain the consent-refusal record: {raw}"
    );
}
