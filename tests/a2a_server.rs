#![cfg(feature = "a2a")]

use std::sync::{Arc, Mutex};
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
    MessageKind, NodeState, PeerId, PeerIdentity, SemanticMessageType,
};
use rustain::domain::models::{PinnedKey, PinnedKeyAlgorithm, TrustTier};
use rustain::domain::ports::{
    AgentMessageBus, DeliveryPolicy, EffectiveDeliveryPolicy, InboundApprovalTicket,
    InboundPeerError, InboundPeerRuntime, InboundPeerTask, PeerInteractionRecorder,
    RelationshipDeliveryPolicy, RoomJournal, RoomJournalReader,
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

#[derive(Default)]
struct TypeRecordingRuntime {
    seen: Mutex<Vec<SemanticMessageType>>,
    modes: Mutex<Vec<rustain::domain::models::ResponseMode>>,
    policy: Option<EffectiveDeliveryPolicy>,
    senders: Mutex<Vec<tokio::sync::watch::Sender<NodeState>>>,
}

#[async_trait::async_trait]
impl InboundPeerRuntime for TypeRecordingRuntime {
    fn response_policy(
        &self,
        peer_id: &PeerId,
        message_type: SemanticMessageType,
    ) -> rustain::domain::ports::PeerResponsePolicy {
        let response = self.policy.as_ref().map_or_else(
            rustain::domain::ports::PeerResponsePolicy::default,
            |policy| policy.response_policy_for_peer(peer_id, message_type),
        );
        self.seen.lock().expect("seen lock").push(message_type);
        self.modes.lock().expect("mode lock").push(response.mode);
        response
    }

    async fn start(
        &self,
        _task: InboundPeerTask,
        _cancel: CancellationToken,
    ) -> Result<tokio::sync::watch::Receiver<NodeState>, InboundPeerError> {
        let (sender, receiver) = tokio::sync::watch::channel(NodeState::Running);
        self.senders.lock().expect("senders lock").push(sender);
        Ok(receiver)
    }

    async fn request_admission_approval(
        &self,
        _peer_id: &PeerId,
        _summary: &str,
    ) -> Result<InboundApprovalTicket, InboundPeerError> {
        Err(InboundPeerError::unavailable("approval is not used"))
    }

    async fn take_result_text(&self, _node_id: &AgentId) -> Option<String> {
        None
    }

    async fn disclosure_forbidden_fragments(&self) -> Vec<String> {
        Vec::new()
    }

    async fn reconcile_orphaned_tasks(&self, _subagent_type: &str) -> Vec<AgentId> {
        Vec::new()
    }
}
struct BrokenRoomJournal;

#[async_trait::async_trait]
impl RoomJournal for BrokenRoomJournal {
    async fn record_event(
        &self,
        _event: rustain::domain::models::RoomEvent,
    ) -> Result<(), rustain::domain::ports::RoomJournalError> {
        Err(rustain::domain::ports::RoomJournalError::Append(
            "disk full".to_owned(),
        ))
    }
}

struct BlockingRetractJournal {
    inner: Arc<dyn RoomJournal>,
    append_started: Arc<Notify>,
    resume_append: Arc<Notify>,
}

#[async_trait::async_trait]
impl RoomJournal for BlockingRetractJournal {
    async fn record_event(
        &self,
        event: rustain::domain::models::RoomEvent,
    ) -> Result<(), rustain::domain::ports::RoomJournalError> {
        if matches!(
            event,
            rustain::domain::models::RoomEvent::RecipientItemRetracted { .. }
        ) {
            self.append_started.notify_one();
            self.resume_append.notified().await;
        }
        self.inner.record_event(event).await
    }
}

struct ToggleFailingReader {
    inner: Arc<NodeJournal>,
    /// 0 = healthy, 1 = tail probe fails, 2 = full load fails.
    failure_mode: Arc<std::sync::atomic::AtomicU8>,
}

#[async_trait::async_trait]
impl RoomJournalReader for ToggleFailingReader {
    async fn load_entries(
        &self,
    ) -> Result<Vec<rustain::domain::models::JournalEntry>, rustain::domain::ports::RoomJournalError>
    {
        if self.failure_mode.load(std::sync::atomic::Ordering::SeqCst) == 2 {
            return Err(rustain::domain::ports::RoomJournalError::Read(
                "injected read failure".to_owned(),
            ));
        }
        RoomJournalReader::load_entries(self.inner.as_ref()).await
    }

    async fn latest_seq(&self) -> Result<u64, rustain::domain::ports::RoomJournalError> {
        if self.failure_mode.load(std::sync::atomic::Ordering::SeqCst) == 1 {
            return Err(rustain::domain::ports::RoomJournalError::Read(
                "injected tail failure".to_owned(),
            ));
        }
        RoomJournalReader::latest_seq(self.inner.as_ref()).await
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
    let signer = PeerIdentity::from_public_key(vec![7; 32]).expect("peer identity");
    let recipient =
        AgentId::from_peer_path(&format!("{}/local-peer-session", signer.peer_id.as_str()))
            .expect("peer-rooted recipient");
    AgentEnvelope::new(
        AgentEnvelopeHeader {
            message_type: String::new(),
            sender: AgentId::parse("peer-agent").expect("valid sender"),
            recipient,
            correlation_id: CorrelationId::new(correlation_id),
            kind: MessageKind::PeerMessage,
            sequence: 1,
            not_after: i64::MAX,
            nonce: "nonce".to_owned(),
            content_hash: vec![1],
            prev_hash: vec![2],
        },
        serde_json::json!("hello"),
        signer,
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

#[tokio::test]
async fn message_type_metadata_is_exact_match_and_vanilla_defaults_to_unknown() {
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
    let team = rustain::domain::models::TeamPolicy {
        overrides: rustain::domain::models::TeamOverrides {
            per_type: std::collections::BTreeMap::from([
                (
                    "consultation".to_owned(),
                    toml::Value::try_from(rustain::domain::models::TeamTypeOverride {
                        response_mode: Some(rustain::domain::models::ResponseMode::NotifyAndDraft),
                        notification: None,
                    })
                    .expect("test value serializes"),
                ),
                (
                    "bug_report".to_owned(),
                    toml::Value::try_from(rustain::domain::models::TeamTypeOverride {
                        response_mode: Some(rustain::domain::models::ResponseMode::NotifyAndWait),
                        notification: None,
                    })
                    .expect("test value serializes"),
                ),
            ]),
            ..Default::default()
        },
        ..Default::default()
    };
    let individual = rustain::domain::models::IndividualPolicy {
        defaults: rustain::domain::models::IndividualDefaults {
            response_mode: Some(rustain::domain::models::ResponseMode::NotifyAndAuto),
            ..Default::default()
        },
        ..Default::default()
    };
    let effective = rustain::domain::services::team_policy::resolve_effective_policy(
        &individual,
        Some(&team),
        &[],
    );
    let runtime = Arc::new(TypeRecordingRuntime {
        policy: Some(EffectiveDeliveryPolicy::new(Arc::new(effective))),
        ..Default::default()
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
            runtime: Some(runtime.clone()),
            transparency: Arc::new(TransparencySink::new(room)),
            policy: A2aAdmissionPolicy::Allow,
            workspace: workspace.path().to_path_buf(),
            advertised_host: None,
            cards: Arc::new(SignedCardCache::new()),
        },
        cancel.child_token(),
    ));
    let client = reqwest::Client::new();

    let requests = [
        serde_json::json!({
            "message": {
                "messageId": "typed",
                "role": "user",
                "parts": [{ "kind": "text", "text": "typed" }],
                "metadata": { "x-rustain-message-type": "consultation" }
            }
        }),
        serde_json::json!({
            "message": {
                "messageId": "second-type",
                "role": "user",
                "parts": [{ "kind": "text", "text": "second type" }],
                "metadata": { "x-rustain-message-type": "bug_report" }
            }
        }),
        serde_json::json!({
            "message": {
                "messageId": "case-variant",
                "role": "user",
                "parts": [{ "kind": "text", "text": "case variant" }],
                "metadata": { "X-Rustain-Message-Type": "bug_report" }
            }
        }),
        serde_json::json!({
            "message": {
                "messageId": "vanilla",
                "role": "user",
                "parts": [{ "kind": "text", "text": "vanilla" }]
            }
        }),
    ];
    for (index, params) in requests.into_iter().enumerate() {
        let response = rpc(
            &client,
            &endpoint,
            u64::try_from(index + 1).expect("small id"),
            "message/send",
            params,
        )
        .await;
        assert!(
            matches!(
                response["result"]["status"]["state"].as_str(),
                Some("submitted" | "working")
            ),
            "message must reach the execution runtime: {response}"
        );
    }

    assert_eq!(
        *runtime.seen.lock().expect("seen lock"),
        vec![
            SemanticMessageType::Consultation,
            SemanticMessageType::BugReport,
            SemanticMessageType::Unknown,
            SemanticMessageType::Unknown,
        ],
        "only the exact metadata spelling is extracted; vanilla A2A is the common Unknown path"
    );
    assert_eq!(
        *runtime.modes.lock().expect("mode lock"),
        vec![
            rustain::domain::models::ResponseMode::NotifyAndDraft,
            rustain::domain::models::ResponseMode::NotifyAndWait,
            rustain::domain::models::ResponseMode::NotifyAndAuto,
            rustain::domain::models::ResponseMode::NotifyAndAuto,
        ],
        "the A2A setup_task rail must apply team per-type policy and keep Unknown on the base tier"
    );

    let rows = fold_transparency(&journal.load().await.expect("load journal"));
    assert!(
        rows.iter()
            .all(|row| row.kind != TransparencyKind::RecipientItemAcknowledged),
        "notify-and-auto execution is not a deliberate human acknowledgement"
    );

    cancel.cancel();
    http.await.expect("server task").expect("server shutdown");
}

/// **[K4] AC4 differential.** A real listener must distinguish a poll that
/// merely asks about work from a response that actually hands result text back.
/// The journal is real: the fold observes exactly the same durable records that
/// `/team log`, the CLI, and the panel will render.
#[tokio::test]
async fn message_send_fails_closed_before_exposing_an_unrecorded_recipient_item() {
    let workspace = tempfile::tempdir().expect("workspace");
    let key_dir = tempfile::tempdir().expect("identity directory");
    let runtime = Arc::new(TypeRecordingRuntime::default());
    let signer = IdentityKeyStore::new(key_dir.path())
        .load_or_generate()
        .expect("identity");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind listener");
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    let cancel = CancellationToken::new();
    let http = tokio::spawn(serve(
        listener,
        ServeConfig {
            registry: Arc::new(CapabilityRegistry::new(None)),
            signer,
            security: A2aServerSecurity::default(),
            runtime: Some(runtime.clone()),
            transparency: Arc::new(TransparencySink::new(Arc::new(BrokenRoomJournal))),
            policy: A2aAdmissionPolicy::Allow,
            workspace: workspace.path().to_path_buf(),
            advertised_host: None,
            cards: Arc::new(SignedCardCache::new()),
        },
        cancel.child_token(),
    ));

    let response = rpc(
        &reqwest::Client::new(),
        &endpoint,
        1,
        "message/send",
        serde_json::json!({
            "message": {
                "messageId": "must-not-exist",
                "role": "user",
                "parts": [{ "kind": "text", "text": "not durable" }]
            }
        }),
    )
    .await;
    assert_eq!(response["error"]["code"], -32603);
    assert!(
        runtime
            .senders
            .lock()
            .is_ok_and(|senders| senders.is_empty()),
        "the execution runtime must not see an item whose creation append failed"
    );

    cancel.cancel();
    http.await.expect("server task").expect("server shutdown");
}

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

    let spoofed_peer = rustain::domain::models::PeerId::from_public_key(&[99; 32])
        .expect("fixed spoof id")
        .to_string();
    let accepted = rpc(
        &client,
        &endpoint,
        1,
        "message/send",
        serde_json::json!({
            "message": {
                "messageId": TASK_ID,
                "role": "user",
                "parts": [{ "kind": "text", "text": "perform the task" }],
                "metadata": { "peerId": spoofed_peer.clone() }
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
    let item_id = accepted["result"]["metadata"]["x-rustain-item-id"]
        .as_str()
        .expect("accepted task carries the recipient-minted item id");
    assert_ne!(item_id, TASK_ID, "recipient id is distinct from task.id");
    assert!(
        item_id.starts_with("ri_"),
        "recipient id is opaque and minted"
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
    assert_eq!(
        working["result"]["metadata"]["x-rustain-item-id"], item_id,
        "polling preserves the durable recipient address"
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
    assert!(
        working_rows.iter().any(|row| {
            row.kind == TransparencyKind::RecipientItemReceived
                && row.task.as_deref() == Some(item_id)
        }),
        "message/send durably records the recipient item before returning it"
    );
    let received_row = working_rows
        .iter()
        .find(|row| row.kind == TransparencyKind::RecipientItemReceived)
        .expect("recipient item row");
    assert_ne!(
        received_row.peer, spoofed_peer,
        "caller-supplied identity metadata must never become item provenance"
    );
    assert!(
        working_rows
            .iter()
            .all(|row| row.kind != TransparencyKind::RecipientItemAcknowledged),
        "viewing a task is not acknowledgement"
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

// ── Story 19.16b · the cross-host acknowledgement read verb ─────────────────

/// Seed one recipient item and drive it to `state` on the durable journal.
///
/// ⛔ A fixture seeder, never a bypass: it writes the same `RoomEvent`s the
/// ingress and the daemon's `/team ack` / `/team remove` rails write, and the
/// verb under test still enters through `build_router` → `dispatch` (Rule 2).
async fn seed_item(
    room: &dyn RoomJournal,
    principal_key: &rustain::adapters::a2a::exec::SubmitterKey,
    item_id: &str,
    task: &str,
    state: rustain::domain::models::RecipientItemState,
) {
    use rustain::domain::models::{ItemAddress, ItemId, RecipientItemState, RoomEvent};

    let address = ItemAddress::from_a2a_ingress(
        principal_key.pseudonymous_peer_id(),
        ItemId::from_replay(item_id),
    );
    room.record_event(RoomEvent::RecipientItemReceived {
        address: address.clone(),
        task: task.to_owned(),
        alias: None,
        content: format!("content for {item_id}"),
    })
    .await
    .expect("seed the recipient item");
    match state {
        RecipientItemState::Received { .. } => {}
        RecipientItemState::Acknowledged { .. } => {
            room.record_event(RoomEvent::RecipientItemAcknowledged {
                address,
                alias: None,
            })
            .await
            .expect("seed the acknowledgement");
        }
        RecipientItemState::Removed { .. } => {
            room.record_event(RoomEvent::RecipientItemRemoved { address })
                .await
                .expect("seed the removal");
        }
        _ => unreachable!("RecipientItemState is three states (AD-1827)"),
    }
}

/// Story 19.16b AC1 — `x-rustain-items/list` serves the calling principal's own
/// set through the real front door, with a tombstone that is present-and-marked
/// and another principal's items byte-identically absent.
///
/// Front door: `rpc` → the real axum router → `dispatch`.
/// **Mutant → RED:** call `items_list` directly instead of going through
/// `dispatch` — the `-32601` → served transition goes unproven.
/// **Mutant → RED:** omit removed items from the set.
/// **Mutant → RED:** classify the tombstone BEFORE filtering by principal — B's
/// removed id would appear for caller A and reconstitute the enumeration
/// oracle `ADR-17-4a-01` R21 kills.
/// **Mutant → RED:** serialize the raw `journal_order`.
/// **Mutant → RED:** reject an unknown payload field (`AD-1826`'s decided half).
/// **Mutant → RED:** drop the collapsed-principal disclosure.
///
/// ⚠ **What this half CANNOT prove (`A13`):** the harness binds `127.0.0.1`, so
/// `authenticate` never consults a credential and every caller is the loopback
/// principal. The *credential → principal* mapping is proven at the projection
/// layer instead (`tests/conformance_19_16b_board.rs`), exactly as the shipped
/// precedent at `a2a_server_exec.rs:1850` writes it. What this half DOES prove
/// on the wire is that the filter is by **principal**: the seeded credential-B
/// items belong to a different `ItemPrincipal` and never appear.
#[tokio::test]
async fn ac1_the_read_verb_serves_only_the_callers_own_set_through_the_real_front_door() {
    use rustain::adapters::a2a::exec::SubmitterKey;
    use rustain::domain::models::RecipientItemState;

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

    let caller = SubmitterKey::loopback();
    let other = SubmitterKey::from_api_key("credential-b");
    seed_item(
        room.as_ref(),
        &caller,
        "ri_mine_live",
        "task-live",
        RecipientItemState::Received {
            content: String::new(),
        },
    )
    .await;
    seed_item(
        room.as_ref(),
        &caller,
        "ri_mine_acked",
        "task-acked",
        RecipientItemState::Acknowledged {
            content: String::new(),
        },
    )
    .await;
    seed_item(
        room.as_ref(),
        &caller,
        "ri_mine_gone",
        "task-gone",
        RecipientItemState::Removed {
            acknowledged_before: false,
        },
    )
    .await;
    seed_item(
        room.as_ref(),
        &other,
        "ri_theirs_gone",
        "task-theirs",
        RecipientItemState::Removed {
            acknowledged_before: false,
        },
    )
    .await;
    // AC3(e) wire pin: an item acknowledged and THEN removed keeps both facts
    // on the wire — `state: "removed"` (⛔ three states, never a fifth) plus
    // the ADDITIVE `acknowledgedBeforeRemoval` sibling. Seeded event-by-event
    // because `seed_item`'s one-shot states cannot express the sequence.
    {
        use rustain::domain::models::{ItemAddress, ItemId, RoomEvent};
        let acked_gone = ItemAddress::from_a2a_ingress(
            caller.pseudonymous_peer_id(),
            ItemId::from_replay("ri_mine_acked_gone"),
        );
        room.record_event(RoomEvent::RecipientItemReceived {
            address: acked_gone.clone(),
            task: "task-acked-gone".to_owned(),
            alias: None,
            content: "content".to_owned(),
        })
        .await
        .expect("seed received");
        room.record_event(RoomEvent::RecipientItemAcknowledged {
            address: acked_gone.clone(),
            alias: None,
        })
        .await
        .expect("seed acknowledged");
        room.record_event(RoomEvent::RecipientItemRemoved {
            address: acked_gone,
        })
        .await
        .expect("seed removed");
    }

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
            runtime: None,
            transparency: Arc::new(TransparencySink::new(room).with_reader(journal.clone())),
            policy: A2aAdmissionPolicy::Allow,
            workspace: workspace.path().to_path_buf(),
            advertised_host: None,
            cards: Arc::new(SignedCardCache::new()),
        },
        cancel.child_token(),
    ));
    let client = reqwest::Client::new();

    // AC5(a): a forward-compatible extra payload key is IGNORED, not refused.
    let response = rpc(
        &client,
        &endpoint,
        1,
        "x-rustain-items/list",
        serde_json::json!({ "aFieldFromANewerBuild": 7 }),
    )
    .await;
    assert!(
        response.get("error").is_none(),
        "an unknown payload field must be ignored, never refused: {response}"
    );
    let items = response["result"]["items"]
        .as_array()
        .unwrap_or_else(|| panic!("the read returns the caller's SET: {response}"));

    let ids: Vec<&str> = items
        .iter()
        .map(|item| item["itemId"].as_str().expect("item id"))
        .collect();
    assert_eq!(
        ids,
        vec![
            "ri_mine_live",
            "ri_mine_acked",
            "ri_mine_gone",
            "ri_mine_acked_gone",
        ],
        "the set is the caller's own items in arrival order: {response}"
    );

    // AC1(e): the tombstone is present-and-marked, in the RESULT, not an error.
    let states: std::collections::HashMap<&str, &str> = items
        .iter()
        .map(|item| {
            (
                item["itemId"].as_str().expect("item id"),
                item["state"].as_str().expect("item state"),
            )
        })
        .collect();
    assert_eq!(states["ri_mine_gone"], "removed");
    // Positive control: the mapper is not answering "removed" for everything —
    // an acknowledgement performed on the OTHER rail reaches this read.
    assert_eq!(states["ri_mine_acked"], "acknowledged");
    assert_eq!(states["ri_mine_live"], "received");
    assert_eq!(
        states["ri_mine_acked_gone"], "removed",
        "⛔ the wire stays three-state: ack-then-removed is `removed`, never a fifth state"
    );
    let ids: Vec<&str> = items
        .iter()
        .map(|item| item["itemId"].as_str().expect("item id"))
        .collect();
    let acked_gone = &items[ids
        .iter()
        .position(|id| *id == "ri_mine_acked_gone")
        .expect("the ack-then-removed item is on the wire")];
    assert_eq!(acked_gone["acknowledgedBeforeRemoval"], true);
    let bare_gone = &items[ids
        .iter()
        .position(|id| *id == "ri_mine_gone")
        .expect("the bare-removed item is on the wire")];
    assert!(
        bare_gone.get("acknowledgedBeforeRemoval").is_none(),
        "the sibling field is ADDITIVE — absent when there was no acknowledgement"
    );

    // AC1(e)/`A22`: another principal's REMOVED item is absent, byte-identically
    // to an id that never existed.
    let raw = response.to_string();
    assert!(
        !raw.contains("ri_theirs_gone"),
        "filter by principal FIRST: a tombstone-before-filter handler leaks a \
         `removed` row for an id the caller does not own — the enumeration \
         oracle `A3` exists to kill: {raw}"
    );

    // AC1(f): the fold-scheme-dependent number never reaches the wire.
    assert!(
        !raw.contains("journalOrder") && !raw.contains("journal_order"),
        "`journal_order` is not stable across the two assignment schemes: {raw}"
    );
    assert_eq!(items[0]["ordinal"], 0, "a dense per-peer arrival ordinal");
    assert_eq!(items[2]["ordinal"], 2);

    // AC2(c): on this loopback bind the collapse is DISCLOSED, not refused.
    assert_eq!(
        response["result"]["principalCollapsed"], true,
        "a loopback bind never consults a credential, so the board must be told \
         these rows are every local caller's: {response}"
    );

    cancel.cancel();
    http.await.expect("server task").expect("server shutdown");
}

/// Story 19.16b AC1(d) — reading acknowledgement state is not itself an
/// acknowledgement: N reads leave the journal byte-count and row-count exactly
/// where they were.
///
/// **Ratchet (Rule 4):** journal-length equality across N reads — structural,
/// ⛔ never a timing window.
/// **Mutant → RED:** add a `StatusQueried`-style `transparency.record(..)` to
/// the handler, the way the adjacent `tasks/get` does twice over.
/// **Positive control:** the shipped
/// `ac4_working_poll_records_status_query_without_disclosure_but_completed_fetch_records_both`
/// in this same file proves the journal counter CAN move on a served read —
/// without it, "the count did not change" is green from birth.
#[tokio::test]
async fn ac1_repeated_reads_of_the_acknowledgement_set_never_touch_the_journal() {
    use rustain::adapters::a2a::exec::SubmitterKey;
    use rustain::domain::models::RecipientItemState;

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
    seed_item(
        room.as_ref(),
        &SubmitterKey::loopback(),
        "ri_read_me",
        "task-read",
        RecipientItemState::Received {
            content: String::new(),
        },
    )
    .await;

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
            runtime: None,
            transparency: Arc::new(TransparencySink::new(room).with_reader(journal.clone())),
            policy: A2aAdmissionPolicy::Allow,
            workspace: workspace.path().to_path_buf(),
            advertised_host: None,
            cards: Arc::new(SignedCardCache::new()),
        },
        cancel.child_token(),
    ));
    let client = reqwest::Client::new();

    let before = journal.load().await.expect("load journal").len();
    for id in 1..=4 {
        let response = rpc(
            &client,
            &endpoint,
            id,
            "x-rustain-items/list",
            serde_json::json!({}),
        )
        .await;
        assert_eq!(
            response["result"]["items"].as_array().expect("items").len(),
            1,
            "control: every read actually served the set, so the ratchet below \
             is measuring a read that happened: {response}"
        );
    }
    let after = journal.load().await.expect("load journal").len();
    assert_eq!(
        before, after,
        "reading acknowledgement state is not itself an acknowledgement, and \
         never marks anything (AD-1822)"
    );

    cancel.cancel();
    http.await.expect("server task").expect("server shutdown");
}

/// Story 19.16b AC1 — an acknowledgement performed on the **daemon** rail
/// becomes visible to the **cross-host** read without restarting the host.
///
/// `FR165`'s only sentence for this story is *"a sender can observe each
/// recipient's acknowledgement state"*. The served projection has exactly one
/// in-place writer — the inbound `message/send` ingress — while `/team ack`
/// and `/team remove` travel the daemon socket and land in the durable
/// journal. Without a read-side re-fold the verb would answer `received`
/// forever and `FR165` would be false on any host that has not restarted.
///
/// **Mutant → RED:** delete the `refresh_recipient_items` call from the
/// handler. The first read still passes (the startup fold saw the item) and
/// the second read answers `received` for an acknowledged item.
/// **Positive control:** the first read is asserted `received`, so the second
/// read's `acknowledged` is a transition this call observed and not a state
/// the harness seeded.
#[tokio::test]
async fn ac1_an_acknowledgement_written_after_startup_is_visible_without_a_restart() {
    use rustain::adapters::a2a::exec::SubmitterKey;
    use rustain::domain::models::{ItemAddress, ItemId, RecipientItemState, RoomEvent};

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

    let caller = SubmitterKey::loopback();
    seed_item(
        room.as_ref(),
        &caller,
        "ri_acked_later",
        "task-later",
        RecipientItemState::Received {
            content: String::new(),
        },
    )
    .await;

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
            runtime: None,
            transparency: Arc::new(
                TransparencySink::new(room.clone()).with_reader(journal.clone()),
            ),
            policy: A2aAdmissionPolicy::Allow,
            workspace: workspace.path().to_path_buf(),
            advertised_host: None,
            cards: Arc::new(SignedCardCache::new()),
        },
        cancel.child_token(),
    ));
    let client = reqwest::Client::new();

    // Positive control: before the operator acts, the read says `received`.
    let before = rpc(
        &client,
        &endpoint,
        1,
        "x-rustain-items/list",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(
        before["result"]["items"][0]["state"], "received",
        "control: the read starts from the un-acknowledged state: {before}"
    );

    // The operator acknowledges on the OTHER rail — durable journal only; the
    // served projection is never told.
    room.record_event(RoomEvent::RecipientItemAcknowledged {
        address: ItemAddress::from_a2a_ingress(
            caller.pseudonymous_peer_id(),
            ItemId::from_replay("ri_acked_later"),
        ),
        alias: None,
    })
    .await
    .expect("the acknowledgement is durable");

    let after = rpc(
        &client,
        &endpoint,
        2,
        "x-rustain-items/list",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(
        after["result"]["items"][0]["state"], "acknowledged",
        "the sender must observe the acknowledgement without a host restart: {after}"
    );

    cancel.cancel();
    http.await.expect("server task").expect("server shutdown");
}

/// Story 19.16b AC1(g) — **Rule 1: the client method's first non-test caller is
/// real, and this keystone reaches the served arm THROUGH the shipped client.**
///
/// `TaskClient::list_items` lives on the inherent impl (Story 19.16f `AC2`
/// re-homed it off `A2aTaskTransport`, deleting the seven double bodies that
/// answered for it). Its production callers are the Act 1 board's per-peer
/// read (`a2a::board::read_peer`, reached from
/// `transparency_bridge::team_command`'s `/team board` arm) and the retract's
/// confirm-time preview. ⛔ A caller **count** or a source **grep** would
/// be satisfied by a dead implementation; this drives the real
/// `A2aClientAdapter::post_jsonrpc` against the real axum router and asserts
/// the behaviour only the wired path produces.
///
/// **Mutant → RED:** point `TaskClient::list_items` at any other method name —
/// the peer answers `-32601` and the transport returns `A2aError::JsonRpc`,
/// which is exactly what the board renders as `⚠ unreachable`.
#[tokio::test]
async fn ac1_the_client_transport_method_reaches_the_served_arm_over_a_real_socket() {
    use rustain::adapters::a2a::client::A2aClientAdapter;
    use rustain::adapters::a2a::driver::TaskClient;
    use rustain::adapters::a2a::exec::SubmitterKey;
    use rustain::domain::models::{A2aPeerSource, A2aPeerSpec, RecipientItemState, RedactedUrl};

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
    seed_item(
        room.as_ref(),
        &SubmitterKey::loopback(),
        "ri_over_the_wire",
        "task-wire",
        RecipientItemState::Acknowledged {
            content: String::new(),
        },
    )
    .await;

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
            runtime: None,
            transparency: Arc::new(TransparencySink::new(room).with_reader(journal.clone())),
            policy: A2aAdmissionPolicy::Allow,
            workspace: workspace.path().to_path_buf(),
            advertised_host: None,
            cards: Arc::new(SignedCardCache::new()),
        },
        cancel.child_token(),
    ));

    // The SHIPPED client, not a hand-built request: `A2aClientAdapter` owns the
    // credential attachment, the anchor handling and the JSON-RPC envelope.
    let peer = A2aPeerSpec::new(
        "board-peer",
        RedactedUrl::from(endpoint.clone()),
        A2aPeerSource::Workspace,
    );
    let adapter = Arc::new(A2aClientAdapter::new(&peer, None).expect("client adapter"));
    let transport = TaskClient::new(adapter, endpoint);

    let result = transport
        .list_items()
        .await
        .expect("the peer serves the read verb the client asks for");
    assert_eq!(
        result["items"][0]["itemId"], "ri_over_the_wire",
        "the trait method must reach the served dispatch arm: {result}"
    );
    assert_eq!(
        result["items"][0]["state"], "acknowledged",
        "…and carry the acknowledgement state the board renders: {result}"
    );
    assert_eq!(
        result["principalCollapsed"], true,
        "…including the legibility disclosure the board is required to show"
    );

    cancel.cancel();
    http.await.expect("server task").expect("server shutdown");
}

// ── Story 19.16b review · the board's aggregation path (collect_board) ──────

/// A real sender-side egress over the given roster: the PRODUCTION compose
/// front door (`A2aEgress::compose` → `runtime()`), the same one the daemon
/// installs — ⛔ never a hand-built runtime (Rule 2 for the board half).
///
/// The caller seeds the sender journal (`RemoteEnvelopeDispatched` rows) and
/// the peer's own journal before collecting; card slots settle event-driven
/// (no sleep, no poll) through the compose-spawned boot fetches.
async fn board_egress(
    peers: Vec<rustain::domain::models::A2aPeerSpec>,
    journal: Arc<rustain::infrastructure::subagent::NodeJournal>,
    room: Arc<dyn RoomJournal>,
) -> rustain::adapters::a2a::egress::A2aEgress {
    let (event_tx, _event_rx) =
        tokio::sync::mpsc::unbounded_channel::<rustain::domain::events::AppEvent>();
    let egress = rustain::adapters::a2a::egress::A2aEgress::compose(
        peers,
        NodeTree::new(),
        room,
        journal,
        event_tx,
    )
    .expect("compose the sender egress");
    tokio::time::timeout(Duration::from_secs(10), egress.await_cards_settled())
        .await
        .expect("boot card fetches settle");
    egress
}

/// Story 19.16b AC3(b2) — a dead roster peer renders `⚠ unreachable`, ⛔ never
/// `✗ declined`: *"a host being down is not a person saying no"* (`:236`).
///
/// **The production mapping site is `collect_board`'s `Err(())` arm** — the
/// M10 mutation recipe's actual target. The unit test pins the token table;
/// THIS keystone drives the mapping through the real egress, a real closed
/// port, and a real live peer, so mutating the arm to `Declined` turns it RED
/// where the receipt's first run could not reach.
///
/// **Mutant → RED:** map `Err(())` to `BoardOutcome::Declined` in
/// `collect_board` — the `⚠` assertion fails on the dead peer's row.
#[tokio::test]
async fn ac3b2_an_unreachable_roster_peer_renders_warned_never_declined() {
    use rustain::adapters::a2a::exec::SubmitterKey;
    use rustain::domain::models::RecipientItemState;

    // The live peer: a real serve() holding one of this sender's items.
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
    seed_item(
        room.as_ref(),
        &SubmitterKey::loopback(),
        "ri_board_live",
        "task-board-live",
        RecipientItemState::Received {
            content: String::new(),
        },
    )
    .await;
    // …and the sender durably dispatched that task to this peer (`AC3(g)`).
    rustain::domain::ports::RoomJournal::record_event(
        room.as_ref(),
        rustain::domain::models::RoomEvent::RemoteEnvelopeDispatched {
            peer: rustain::domain::models::a2a_peer_spec::alias_pseudonym("board-live"),
            task: Some("task-board-live".to_owned()),
            bytes: 8,
            act: rustain::domain::models::DispatchAct::Task,
        },
    )
    .await
    .expect("seed the dispatched row");

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
            runtime: None,
            transparency: Arc::new(
                TransparencySink::new(room.clone()).with_reader(journal.clone()),
            ),
            policy: A2aAdmissionPolicy::Allow,
            workspace: workspace.path().to_path_buf(),
            advertised_host: None,
            cards: Arc::new(SignedCardCache::new()),
        },
        cancel.child_token(),
    ));

    // The dead peer: a port that answers nothing.
    let dead = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind dead listener");
        let addr = listener.local_addr().expect("dead address");
        drop(listener);
        addr
    };

    let live_peer = rustain::domain::models::A2aPeerSpec::new(
        "board-live",
        rustain::domain::models::RedactedUrl::from(endpoint),
        rustain::domain::models::A2aPeerSource::Workspace,
    );
    let dead_peer = rustain::domain::models::A2aPeerSpec::new(
        "board-dead",
        rustain::domain::models::RedactedUrl::from(format!("http://{dead}/")),
        rustain::domain::models::A2aPeerSource::Workspace,
    );
    let egress = board_egress(vec![dead_peer, live_peer], journal, room).await;

    let view =
        rustain::adapters::a2a::board::collect_board(egress.runtime(), std::time::Instant::now())
            .await
            .expect("the first refresh is admitted");
    let rendered = rustain::adapters::a2a::board::render_board(&view);

    assert!(
        rendered.contains("⚠ board-dead"),
        "the dead roster peer is warned about, in the ratified token: {rendered}"
    );
    assert!(
        !rendered.contains("declined") && !rendered.contains("✗"),
        "a host being down is not a person saying no (`UX-DR-TM-02:236`): {rendered}"
    );
    assert!(
        rendered.contains("board-live"),
        "positive control: the live peer's read landed and its row rendered: {rendered}"
    );

    cancel.cancel();
    http.await.expect("server task").expect("server shutdown");
}

/// Story 19.16b AC3(b) — the board is assembled from REMOTE reads, ⛔ never
/// from the local projection (`A21`): the sender's local journal holds what
/// it RECEIVED (an inbound item from a third peer), never what it sent, so a
/// local-projection build renders the board inverted — or empty.
///
/// **Mutant → RED:** build the board from the host's own
/// `JournalRecipientItemProjection` — the row set below stops matching (the
/// remote peer's row vanishes and/or the inbound item's principal leaks in).
///
/// **Positive control:** the inbound item this host holds is real and folded
/// — and its id appears NOWHERE on the board.
#[tokio::test]
async fn ac3_the_board_is_assembled_from_remote_reads_not_the_local_projection() {
    use rustain::adapters::a2a::exec::SubmitterKey;
    use rustain::domain::models::RecipientItemState;

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

    // The INBOUND item: what a THIRD peer sent to this host. The local
    // projection folds it; the board must never render it as outbound.
    let third_party = SubmitterKey::from_api_key("credential-third-party");
    seed_item(
        room.as_ref(),
        &third_party,
        "ri_inbound_from_third",
        "task-inbound",
        RecipientItemState::Received {
            content: String::new(),
        },
    )
    .await;
    // Positive control: the local projection really holds it.
    assert!(
        rustain::adapters::policy::JournalRecipientItemProjection::from_entries(
            &journal.load().await.expect("load journal")
        )
        .find_by_id("ri_inbound_from_third")
        .is_some(),
        "control: the local fold holds the inbound item the board must not show"
    );

    // The REMOTE item: the live peer's copy of what this sender dispatched.
    seed_item(
        room.as_ref(),
        &SubmitterKey::loopback(),
        "ri_board_sent",
        "task-board-sent",
        RecipientItemState::Acknowledged {
            content: String::new(),
        },
    )
    .await;
    rustain::domain::ports::RoomJournal::record_event(
        room.as_ref(),
        rustain::domain::models::RoomEvent::RemoteEnvelopeDispatched {
            peer: rustain::domain::models::a2a_peer_spec::alias_pseudonym("board-remote"),
            task: Some("task-board-sent".to_owned()),
            bytes: 8,
            act: rustain::domain::models::DispatchAct::Task,
        },
    )
    .await
    .expect("seed the dispatched row");

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
            runtime: None,
            transparency: Arc::new(
                TransparencySink::new(room.clone()).with_reader(journal.clone()),
            ),
            policy: A2aAdmissionPolicy::Allow,
            workspace: workspace.path().to_path_buf(),
            advertised_host: None,
            cards: Arc::new(SignedCardCache::new()),
        },
        cancel.child_token(),
    ));

    let peer = rustain::domain::models::A2aPeerSpec::new(
        "board-remote",
        rustain::domain::models::RedactedUrl::from(endpoint),
        rustain::domain::models::A2aPeerSource::Workspace,
    );
    let egress = board_egress(vec![peer], journal, room).await;

    let view =
        rustain::adapters::a2a::board::collect_board(egress.runtime(), std::time::Instant::now())
            .await
            .expect("the first refresh is admitted");
    let rendered = rustain::adapters::a2a::board::render_board(&view);

    assert_eq!(
        view.rows.len(),
        1,
        "exactly the remote peer's row — assembled from the remote read: {rendered}"
    );
    assert!(
        rendered.contains("✓ board-remote"),
        "the dispatched item's acknowledgement is what the REMOTE read reports: {rendered}"
    );
    assert!(
        !rendered.contains("ri_inbound_from_third") && !rendered.contains("task-inbound"),
        "the host's own inbound item is not this sender's outbound fan-out — the board \
         inverted (`A21`): {rendered}"
    );

    cancel.cancel();
    http.await.expect("server task").expect("server shutdown");
}

/// Story 19.16b AC3(g) — under a collapsed principal (this loopback harness),
/// the peer's set mixes every local caller's items; the row answers for THIS
/// sender's dispatched task, not the peer's newest bag.
///
/// **Mutant → RED:** drop the task correlation from `parse_peer_reply` — the
/// newest item overall (someone else's acknowledged send) wins the row and it
/// renders `✓` instead of `●`.
#[tokio::test]
async fn ac3g_the_board_row_answers_for_the_dispatched_task_not_the_newest_bag() {
    use rustain::adapters::a2a::exec::SubmitterKey;
    use rustain::domain::models::RecipientItemState;

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

    // Two items under the SAME (loopback) principal on the peer: another
    // local's acknowledged send, newest; ours, plain received.
    seed_item(
        room.as_ref(),
        &SubmitterKey::loopback(),
        "ri_other_local",
        "task-other-local",
        RecipientItemState::Acknowledged {
            content: String::new(),
        },
    )
    .await;
    seed_item(
        room.as_ref(),
        &SubmitterKey::loopback(),
        "ri_ours",
        "task-ours",
        RecipientItemState::Received {
            content: String::new(),
        },
    )
    .await;
    // The sender durably dispatched ONLY task-ours to this peer.
    rustain::domain::ports::RoomJournal::record_event(
        room.as_ref(),
        rustain::domain::models::RoomEvent::RemoteEnvelopeDispatched {
            peer: rustain::domain::models::a2a_peer_spec::alias_pseudonym("board-ours"),
            task: Some("task-ours".to_owned()),
            bytes: 8,
            act: rustain::domain::models::DispatchAct::Task,
        },
    )
    .await
    .expect("seed the dispatched row");

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
            runtime: None,
            transparency: Arc::new(
                TransparencySink::new(room.clone()).with_reader(journal.clone()),
            ),
            policy: A2aAdmissionPolicy::Allow,
            workspace: workspace.path().to_path_buf(),
            advertised_host: None,
            cards: Arc::new(SignedCardCache::new()),
        },
        cancel.child_token(),
    ));

    let peer = rustain::domain::models::A2aPeerSpec::new(
        "board-ours",
        rustain::domain::models::RedactedUrl::from(endpoint),
        rustain::domain::models::A2aPeerSource::Workspace,
    );
    let egress = board_egress(vec![peer], journal, room).await;

    let view =
        rustain::adapters::a2a::board::collect_board(egress.runtime(), std::time::Instant::now())
            .await
            .expect("the first refresh is admitted");
    let rendered = rustain::adapters::a2a::board::render_board(&view);

    assert_eq!(view.rows.len(), 1, "{rendered}");
    assert!(
        rendered.contains("● board-ours"),
        "the row reports OUR dispatched item (`received`), not the newest bag's \
         `acknowledged` belonging to another local caller: {rendered}"
    );

    cancel.cancel();
    http.await.expect("server task").expect("server shutdown");
}

/// Story 19.16b AC2(c) — on a loopback bind the read SERVES and the board
/// DISCLOSES the collapse, ⛔ never only in a log.
///
/// **Mutant → RED:** drop the disclosure (or accumulate it from peers that
/// contribute no row and render it unconditionally — the unit test pins the
/// conditional; this pins the presence, from a real loopback peer that
/// contributed a real row).
#[tokio::test]
async fn ac2c_the_board_carries_the_collapse_disclosure_from_a_loopback_peer() {
    use rustain::adapters::a2a::exec::SubmitterKey;
    use rustain::domain::models::RecipientItemState;

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
    seed_item(
        room.as_ref(),
        &SubmitterKey::loopback(),
        "ri_collapse",
        "task-collapse",
        RecipientItemState::Received {
            content: String::new(),
        },
    )
    .await;
    rustain::domain::ports::RoomJournal::record_event(
        room.as_ref(),
        rustain::domain::models::RoomEvent::RemoteEnvelopeDispatched {
            peer: rustain::domain::models::a2a_peer_spec::alias_pseudonym("board-loop"),
            task: Some("task-collapse".to_owned()),
            bytes: 8,
            act: rustain::domain::models::DispatchAct::Task,
        },
    )
    .await
    .expect("seed the dispatched row");

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
            runtime: None,
            transparency: Arc::new(
                TransparencySink::new(room.clone()).with_reader(journal.clone()),
            ),
            policy: A2aAdmissionPolicy::Allow,
            workspace: workspace.path().to_path_buf(),
            advertised_host: None,
            cards: Arc::new(SignedCardCache::new()),
        },
        cancel.child_token(),
    ));

    let peer = rustain::domain::models::A2aPeerSpec::new(
        "board-loop",
        rustain::domain::models::RedactedUrl::from(endpoint),
        rustain::domain::models::A2aPeerSource::Workspace,
    );
    let egress = board_egress(vec![peer], journal, room).await;

    let view =
        rustain::adapters::a2a::board::collect_board(egress.runtime(), std::time::Instant::now())
            .await
            .expect("the first refresh is admitted");
    let rendered = rustain::adapters::a2a::board::render_board(&view);

    assert!(
        view.principal_collapsed,
        "the loopback peer's reply carries the collapse, and the row it produced \
         accumulates it: {rendered}"
    );
    assert!(
        rendered.contains(rustain::adapters::a2a::board::COLLAPSED_PRINCIPAL_DISCLOSURE),
        "the disclosure renders ON the board, never only in a log: {rendered}"
    );

    cancel.cancel();
    http.await.expect("server task").expect("server shutdown");
}

// ── Story 19.16d · the cross-host retract target ────────────────────────────
//
// ⚠ **What this half CANNOT prove, stated rather than hidden (`19-16b A13`).**
// Every harness here binds `127.0.0.1`, and `authenticate` returns
// `SubmitterKey::loopback()` before the api-key branch on a loopback bind, so
// on this wire every caller is ONE principal. The wire therefore proves the
// front door, the principal's *source* (the authenticated caller, never a
// parameter) and the byte-identity of not-yours versus never-existed against
// a SECOND principal seeded directly into the journal. That two configured
// credentials are two principals is proven where it is decided — the
// projection — in `tests/conformance_19_16d_retract.rs`, the shipped
// `a2a_server_exec.rs` precedent's split.

/// A real journal on a temp workspace, with the room writer the ingress and
/// the daemon rails both use.
async fn retract_room() -> (tempfile::TempDir, Arc<NodeJournal>, Arc<dyn RoomJournal>) {
    let workspace = tempfile::tempdir().expect("workspace");
    let journal = Arc::new(
        NodeJournal::open_workspace(workspace.path())
            .await
            .expect("open real node journal"),
    );
    let room: Arc<dyn RoomJournal> = Arc::new(NodeRoomJournal::new(journal.clone(), None));
    (workspace, journal, room)
}

/// Serve the real router over a real loopback socket. `sink` is the journal
/// the server appends through; the ordinary reader is the real journal, so a
/// `BrokenRoomJournal` sink still serves a fold of the seeded items.
async fn serve_retract_host(
    workspace: &std::path::Path,
    journal: Arc<NodeJournal>,
    sink: Arc<dyn RoomJournal>,
    policy: A2aAdmissionPolicy,
) -> (
    String,
    CancellationToken,
    tokio::task::JoinHandle<anyhow::Result<()>>,
    tempfile::TempDir,
) {
    serve_retract_host_with_reader(workspace, journal, sink, policy).await
}

async fn serve_retract_host_with_reader(
    workspace: &std::path::Path,
    reader: Arc<dyn RoomJournalReader>,
    sink: Arc<dyn RoomJournal>,
    policy: A2aAdmissionPolicy,
) -> (
    String,
    CancellationToken,
    tokio::task::JoinHandle<anyhow::Result<()>>,
    tempfile::TempDir,
) {
    let key_dir = tempfile::tempdir().expect("identity directory");
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
            runtime: None,
            transparency: Arc::new(TransparencySink::new(sink).with_reader(reader)),
            policy,
            workspace: workspace.to_path_buf(),
            advertised_host: None,
            cards: Arc::new(SignedCardCache::new()),
        },
        cancel.child_token(),
    ));
    (endpoint, cancel, http, key_dir)
}

/// Every `RecipientItemRetracted` record in the durable journal, in order.
async fn retract_records(journal: &NodeJournal) -> Vec<rustain::domain::models::RoomEvent> {
    use rustain::domain::models::{JournalRecord, RoomEvent};
    journal
        .load()
        .await
        .expect("load journal")
        .into_iter()
        .filter_map(|entry| match entry.record {
            JournalRecord::Room(event @ RoomEvent::RecipientItemRetracted { .. }) => Some(event),
            _ => None,
        })
        .collect()
}

async fn journal_len(journal: &NodeJournal) -> usize {
    journal.load().await.expect("load journal").len()
}

/// One item's row from `x-rustain-items/list`, read through the front door.
async fn listed_item(
    client: &reqwest::Client,
    endpoint: &str,
    id: u64,
    item_id: &str,
) -> serde_json::Value {
    let response = rpc(
        client,
        endpoint,
        id,
        "x-rustain-items/list",
        serde_json::json!({}),
    )
    .await;
    response["result"]["items"]
        .as_array()
        .unwrap_or_else(|| panic!("the read returns a set: {response}"))
        .iter()
        .find(|item| item["itemId"] == item_id)
        .cloned()
        .unwrap_or_else(|| panic!("{item_id} is in the caller's set: {response}"))
}

fn received_item() -> rustain::domain::models::RecipientItemState {
    rustain::domain::models::RecipientItemState::Received {
        content: String::new(),
    }
}

/// Story 19.16d AC1(a)/(c), AC2(c), AC4(a), AC5(a) — the retract lands through
/// the real front door on the caller's OWN item, and every face agrees.
///
/// Front door: `rpc` → the real axum router → `authenticate` → `dispatch`'s
/// fifth arm. ⛔ `items_retract` is private, so the bypass is unavailable.
///
/// **Mutant → RED (`M01`):** respell the SERVED arm's method string — the
/// canonical `x-rustain-items/retract` answers `-32601`.
/// **Mutant → RED (`M04`):** reject an unknown payload key — the retract
/// carrying `aFieldFromANewerBuild` is refused instead of landing.
/// **Mutant → RED (`M12`):** carry the mark as a fifth `state` — the list
/// stops answering `received` for the retracted item.
/// **Positive control (`M12`):** the un-retracted sibling carries NO
/// `retractedAtMs` key at all.
/// **Mutant → RED (`M09`):** drop the collapse disclosure — the loopback
/// caller's response, and the durable record's export line, stop carrying it.
/// **Mutant → RED (`M15`):** delete the `transparency_row` arm — the folded
/// ledger has no `item-retracted` row.
#[tokio::test]
async fn ac1_a_retract_marks_the_callers_own_item_through_the_real_front_door() {
    use rustain::adapters::a2a::exec::SubmitterKey;
    use rustain::domain::models::RoomEvent;
    use rustain::domain::services::transparency::render_export;

    let (workspace, journal, room) = retract_room().await;
    let caller = SubmitterKey::loopback();
    seed_item(
        room.as_ref(),
        &caller,
        "ri_mine",
        "task-mine",
        received_item(),
    )
    .await;
    seed_item(
        room.as_ref(),
        &caller,
        "ri_kept",
        "task-kept",
        received_item(),
    )
    .await;
    let (endpoint, cancel, http, _keys) = serve_retract_host(
        workspace.path(),
        journal.clone(),
        room.clone(),
        A2aAdmissionPolicy::Allow,
    )
    .await;
    let client = reqwest::Client::new();

    let response = rpc(
        &client,
        &endpoint,
        1,
        "x-rustain-items/retract",
        serde_json::json!({ "itemId": "ri_mine", "aFieldFromANewerBuild": 7 }),
    )
    .await;
    let result = response.get("result").unwrap_or_else(|| {
        panic!("the served arm answers, and an unknown field is ignored: {response}")
    });
    assert_eq!(result["itemId"], "ri_mine");
    assert_eq!(result["alreadyRetracted"], false, "{response}");
    let stamp = result["retractedAtMs"]
        .as_i64()
        .unwrap_or_else(|| panic!("the recipient host minted a stamp: {response}"));
    assert!(
        stamp > 0,
        "a real wall-clock stamp, never the 0 sentinel: {response}"
    );
    assert_eq!(
        result["principalCollapsed"], true,
        "a loopback caller is every local caller — the write must say so: {response}"
    );

    // AC4(a): the list reports the mark as an ADDITIVE sibling, `state` untouched.
    let marked = listed_item(&client, &endpoint, 2, "ri_mine").await;
    assert_eq!(
        marked["state"], "received",
        "⛔ never a fifth state: {marked}"
    );
    assert_eq!(marked["retractedAtMs"], stamp, "{marked}");
    let kept = listed_item(&client, &endpoint, 3, "ri_kept").await;
    assert!(
        kept.get("retractedAtMs").is_none(),
        "positive control: an un-retracted item carries no mark key at all: {kept}"
    );

    // The durable record: host-minted, one line, with the collapse persisted.
    let records = retract_records(&journal).await;
    assert_eq!(records.len(), 1, "{records:?}");
    let RoomEvent::RecipientItemRetracted {
        address,
        retracted_at_ms,
        principal_collapsed,
    } = &records[0]
    else {
        unreachable!("filtered to retract records");
    };
    assert_eq!(address.item().as_str(), "ri_mine");
    assert_eq!(*retracted_at_ms, stamp);
    assert!(
        *principal_collapsed,
        "the collapse is a durable fact, not only a response body"
    );

    // AC5(a): the ledger renders the retract as its own row on every face.
    let rows = fold_transparency(&journal.load().await.expect("load journal"));
    let row = rows
        .iter()
        .find(|row| row.kind == TransparencyKind::RecipientItemRetracted)
        .unwrap_or_else(|| panic!("the retract has a ledger row: {rows:?}"));
    assert_eq!(row.summary, "the sender retracted item ri_mine");
    assert_eq!(
        row.retracted_at_ms,
        Some(stamp),
        "the event's host-minted stamp reaches the one row field every ledger face reads"
    );
    assert!(
        row.one_line().contains("⇠ item-retracted"),
        "{}",
        row.one_line()
    );
    let export = render_export(std::slice::from_ref(row));
    assert!(
        export.contains("\"principalCollapsed\":true"),
        "the durable collapse reaches the export: {export}"
    );
    assert!(
        export.contains(&format!("\"retractedAtMs\":{stamp}")),
        "the retract mark reaches the export: {export}"
    );

    cancel.cancel();
    http.await.expect("server task").expect("server shutdown");
}

/// Story 19.16d AC1(c)/(d) — the principal comes from the authenticated caller
/// and nothing else, and not-yours is byte-identical to never-existed.
///
/// A second principal's item is seeded straight into the journal (the wire
/// cannot mint one — every loopback caller is one principal).
///
/// **Mutant → RED (`M03`/`M06`):** move the principal derivation out of the
/// `dispatch` arm and read it from `params` — the forged `principal` below
/// names credential-B's pseudonym and B's item gets marked.
/// **Positive control (`M03`):** the caller's own item IS retracted, so the
/// check discriminates rather than refusing everything.
/// **Mutant → RED (`M05`):** a second error construction for "not yours" — the
/// two serialized responses stop being equal.
#[tokio::test]
async fn ac1d_not_yours_is_byte_identical_to_never_existed_and_no_parameter_names_the_principal() {
    use rustain::adapters::a2a::exec::SubmitterKey;

    let (workspace, journal, room) = retract_room().await;
    let caller = SubmitterKey::loopback();
    let other = SubmitterKey::from_api_key("credential-b");
    seed_item(
        room.as_ref(),
        &caller,
        "ri_mine",
        "task-mine",
        received_item(),
    )
    .await;
    seed_item(
        room.as_ref(),
        &other,
        "ri_theirs",
        "task-theirs",
        received_item(),
    )
    .await;
    let (endpoint, cancel, http, _keys) = serve_retract_host(
        workspace.path(),
        journal.clone(),
        room.clone(),
        A2aAdmissionPolicy::Allow,
    )
    .await;
    let client = reqwest::Client::new();

    // The SAME JSON-RPC id in both probes, so the echoed id cannot mask a
    // difference (`a2a_server_exec.rs` byte-identity idiom).
    let forged_principal = other.pseudonymous_peer_id().as_str().to_owned();
    let theirs = rpc(
        &client,
        &endpoint,
        7,
        "x-rustain-items/retract",
        serde_json::json!({ "itemId": "ri_theirs", "principal": forged_principal }),
    )
    .await;
    let fabricated = rpc(
        &client,
        &endpoint,
        7,
        "x-rustain-items/retract",
        serde_json::json!({ "itemId": "ri_never_minted", "principal": forged_principal }),
    )
    .await;
    assert_eq!(
        serde_json::to_string(&theirs).unwrap(),
        serde_json::to_string(&fabricated).unwrap(),
        "a foreign principal's real item must answer byte-identically to an id that never \
         existed (ADR-17-4a-01 R21)"
    );
    assert_eq!(theirs["error"]["code"], CODE_TASK_NOT_FOUND, "{theirs}");
    assert!(
        retract_records(&journal).await.is_empty(),
        "a forged `principal` parameter must not reach another principal's item"
    );

    // Positive control: the caller's own item is retractable.
    let mine = rpc(
        &client,
        &endpoint,
        8,
        "x-rustain-items/retract",
        serde_json::json!({ "itemId": "ri_mine" }),
    )
    .await;
    assert!(mine.get("result").is_some(), "{mine}");
    assert_eq!(retract_records(&journal).await.len(), 1);

    // A malformed request is a different operator fact from an unaddressable
    // item: ⛔ never `ITEM_NOT_FOUND` for an absent or empty id.
    for params in [serde_json::json!({}), serde_json::json!({ "itemId": "" })] {
        let malformed = rpc(&client, &endpoint, 9, "x-rustain-items/retract", params).await;
        assert_eq!(
            malformed["error"]["code"], CODE_INVALID_PARAMS,
            "{malformed}"
        );
    }

    cancel.cancel();
    http.await.expect("server task").expect("server shutdown");
}

/// Story 19.16d AC1(b) (`P3`) — a retract NOTIFICATION is refused with
/// `-32600` and writes nothing, because its collapse disclosure rides the
/// response and a 204 would make it undeliverable by the caller's choice.
///
/// ⛔ Not through `rpc()`, which always sends an id: a raw POST with no `id`.
/// **Mutant → RED (`M02`):** accept the notification and perform the write —
/// the answer becomes a 204 and the list shows the item marked.
/// **Positive control:** the identical body WITH an id marks the item.
#[tokio::test]
async fn ac1b_a_retract_notification_is_refused_aloud_and_writes_nothing() {
    use rustain::adapters::a2a::exec::SubmitterKey;

    let (workspace, journal, room) = retract_room().await;
    seed_item(
        room.as_ref(),
        &SubmitterKey::loopback(),
        "ri_note",
        "task-note",
        received_item(),
    )
    .await;
    let (endpoint, cancel, http, _keys) = serve_retract_host(
        workspace.path(),
        journal.clone(),
        room.clone(),
        A2aAdmissionPolicy::Allow,
    )
    .await;
    let client = reqwest::Client::new();

    let notification = client
        .post(&endpoint)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "x-rustain-items/retract",
            "params": { "itemId": "ri_note" },
        }))
        .send()
        .await
        .expect("real listener response");
    assert_eq!(
        notification.status(),
        reqwest::StatusCode::OK,
        "⛔ never the silent 204 a write's disclosure cannot ride"
    );
    let body: serde_json::Value = notification.json().await.expect("a JSON-RPC error body");
    assert_eq!(body["error"]["code"], CODE_INVALID_REQUEST, "{body}");
    let unmarked = listed_item(&client, &endpoint, 1, "ri_note").await;
    assert!(unmarked.get("retractedAtMs").is_none(), "{unmarked}");
    assert!(retract_records(&journal).await.is_empty());

    let call = rpc(
        &client,
        &endpoint,
        2,
        "x-rustain-items/retract",
        serde_json::json!({ "itemId": "ri_note" }),
    )
    .await;
    assert!(call.get("result").is_some(), "positive control: {call}");
    let marked = listed_item(&client, &endpoint, 3, "ri_note").await;
    assert!(marked.get("retractedAtMs").is_some(), "{marked}");

    cancel.cancel();
    http.await.expect("server task").expect("server shutdown");
}

/// Story 19.16d AC1(e) — idempotent on the live states (no second append, no
/// word), refused on a tombstone (`Q3`) — including a tombstone written on the
/// OTHER rail after startup, which this host's fold has not yet seen.
///
/// **Ratchet (Rule 4):** journal length is EQUAL across N retracts after the
/// first. **Positive control (REQUIRED):** the first retract adds exactly one.
/// **Mutant → RED (`M07`):** append on every call.
/// **Mutant → RED:** decide on the cached fold without catching it up inside
/// the critical section — the daemon-rail removal is invisible and the
/// tombstone gets marked instead of refused.
#[tokio::test]
async fn ac1e_a_retract_is_idempotent_while_live_and_refused_on_a_tombstone() {
    use rustain::adapters::a2a::exec::SubmitterKey;
    use rustain::adapters::a2a::jsonrpc::CODE_ITEM_REMOVED;
    use rustain::domain::models::{ItemAddress, ItemId, RecipientItemState, RoomEvent};

    let (workspace, journal, room) = retract_room().await;
    let caller = SubmitterKey::loopback();
    seed_item(
        room.as_ref(),
        &caller,
        "ri_twice",
        "task-twice",
        received_item(),
    )
    .await;
    seed_item(
        room.as_ref(),
        &caller,
        "ri_gone",
        "task-gone",
        RecipientItemState::Removed {
            acknowledged_before: false,
        },
    )
    .await;
    seed_item(
        room.as_ref(),
        &caller,
        "ri_later",
        "task-later",
        received_item(),
    )
    .await;
    let (endpoint, cancel, http, _keys) = serve_retract_host(
        workspace.path(),
        journal.clone(),
        room.clone(),
        A2aAdmissionPolicy::Allow,
    )
    .await;
    let client = reqwest::Client::new();

    let before = journal_len(&journal).await;
    let first = rpc(
        &client,
        &endpoint,
        1,
        "x-rustain-items/retract",
        serde_json::json!({ "itemId": "ri_twice" }),
    )
    .await;
    assert_eq!(first["result"]["alreadyRetracted"], false, "{first}");
    assert_eq!(
        journal_len(&journal).await,
        before + 1,
        "positive control: the first retract appends exactly one record"
    );
    let after_first = journal_len(&journal).await;
    for id in 2..=4 {
        let again = rpc(
            &client,
            &endpoint,
            id,
            "x-rustain-items/retract",
            serde_json::json!({ "itemId": "ri_twice" }),
        )
        .await;
        assert_eq!(again["result"]["alreadyRetracted"], true, "{again}");
        assert_eq!(
            again["result"]["retractedAtMs"], first["result"]["retractedAtMs"],
            "the first mark wins: {again}"
        );
    }
    assert_eq!(
        journal_len(&journal).await,
        after_first,
        "an already-marked item appends nothing"
    );

    // A tombstone refuses aloud — and ⛔ not as `ITEM_NOT_FOUND`.
    let tombstone = rpc(
        &client,
        &endpoint,
        5,
        "x-rustain-items/retract",
        serde_json::json!({ "itemId": "ri_gone" }),
    )
    .await;
    assert_eq!(tombstone["error"]["code"], CODE_ITEM_REMOVED, "{tombstone}");

    // The operator removes `ri_later` on the daemon rail AFTER startup: the
    // record reaches the journal, never this host's in-memory fold.
    room.record_event(RoomEvent::RecipientItemRemoved {
        address: ItemAddress::from_a2a_ingress(
            caller.pseudonymous_peer_id(),
            ItemId::from_replay("ri_later"),
        ),
    })
    .await
    .expect("daemon-rail removal");
    let late = rpc(
        &client,
        &endpoint,
        6,
        "x-rustain-items/retract",
        serde_json::json!({ "itemId": "ri_later" }),
    )
    .await;
    assert_eq!(
        late["error"]["code"], CODE_ITEM_REMOVED,
        "a removal on the other rail must be seen before the retract decides (Q3): {late}"
    );
    assert_eq!(
        retract_records(&journal).await.len(),
        1,
        "neither tombstone was marked"
    );

    cancel.cancel();
    http.await.expect("server task").expect("server shutdown");
}

/// A daemon-rail removal can append after the retract caught its projection up
/// but before the retract record lands. The reply must follow durable order,
/// not the stale in-memory decision.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ac3f_an_interleaved_removal_is_refolded_before_retract_answers() {
    use rustain::adapters::a2a::exec::SubmitterKey;
    use rustain::adapters::a2a::jsonrpc::CODE_ITEM_REMOVED;
    use rustain::domain::models::{ItemAddress, ItemId, RoomEvent};

    let (workspace, journal, room) = retract_room().await;
    let caller = SubmitterKey::loopback();
    seed_item(
        room.as_ref(),
        &caller,
        "ri_interleaved",
        "task-interleaved",
        received_item(),
    )
    .await;

    let append_started = Arc::new(Notify::new());
    let resume_append = Arc::new(Notify::new());
    let blocking_sink: Arc<dyn RoomJournal> = Arc::new(BlockingRetractJournal {
        inner: room.clone(),
        append_started: append_started.clone(),
        resume_append: resume_append.clone(),
    });
    let (endpoint, cancel, http, _keys) = serve_retract_host(
        workspace.path(),
        journal.clone(),
        blocking_sink,
        A2aAdmissionPolicy::Allow,
    )
    .await;

    let request = {
        let endpoint = endpoint.clone();
        tokio::spawn(async move {
            rpc(
                &reqwest::Client::new(),
                &endpoint,
                1,
                "x-rustain-items/retract",
                serde_json::json!({ "itemId": "ri_interleaved" }),
            )
            .await
        })
    };
    append_started.notified().await;
    room.record_event(RoomEvent::RecipientItemRemoved {
        address: ItemAddress::from_a2a_ingress(
            caller.pseudonymous_peer_id(),
            ItemId::from_replay("ri_interleaved"),
        ),
    })
    .await
    .expect("daemon-rail removal");
    resume_append.notify_one();

    let response = request.await.expect("retract request");
    assert_eq!(
        response["error"]["code"], CODE_ITEM_REMOVED,
        "the durable removal precedes the retract record, so the tombstone refuses the mark: \
         {response}"
    );
    let item = listed_item(&reqwest::Client::new(), &endpoint, 2, "ri_interleaved").await;
    assert!(
        item.get("retractedAtMs").is_none(),
        "the in-memory view agrees with cold replay: {item}"
    );

    cancel.cancel();
    http.await.expect("server task").expect("server shutdown");
}

/// A write may use a cached fold only after proving it is current. A failed
/// tail probe or growth fold refuses before append instead of reporting a mark
/// that replay may reject.
#[tokio::test]
async fn ac3f_journal_refresh_failures_refuse_before_append() {
    use rustain::adapters::a2a::exec::SubmitterKey;
    use rustain::adapters::a2a::jsonrpc::CODE_INTERNAL_ERROR;
    use rustain::domain::models::RoomEvent;

    for failure_mode in [1, 2] {
        let (workspace, journal, room) = retract_room().await;
        seed_item(
            room.as_ref(),
            &SubmitterKey::loopback(),
            "ri_stale",
            "task-stale",
            received_item(),
        )
        .await;
        let mode = Arc::new(std::sync::atomic::AtomicU8::new(0));
        let reader: Arc<dyn RoomJournalReader> = Arc::new(ToggleFailingReader {
            inner: journal.clone(),
            failure_mode: mode.clone(),
        });
        let (endpoint, cancel, http, _keys) = serve_retract_host_with_reader(
            workspace.path(),
            reader,
            room.clone(),
            A2aAdmissionPolicy::Allow,
        )
        .await;
        let client = reqwest::Client::new();
        let _ = listed_item(&client, &endpoint, 1, "ri_stale").await;
        if failure_mode == 2 {
            room.record_event(RoomEvent::Unrecognized)
                .await
                .expect("grow journal beyond the projection watermark");
        }
        let before = journal_len(&journal).await;
        mode.store(failure_mode, std::sync::atomic::Ordering::SeqCst);

        let response = rpc(
            &client,
            &endpoint,
            2,
            "x-rustain-items/retract",
            serde_json::json!({ "itemId": "ri_stale" }),
        )
        .await;
        assert_eq!(
            response["error"]["code"], CODE_INTERNAL_ERROR,
            "a mutating decision cannot use a stale cached projection (mode {failure_mode}): \
             {response}"
        );
        assert_eq!(
            journal_len(&journal).await,
            before,
            "a failed refresh refuses before appending a retract (mode {failure_mode})"
        );

        cancel.cancel();
        http.await.expect("server task").expect("server shutdown");
    }
}

/// Story 19.16d AC1(e)/AC3(f) (`P4`) — N PARALLEL retracts of one item append
/// EXACTLY ONE record. The sequential ratchet above is green under every
/// interleaving; this one holds only because decide + append + apply share
/// one critical section.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ac3f_parallel_retracts_of_one_item_append_exactly_one_record() {
    use rustain::adapters::a2a::exec::SubmitterKey;

    let (workspace, journal, room) = retract_room().await;
    seed_item(
        room.as_ref(),
        &SubmitterKey::loopback(),
        "ri_race",
        "task-race",
        received_item(),
    )
    .await;
    let (endpoint, cancel, http, _keys) = serve_retract_host(
        workspace.path(),
        journal.clone(),
        room.clone(),
        A2aAdmissionPolicy::Allow,
    )
    .await;
    let client = reqwest::Client::new();

    let calls = (0..16u64).map(|id| {
        let client = client.clone();
        let endpoint = endpoint.clone();
        tokio::spawn(async move {
            rpc(
                &client,
                &endpoint,
                id,
                "x-rustain-items/retract",
                serde_json::json!({ "itemId": "ri_race" }),
            )
            .await
        })
    });
    let responses = futures::future::join_all(calls).await;
    let landed = responses
        .iter()
        .map(|response| response.as_ref().expect("retract task"))
        .filter(|response| response["result"]["alreadyRetracted"] == false)
        .count();
    assert_eq!(landed, 1, "exactly one call landed the mark: {responses:?}");
    assert_eq!(
        retract_records(&journal).await.len(),
        1,
        "N parallel retracts of one item append exactly one record"
    );

    cancel.cancel();
    http.await.expect("server task").expect("server shutdown");
}

/// Story 19.16d AC1(g) (`P8`, `Q2`) — the write consults the admission core.
/// `deny` (the default) refuses; `ask` refuses too, naming why; neither
/// touches the journal, and the refusal is ⛔ not `ITEM_NOT_FOUND`.
///
/// **Mutant → RED (`M21`):** drop the policy consult — both hosts mark the
/// item. **Positive control:** under `allow` the same retract lands
/// (`ac1_a_retract_marks_the_callers_own_item_through_the_real_front_door`),
/// and the shipped `the_default_policy_refuses` proves the policy path fires.
#[tokio::test]
async fn ac1g_deny_and_ask_refuse_the_retract_before_any_mutation() {
    use rustain::adapters::a2a::exec::SubmitterKey;
    use rustain::adapters::a2a::jsonrpc::CODE_REFUSED_BY_POLICY;

    for (policy, cause) in [
        (A2aAdmissionPolicy::default(), "\"deny\""),
        (
            A2aAdmissionPolicy::Ask,
            "the ask policy has no approval shape for a non-task verb",
        ),
    ] {
        let (workspace, journal, room) = retract_room().await;
        seed_item(
            room.as_ref(),
            &SubmitterKey::loopback(),
            "ri_policy",
            "task-policy",
            received_item(),
        )
        .await;
        let (endpoint, cancel, http, _keys) =
            serve_retract_host(workspace.path(), journal.clone(), room.clone(), policy).await;
        let client = reqwest::Client::new();

        let refused = rpc(
            &client,
            &endpoint,
            1,
            "x-rustain-items/retract",
            serde_json::json!({ "itemId": "ri_policy" }),
        )
        .await;
        assert_eq!(
            refused["error"]["code"], CODE_REFUSED_BY_POLICY,
            "{refused}"
        );
        let message = refused["error"]["message"].as_str().expect("a reason");
        assert!(
            message.contains("disabled by policy") && message.contains(cause),
            "{policy:?}: the refusal names the policy and its cause: {message}"
        );
        assert!(retract_records(&journal).await.is_empty(), "{policy:?}");
        let item = listed_item(&client, &endpoint, 2, "ri_policy").await;
        assert!(item.get("retractedAtMs").is_none(), "{policy:?}: {item}");

        cancel.cancel();
        http.await.expect("server task").expect("server shutdown");
    }
}

/// Story 19.16d AC3(f) — journal first, fail closed. A journal that cannot
/// append refuses the retract and leaves the item unmarked.
///
/// **Mutant → RED (`M11`):** apply before journalling, or ignore the append
/// error — the list shows the item marked with no durable record behind it.
#[tokio::test]
async fn ac3f_a_journal_that_cannot_append_fails_the_retract_closed() {
    use rustain::adapters::a2a::exec::SubmitterKey;
    use rustain::adapters::a2a::jsonrpc::CODE_INTERNAL_ERROR;

    let (workspace, journal, room) = retract_room().await;
    seed_item(
        room.as_ref(),
        &SubmitterKey::loopback(),
        "ri_disk",
        "task-disk",
        received_item(),
    )
    .await;
    let (endpoint, cancel, http, _keys) = serve_retract_host(
        workspace.path(),
        journal.clone(),
        Arc::new(BrokenRoomJournal),
        A2aAdmissionPolicy::Allow,
    )
    .await;
    let client = reqwest::Client::new();

    let refused = rpc(
        &client,
        &endpoint,
        1,
        "x-rustain-items/retract",
        serde_json::json!({ "itemId": "ri_disk" }),
    )
    .await;
    assert_eq!(refused["error"]["code"], CODE_INTERNAL_ERROR, "{refused}");
    let item = listed_item(&client, &endpoint, 2, "ri_disk").await;
    assert!(
        item.get("retractedAtMs").is_none(),
        "a mark with no durable record behind it is a lie the next restart erases: {item}"
    );

    cancel.cancel();
    http.await.expect("server task").expect("server shutdown");
}

/// Story 19.16g — the recipient learns a retract happened. A retract served
/// through the real front door (19.16d's `x-rustain-items/retract`) lands in
/// the recipient's durable journal; a client whose log was never opened, whose
/// observer started BEFORE the retract, discovers it at its next scheduled
/// observation and renders it in the real status bar — with no daemon event,
/// no subscription and no push. This is the sender/recipient-boundary half;
/// the PTY scenario proves the rendered surface separately.
///
/// **Mutant → RED (`M04`, observer half):** a tick that never starts an
/// observation leaves the reminder hidden after the served retract.
#[tokio::test]
async fn story_19_16g_a_served_retract_reaches_a_closed_log_reminder() {
    use rustain::adapters::a2a::exec::SubmitterKey;
    use rustain::adapters::tui::state::{LogAwareness, LogAwarenessView};
    use rustain::infrastructure::transparency_awareness::LogAwarenessObserver;

    let (workspace, journal, room) = retract_room().await;
    let caller = SubmitterKey::loopback();
    seed_item(
        room.as_ref(),
        &caller,
        "ri_mine",
        "task-mine",
        received_item(),
    )
    .await;

    // The recipient's client starts first and observes the received item.
    let t0 = std::time::Instant::now();
    let mut view = LogAwarenessView::default();
    let mut observer = LogAwarenessObserver::for_workspace(workspace.path());
    observer.tick_at(t0, &mut view);
    observer.settle(&mut view).await;
    let LogAwareness::Unseen(before) = view.display else {
        panic!(
            "the received item is itself a ledger row: {:?}",
            view.display
        );
    };

    let (endpoint, cancel, http, _keys) = serve_retract_host(
        workspace.path(),
        journal.clone(),
        room.clone(),
        A2aAdmissionPolicy::Allow,
    )
    .await;
    let response = rpc(
        &reqwest::Client::new(),
        &endpoint,
        1,
        "x-rustain-items/retract",
        serde_json::json!({ "itemId": "ri_mine" }),
    )
    .await;
    assert_eq!(response["result"]["itemId"], "ri_mine", "{response}");
    assert_eq!(retract_records(&journal).await.len(), 1);

    observer.tick_at(t0 + Duration::from_secs(1), &mut view);
    observer.settle(&mut view).await;
    assert_eq!(view.display, LogAwareness::Unseen(before + 1));

    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 1)).unwrap();
    let theme = rustain::adapters::tui::theme::Theme::dark();
    terminal
        .draw(|frame| {
            rustain::adapters::tui::widgets::status_bar::render(
                frame,
                frame.area(),
                "(daemon)",
                None,
                &rustain::domain::models::StatusState::Idle,
                &theme,
                0,
                &[],
                0,
                20,
                rustain::domain::models::PermissionMode::Normal,
                None,
                0,
                false,
                None,
                false,
                true,
                0,
                None,
                0,
                None,
                None,
                None,
                false,
                None,
                None,
                rustain::domain::models::visual::DensityMode::Focus,
                false,
                None,
                view.display,
            );
        })
        .unwrap();
    let row: String = (0..80)
        .map(|x| terminal.backend().buffer().cell((x, 0)).unwrap().symbol())
        .collect();
    assert!(
        row.trim_end().ends_with(&format!("log: {}", before + 1)),
        "{row}"
    );

    cancel.cancel();
    http.await.expect("server task").expect("server shutdown");
}
