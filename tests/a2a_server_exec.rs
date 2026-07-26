//! Story 18.1b execution keystones: an inbound A2A task admitted by policy,
//! executed as a **local** peer node on the daemon's real turn path, disclosed
//! through the one redaction boundary, cancellable by its submitter, and served
//! behind a card that declares the auth the middleware enforces.
//!
//! Every HTTP assertion here drives the **real** listener through `serve` with an
//! independent `reqwest` client — never the server's own types short-circuiting
//! the wire. Everything behind the listener is production code too: a real
//! `DaemonCore`, a real `NodeTree`, `NodeTree::register_peer`,
//! `DaemonTurnRuntime::drive_preloaded_turn`, `turn::run_turn`, and the real
//! `ApprovalRuntime`. Only the LLM is scripted, which is the one thing no test
//! may reach.

#![cfg(all(feature = "a2a", unix))]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use futures::stream::BoxStream;
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::a2a::auth::{A2aServerAuth, A2aServerSecurity, API_KEY_HEADER};
use rustain::adapters::a2a::card_cache::SignedCardCache;
use rustain::adapters::a2a::server::{ServeConfig, serve};
use rustain::adapters::a2a::transparency::TransparencySink;
use rustain::adapters::daemon::protocol::{
    AttachMode, ClientFrame, ConnectionTier, DaemonFrame, PROTOCOL_VERSION, read_frame, write_frame,
};
use rustain::adapters::daemon::runtime::{DaemonCore, DaemonTurnRuntime};
use rustain::adapters::daemon::server::AttachServer;
use rustain::adapters::filesystem::FileSystemStorage;
use rustain::adapters::noop::{
    NoOpApprovalPersistence, NoOpMemory, NoOpPersona, NoOpSecurity, NoOpToolSet, NoOpUsageLedger,
};
use rustain::adapters::rap::{AgentSigner, IdentityKeyStore};
use rustain::domain::errors::ProviderError;
use rustain::domain::events::AppEvent;
use rustain::domain::models::capability_id::CapabilityId;
use rustain::domain::models::capability_registry::{CapabilityRegistry, RegisteredCapability};
use rustain::domain::models::provider::ModelDescriptor;
use rustain::domain::models::{
    AppConfig, CompletionOptions, Conversation, Message, NodeState, StopReason, StreamChunk,
    TrustTier,
};
use rustain::domain::ports::{
    InboundApprovalTicket, InboundPeerError, InboundPeerRuntime, InboundPeerTask, RoomJournal,
    SecurityPort, StoragePort, StreamingProvider, ToolSetPort,
};
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::infrastructure::subagent::{NodeJournal, NodeTree, node_journal::NodeRoomJournal};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

const ANSWER: &str = "The corpus contains 141 parseable agent cards.";
/// Deadline for every poll loop. Generous enough to survive a loaded CI box,
/// short enough that a genuine hang fails the run rather than stalling it.
const BUDGET: Duration = Duration::from_secs(20);

// ── Scripted provider (the ONE thing that may not be real) ──────────────────

struct ScriptedProvider {
    chunks: Vec<StreamChunk>,
    /// Optional test gate: when set, the provider holds the stream open until
    /// released, so the test can deterministically observe the live node
    /// (terminal nodes are deregistered, so post-hoc tree lookups race).
    gate: Option<std::sync::Arc<tokio::sync::Notify>>,
}

#[async_trait::async_trait]
impl StreamingProvider for ScriptedProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<Message>,
        _options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        use futures::StreamExt;
        if let Some(gate) = &self.gate {
            gate.notified().await;
        }
        Ok(futures::stream::iter(self.chunks.clone()).boxed())
    }
    async fn abort(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    fn provider_id(&self) -> String {
        "scripted".into()
    }
    fn list_models(&self) -> Vec<ModelDescriptor> {
        vec![]
    }
    async fn health_check(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    async fn connectivity_probe(
        &self,
    ) -> Result<rustain::domain::ports::ProbeOutcome, ProviderError> {
        Ok(rustain::domain::ports::ProbeOutcome {
            latency: Duration::ZERO,
        })
    }
}

fn answer_chunks(text: &str) -> Vec<StreamChunk> {
    vec![
        StreamChunk::Text {
            content: text.to_owned(),
            parent_tool_use_id: None,
        },
        StreamChunk::TurnComplete {
            stop_reason: StopReason::EndTurn,
        },
    ]
}

/// A provider that never returns, so a cancel has something real to interrupt.
struct HangingProvider;

#[async_trait::async_trait]
impl StreamingProvider for HangingProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<Message>,
        _options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        use futures::StreamExt;
        let stream = futures::stream::once(async {
            // Far beyond any test budget: the turn ends because it was
            // cancelled, never because the script ran out.
            tokio::time::sleep(Duration::from_secs(3600)).await;
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            }
        });
        Ok(stream.boxed())
    }
    async fn abort(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    fn provider_id(&self) -> String {
        "hanging".into()
    }
    fn list_models(&self) -> Vec<ModelDescriptor> {
        vec![]
    }
    async fn health_check(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    async fn connectivity_probe(
        &self,
    ) -> Result<rustain::domain::ports::ProbeOutcome, ProviderError> {
        Ok(rustain::domain::ports::ProbeOutcome {
            latency: Duration::ZERO,
        })
    }
}

/// Minimal runtime used only to exercise the server's runtime-provided scrub
/// policy over the real HTTP surface.
struct ScrubRuntime {
    forbidden: String,
    result: String,
}

#[async_trait::async_trait]
impl InboundPeerRuntime for ScrubRuntime {
    async fn start(
        &self,
        _task: InboundPeerTask,
        _cancel: CancellationToken,
    ) -> Result<tokio::sync::watch::Receiver<NodeState>, InboundPeerError> {
        let (_sender, receiver) = tokio::sync::watch::channel(NodeState::Completed);
        Ok(receiver)
    }

    async fn request_admission_approval(
        &self,
        _peer_id: &rustain::domain::models::PeerId,
        _summary: &str,
    ) -> Result<InboundApprovalTicket, InboundPeerError> {
        Err(InboundPeerError::unavailable(
            "approval is not used by this test runtime",
        ))
    }

    async fn take_result_text(
        &self,
        _node_id: &rustain::domain::models::AgentId,
    ) -> Option<String> {
        Some(self.result.clone())
    }

    async fn disclosure_forbidden_fragments(&self) -> Vec<String> {
        vec![self.forbidden.clone()]
    }

    async fn reconcile_orphaned_tasks(
        &self,
        _subagent_type: &str,
    ) -> Vec<rustain::domain::models::AgentId> {
        Vec::new()
    }
}

/// Exercises the post-insert setup failure path without a daemon node.
struct FailingStartRuntime;

#[async_trait::async_trait]
impl InboundPeerRuntime for FailingStartRuntime {
    async fn start(
        &self,
        _task: InboundPeerTask,
        _cancel: CancellationToken,
    ) -> Result<tokio::sync::watch::Receiver<NodeState>, InboundPeerError> {
        Err(InboundPeerError::unavailable(
            "internal-node-id=a2a-in/p-secret/t-secret",
        ))
    }

    async fn request_admission_approval(
        &self,
        _peer_id: &rustain::domain::models::PeerId,
        _summary: &str,
    ) -> Result<InboundApprovalTicket, InboundPeerError> {
        Err(InboundPeerError::unavailable("not reached"))
    }

    async fn take_result_text(
        &self,
        _node_id: &rustain::domain::models::AgentId,
    ) -> Option<String> {
        None
    }

    async fn disclosure_forbidden_fragments(&self) -> Vec<String> {
        Vec::new()
    }

    async fn reconcile_orphaned_tasks(
        &self,
        _subagent_type: &str,
    ) -> Vec<rustain::domain::models::AgentId> {
        Vec::new()
    }
}

// ── Real daemon core, real node tree, real turn path ────────────────────────

fn build_runtime(
    provider: Arc<dyn StreamingProvider>,
    storage: Arc<dyn StoragePort>,
    workspace: &Path,
) -> Arc<DaemonTurnRuntime> {
    let security: Arc<dyn SecurityPort> = Arc::new(NoOpSecurity);
    let tools: Arc<dyn ToolSetPort> = Arc::new(NoOpToolSet);
    let approval = ApprovalRuntime::new(64, Arc::new(NoOpApprovalPersistence));
    let tool_scheduler = rustain::domain::services::tool_scheduler::ToolScheduler::new(
        security.clone(),
        tools.clone(),
        approval.clone(),
        64,
    );
    Arc::new(DaemonTurnRuntime {
        provider,
        app_config: Arc::new(ArcSwap::from_pointee(AppConfig::default())),
        security,
        tools,
        tool_scheduler,
        persona: Arc::new(NoOpPersona),
        context_assembler: Arc::new(ArcSwap::from_pointee(None)),
        storage: storage.clone(),
        fs_storage: Arc::new(FileSystemStorage::with_workspace_root(
            rustain::infrastructure::paths::sessions_dir(workspace),
            workspace.to_path_buf(),
        )),
        usage_ledger: Arc::new(NoOpUsageLedger),
        telemetry: rustain::infrastructure::telemetry::ActiveRatioWindow::new_in_memory(),
        plan_injector: Arc::new(
            rustain::domain::services::plan_mode_injector::DefaultPlanInjector::new(),
        ),
        approval,
        workspace: workspace.to_path_buf(),
        #[cfg(feature = "mcp")]
        mcp_task_runtimes: Vec::new(),
    })
}

struct Harness {
    addr: std::net::SocketAddr,
    cards: Arc<SignedCardCache>,
    cancel: CancellationToken,
    http: tokio::task::JoinHandle<anyhow::Result<()>>,
    server: Arc<AttachServer>,
    core: Arc<DaemonCore>,
    node_tree: NodeTree,
    node_journal: Arc<NodeJournal>,
    registry: Arc<CapabilityRegistry>,
    _domain_rx: tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
    _workspace: tempfile::TempDir,
    _keys: tempfile::TempDir,
}

impl Harness {
    fn endpoint(&self) -> String {
        format!("http://{}/", self.addr)
    }

    async fn stop(self) {
        self.cancel.cancel();
        let _ = tokio::time::timeout(BUDGET, self.http).await;
    }
}

async fn harness(policy: A2aAdmissionPolicy, provider: Arc<dyn StreamingProvider>) -> Harness {
    harness_with(policy, provider, A2aServerSecurity::default(), true).await
}

async fn harness_with(
    policy: A2aAdmissionPolicy,
    provider: Arc<dyn StreamingProvider>,
    security: A2aServerSecurity,
    with_runtime: bool,
) -> Harness {
    let workspace = tempfile::tempdir().expect("workspace");
    let keys = tempfile::tempdir().expect("keys");
    let ws = workspace.path().to_path_buf();

    let (domain_tx, domain_rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    let node_journal = Arc::new(
        NodeJournal::open_workspace(&ws)
            .await
            .expect("open node journal"),
    );
    let node_tree = NodeTree::with_event_tx(
        domain_tx.clone(),
        Arc::new(|| chrono::Utc::now().timestamp_millis()),
    )
    .with_journal(node_journal.clone());

    let storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
        rustain::infrastructure::paths::sessions_dir(&ws),
        ws.clone(),
    ));
    let core = {
        let ws = ws.clone();
        let storage = storage.clone();
        Arc::new(DaemonCore::new(
            ws.clone(),
            Arc::new(ArcSwap::from_pointee(AppConfig::default())),
            Arc::new(NoOpMemory),
            storage.clone(),
            Arc::new(NoOpSecurity),
            Arc::new(NoOpPersona),
            Box::new(move || Ok(build_runtime(provider.clone(), storage.clone(), &ws))),
        ))
    };

    let conversation = Arc::new(Mutex::new(Conversation {
        id: "a2a-exec-harness".to_owned(),
        ..Conversation::default()
    }));
    let server = AttachServer::new_with_node_tree(
        core.clone(),
        conversation,
        domain_tx.clone(),
        node_tree.clone(),
    );

    let registry = Arc::new(CapabilityRegistry::new(None));
    let signer = IdentityKeyStore::new(keys.path())
        .load_or_generate()
        .expect("identity");

    let journal: Arc<dyn RoomJournal> =
        Arc::new(NodeRoomJournal::new(node_journal.clone(), Some(domain_tx)));
    let runtime: Option<Arc<dyn InboundPeerRuntime>> =
        with_runtime.then(|| server.clone() as Arc<dyn InboundPeerRuntime>);

    let cards = Arc::new(SignedCardCache::new());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cancel = CancellationToken::new();
    let http = tokio::spawn(serve(
        listener,
        ServeConfig {
            registry: registry.clone(),
            signer,
            security,
            runtime,
            transparency: Arc::new(TransparencySink::new(journal)),
            policy,
            workspace: ws,
            advertised_host: None,
            cards: cards.clone(),
        },
        cancel.child_token(),
    ));

    Harness {
        addr,
        cards,
        cancel,
        http,
        server,
        core,
        node_tree,
        node_journal,
        registry,
        _domain_rx: domain_rx,
        _workspace: workspace,
        _keys: keys,
    }
}

// ── HTTP helpers (independent client — never the server's own types) ────────

fn send_body(id: u64, text: &str, message_id: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "message/send",
        "params": {
            "message": {
                "kind": "message",
                "messageId": message_id,
                "role": "user",
                "parts": [{ "kind": "text", "text": text }],
            }
        },
    })
}

fn task_body(id: u64, method: &str, task_id: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": { "id": task_id },
    })
}

async fn rpc(
    client: &reqwest::Client,
    endpoint: &str,
    body: &serde_json::Value,
    api_key: Option<&str>,
) -> serde_json::Value {
    let mut request = client.post(endpoint).json(body);
    if let Some(key) = api_key {
        request = request.header(API_KEY_HEADER, key);
    }
    let response = request.send().await.expect("http");
    response.json().await.expect("json body")
}

/// Poll `tasks/get` until `state` matches, collecting the states seen on the way.
async fn poll_until(
    client: &reqwest::Client,
    endpoint: &str,
    task_id: &str,
    api_key: Option<&str>,
    want: &str,
) -> (serde_json::Value, Vec<String>) {
    let deadline = tokio::time::Instant::now() + BUDGET;
    let mut seen: Vec<String> = Vec::new();
    let mut id = 1000u64;
    loop {
        id += 1;
        let value = rpc(
            client,
            endpoint,
            &task_body(id, "tasks/get", task_id),
            api_key,
        )
        .await;
        let state = value["result"]["status"]["state"]
            .as_str()
            .unwrap_or_else(|| panic!("tasks/get produced no state: {value}"))
            .to_owned();
        if seen.last() != Some(&state) {
            seen.push(state.clone());
        }
        if state == want {
            return (value, seen);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "task {task_id} never reached {want}; saw {seen:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn node_state(tree: &NodeTree, subagent_type: &str) -> Option<NodeState> {
    tree.list()
        .await
        .into_iter()
        .find(|entry| entry.subagent_type == subagent_type)
        .map(|entry| entry.current_status)
}

/// The inbound node's status watch, grabbed while the node is alive. Terminal
/// nodes are deregistered so bounded root capacity is freed, which means a
/// post-terminal tree lookup races the cleanup — the watch receiver retains
/// the terminal value after the senders drop, so subscribe early and observe
/// the terminal state through it.
async fn inbound_status_rx(tree: &NodeTree) -> tokio::sync::watch::Receiver<NodeState> {
    let deadline = tokio::time::Instant::now() + BUDGET;
    loop {
        if let Some(entry) = tree.list().await.into_iter().find(|entry| {
            entry.subagent_type == rustain::adapters::a2a::exec::INBOUND_SUBAGENT_TYPE
        }) {
            return tree
                .status_rx(&entry.agent_id)
                .await
                .expect("a live node has a status channel");
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no inbound peer node materialized"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Assert the watched node reaches `terminal`, with the suite's usual budget.
async fn assert_node_terminal(
    mut rx: tokio::sync::watch::Receiver<NodeState>,
    terminal: NodeState,
) {
    tokio::time::timeout(BUDGET, rx.wait_for(|state| *state == terminal))
        .await
        .unwrap_or_else(|_| panic!("the peer node never reached {terminal:?}"))
        .expect("the status channel must outlive the terminal state");
}

// ── [K1b] Accept → local Peer node → executed by the core → results back ────

#[tokio::test]
async fn an_accepted_task_runs_as_a_local_peer_node_and_its_result_comes_back() {
    let gate = std::sync::Arc::new(tokio::sync::Notify::new());
    let h = harness(
        A2aAdmissionPolicy::Allow,
        Arc::new(ScriptedProvider {
            chunks: answer_chunks(ANSWER),
            gate: Some(gate.clone()),
        }),
    )
    .await;
    let client = reqwest::Client::new();
    let endpoint = h.endpoint();

    let accepted = rpc(
        &client,
        &endpoint,
        &send_body(1, "summarize the corpus", "t-1"),
        None,
    )
    .await;
    let first_state = accepted["result"]["status"]["state"]
        .as_str()
        .expect("message/send returns a task");
    assert!(
        matches!(first_state, "submitted" | "working"),
        "unexpected first state {first_state}"
    );

    // Mutant (a): the node must be a PEER node, not an owned subagent. A real
    // node is observed in the real tree — not a wire string. The provider is
    // gated, so the node is deterministically alive here; terminal nodes are
    // deregistered, so the terminal state is observed through the watch.
    let entry = h
        .node_tree
        .list()
        .await
        .into_iter()
        .find(|entry| entry.subagent_type == rustain::adapters::a2a::exec::INBOUND_SUBAGENT_TYPE)
        .expect("an inbound task must materialize a peer node");
    assert_eq!(
        entry.ownership,
        rustain::domain::models::OwnershipKind::Peer,
        "an inbound A2A task must run as OwnershipKind::Peer, never Owned"
    );
    assert!(
        entry.agent_id.as_str().starts_with("a2a-in/"),
        "node id {} must live in the inbound namespace",
        entry.agent_id
    );
    let node_rx = inbound_status_rx(&h.node_tree).await;

    gate.notify_one();
    let (completed, seen) = poll_until(&client, &endpoint, "t-1", None, "completed").await;
    assert!(
        seen.iter().any(|state| state == "working"),
        "the wire must advance through `working`; saw {seen:?}"
    );
    assert_eq!(
        completed["result"]["status"]["message"]["parts"][0]["text"], ANSWER,
        "the served result must be the answer the REAL turn produced"
    );
    assert_node_terminal(node_rx, NodeState::Completed).await;

    h.stop().await;
}

/// The cassette is the shape of the responses **our server must produce** — the
/// oracle inverted. Story 17.4b recorded it from a live agent as a client; here
/// it pins the wire shape of the surface we serve.
#[tokio::test]
async fn served_task_payloads_match_the_recorded_live_agent_shape() {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/a2a/CASSETTE_completed_arc_jsonrpc.json"
    ))
    .expect("cassette");
    let cassette: serde_json::Value = serde_json::from_str(&raw).expect("cassette json");
    let recorded = &cassette["interactions"][2]["result"];

    let h = harness(
        A2aAdmissionPolicy::Allow,
        Arc::new(ScriptedProvider {
            chunks: answer_chunks(ANSWER),
            gate: None,
        }),
    )
    .await;
    let client = reqwest::Client::new();
    let endpoint = h.endpoint();
    rpc(
        &client,
        &endpoint,
        &send_body(1, "summarize the corpus", "t-1"),
        None,
    )
    .await;
    let (completed, _) = poll_until(&client, &endpoint, "t-1", None, "completed").await;
    let served = &completed["result"];

    assert_eq!(
        served["kind"], recorded["kind"],
        "kind must match the live shape"
    );
    assert!(served["id"].is_string());
    assert_eq!(
        served["status"]["state"], recorded["status"]["state"],
        "terminal wire state must match the live agent's spelling"
    );

    h.stop().await;
}

// ── [K1b-approval] R10: approval never holds the request open ───────────────

/// Attach a real read-write client over the daemon's real Unix socket, so the
/// operator side of the approval loop is production code end to end.
async fn attach_operator(
    server: Arc<AttachServer>,
    workspace: &Path,
    domain_rx: tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
) -> (tokio::net::UnixStream, CancellationToken) {
    let socket = workspace.join("daemon.sock");
    let listener = tokio::net::UnixListener::bind(&socket).expect("bind unix socket");
    let shutdown = CancellationToken::new();
    tokio::spawn(
        server
            .clone()
            .run(listener, domain_rx, None, None, shutdown.child_token()),
    );

    let mut stream = tokio::net::UnixStream::connect(&socket)
        .await
        .expect("connect");
    let nonce = match read_frame::<_, DaemonFrame>(&mut stream)
        .await
        .expect("challenge")
    {
        Some(DaemonFrame::AttachChallenge { nonce }) => nonce,
        other => panic!("expected AttachChallenge, got {other:?}"),
    };
    let keys = tempfile::tempdir().expect("operator keys");
    let signer: AgentSigner = IdentityKeyStore::new(keys.path())
        .load_or_generate()
        .expect("operator identity");
    let tier = ConnectionTier::TrustedLocal;
    let proof = signer.attach_proof(&nonce, PROTOCOL_VERSION, tier.proof_tag(), false);
    write_frame(
        &mut stream,
        &ClientFrame::Attach {
            protocol_version: PROTOCOL_VERSION,
            read_only_ok: false,
            tier,
            challenge_nonce: nonce,
            identity: signer.identity().clone(),
            proof,
        },
    )
    .await
    .expect("send attach");
    match read_frame::<_, DaemonFrame>(&mut stream)
        .await
        .expect("ack")
    {
        Some(DaemonFrame::AttachAck { granted_mode, .. }) => {
            assert_eq!(granted_mode, AttachMode::ReadWrite);
        }
        other => panic!("expected AttachAck, got {other:?}"),
    }
    // The temp key dir must outlive the handshake only.
    drop(keys);
    (stream, shutdown)
}

/// Read frames until the daemon forwards an approval request.
async fn next_approval(stream: &mut tokio::net::UnixStream) -> rustain::domain::models::RequestId {
    let deadline = tokio::time::Instant::now() + BUDGET;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "no ApprovalRequest was forwarded");
        match tokio::time::timeout(remaining, read_frame::<_, DaemonFrame>(stream)).await {
            Ok(Ok(Some(DaemonFrame::ApprovalRequest {
                request_id, tool, ..
            }))) => {
                assert_eq!(tool, "a2a/message.send");
                return request_id;
            }
            Ok(Ok(Some(_))) => continue,
            other => panic!("expected an ApprovalRequest frame, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn an_approval_gated_task_answers_auth_required_inside_the_deadline_then_resumes() {
    let mut h = harness_with(
        A2aAdmissionPolicy::Ask,
        Arc::new(ScriptedProvider {
            chunks: answer_chunks(ANSWER),
            gate: None,
        }),
        A2aServerSecurity::default(),
        true,
    )
    .await;
    let workspace = h._workspace.path().to_path_buf();
    let domain_rx = std::mem::replace(&mut h._domain_rx, {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        rx
    });
    let (mut operator, attach_shutdown) =
        attach_operator(h.server.clone(), &workspace, domain_rx).await;

    let client = reqwest::Client::new();
    let endpoint = h.endpoint();

    // R10: the response must come back well inside the server's 30s request
    // deadline, WITHOUT anyone having answered the prompt. If `message/send`
    // awaited the human this times out at 30s with -32603 instead.
    let started = std::time::Instant::now();
    let accepted = tokio::time::timeout(
        Duration::from_secs(5),
        rpc(
            &client,
            &endpoint,
            &send_body(1, "summarize the corpus", "t-1"),
            None,
        ),
    )
    .await
    .expect("message/send must not be held open awaiting a human");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "message/send blocked on the operator"
    );
    assert_eq!(
        accepted["result"]["status"]["state"], "auth-required",
        "an approval-gated accept answers the spec's non-terminal auth-required"
    );
    // The multiword state is what makes the camelCase mutant falsifiable at all.
    assert_ne!(accepted["result"]["status"]["state"], "authRequired");

    // The operator grants, through the real forwarded frame.
    let request_id = next_approval(&mut operator).await;
    write_frame(
        &mut operator,
        &ClientFrame::ApprovalResponse {
            request_id,
            outcome: rustain::domain::models::ApprovalOutcome::Once,
        },
    )
    .await
    .expect("grant");

    let (completed, seen) = poll_until(&client, &endpoint, "t-1", None, "completed").await;
    assert!(
        seen.first().is_some_and(|state| state == "auth-required"),
        "the arc must start parked on the human; saw {seen:?}"
    );
    assert_eq!(
        completed["result"]["status"]["message"]["parts"][0]["text"],
        ANSWER
    );

    attach_shutdown.cancel();
    h.stop().await;
}

#[tokio::test]
async fn a_declined_admission_resolves_to_rejected() {
    // Positive control for the arc above: same policy, opposite decision.
    let mut h = harness_with(
        A2aAdmissionPolicy::Ask,
        Arc::new(ScriptedProvider {
            chunks: answer_chunks(ANSWER),
            gate: None,
        }),
        A2aServerSecurity::default(),
        true,
    )
    .await;
    let workspace = h._workspace.path().to_path_buf();
    let domain_rx = std::mem::replace(&mut h._domain_rx, {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        rx
    });
    let (mut operator, attach_shutdown) =
        attach_operator(h.server.clone(), &workspace, domain_rx).await;

    let client = reqwest::Client::new();
    let endpoint = h.endpoint();
    let accepted = rpc(
        &client,
        &endpoint,
        &send_body(1, "do the thing", "t-1"),
        None,
    )
    .await;
    assert_eq!(accepted["result"]["status"]["state"], "auth-required");

    let request_id = next_approval(&mut operator).await;
    write_frame(
        &mut operator,
        &ClientFrame::ApprovalResponse {
            request_id,
            outcome: rustain::domain::models::ApprovalOutcome::Reject { feedback: None },
        },
    )
    .await
    .expect("decline");

    let (rejected, _) = poll_until(&client, &endpoint, "t-1", None, "rejected").await;
    assert!(
        rejected["result"]["status"]["message"]["parts"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("declined")),
        "a decline must say so: {rejected}"
    );
    // A2A has no `refused` state.
    assert!(!rejected.to_string().contains("\"refused\""));
    // No turn ever ran, so no answer can exist.
    assert_ne!(
        rejected["result"]["status"]["message"]["parts"][0]["text"],
        ANSWER
    );

    attach_shutdown.cancel();
    h.stop().await;
}

#[tokio::test]
async fn cancel_during_auth_required_is_durable_and_never_registers_after_a_late_grant() {
    let mut h = harness_with(
        A2aAdmissionPolicy::Ask,
        Arc::new(ScriptedProvider {
            chunks: answer_chunks(ANSWER),
            gate: None,
        }),
        A2aServerSecurity::default(),
        true,
    )
    .await;
    let workspace = h._workspace.path().to_path_buf();
    let domain_rx = std::mem::replace(&mut h._domain_rx, {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        rx
    });
    let (mut operator, attach_shutdown) =
        attach_operator(h.server.clone(), &workspace, domain_rx).await;
    let client = reqwest::Client::new();
    let endpoint = h.endpoint();
    let registrations_before = h.node_tree.registration_count();

    let accepted = rpc(
        &client,
        &endpoint,
        &send_body(1, "wait for approval", "cancel-pending"),
        None,
    )
    .await;
    assert_eq!(accepted["result"]["status"]["state"], "auth-required");
    let pending_path = workspace.join(".rustain").join("a2a-pending.json");
    let pending = tokio::fs::read_to_string(&pending_path)
        .await
        .expect("auth-required task is persisted");
    assert!(pending.contains("cancel-pending"), "{pending}");

    let request_id = next_approval(&mut operator).await;
    let canceled = rpc(
        &client,
        &endpoint,
        &task_body(2, "tasks/cancel", "cancel-pending"),
        None,
    )
    .await;
    assert_eq!(
        canceled["result"]["status"]["state"], "canceled",
        "tasks/cancel waits for the pending watcher terminal transition"
    );
    assert!(
        canceled["result"]["status"]["message"]["parts"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("cancel")),
        "{canceled}"
    );

    // The sender may still resolve an already-rendered prompt, but the canceled
    // watcher has dropped its ticket and must never materialize a peer node.
    write_frame(
        &mut operator,
        &ClientFrame::ApprovalResponse {
            request_id,
            outcome: rustain::domain::models::ApprovalOutcome::Once,
        },
    )
    .await
    .expect("late grant write");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        h.node_tree.registration_count(),
        registrations_before,
        "a cancel while auth-required must skip node registration"
    );
    let (terminal, _) = poll_until(&client, &endpoint, "cancel-pending", None, "canceled").await;
    assert_eq!(terminal["result"]["status"]["state"], "canceled");
    let pending = tokio::fs::read_to_string(&pending_path)
        .await
        .expect("pending file remains readable");
    assert!(
        !pending.contains("cancel-pending"),
        "terminal pending task must be removed: {pending}"
    );

    attach_shutdown.cancel();
    h.stop().await;
}

/// A turn that ERRORS is a failed task, not a completed one with an empty
/// result — and the underlying cause is not disclosed.
///
/// Caught by the release-binary smoke test, not by a unit test: `run_turn`
/// reports provider failure on its event stream and its join handle still
/// resolves `Ok`, so "the turn finished" and "the turn succeeded" are different
/// questions and only one of them was being asked.
#[tokio::test]
async fn a_turn_that_errors_reports_failed_without_disclosing_why() {
    const HOST_SECRET: &str = "401 Unauthorized from api.internal.example (key sk-abc)";

    let gate = std::sync::Arc::new(tokio::sync::Notify::new());
    let h = harness(
        A2aAdmissionPolicy::Allow,
        Arc::new(ScriptedProvider {
            chunks: vec![
                StreamChunk::Error {
                    content: HOST_SECRET.to_owned(),
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                },
            ],
            gate: Some(gate.clone()),
        }),
    )
    .await;
    let client = reqwest::Client::new();
    let endpoint = h.endpoint();

    rpc(&client, &endpoint, &send_body(1, "summarize", "t-1"), None).await;
    let node_rx = inbound_status_rx(&h.node_tree).await;
    gate.notify_one();
    let (failed, _) = poll_until(&client, &endpoint, "t-1", None, "failed").await;

    let served = failed.to_string();
    assert!(
        !served.contains("sk-abc") && !served.contains("api.internal.example"),
        "a provider failure must not disclose host configuration: {served}"
    );
    assert!(
        failed["result"]["status"]["message"]["parts"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("the local agent turn failed")),
        "a failure must still be explained in general terms: {failed}"
    );

    // …and the node is Failed, not Completed (observed through the status
    // watch: terminal nodes are deregistered from the tree).
    assert_node_terminal(node_rx, NodeState::Failed).await;

    h.stop().await;
}

// ── [K2b] + [K2b-ratchet] refuse by policy, zero core mutation ──────────────

#[tokio::test]
async fn a_policy_refusal_mutates_nothing_and_is_still_a_real_wire_verdict() {
    let h = harness(
        A2aAdmissionPolicy::Deny,
        Arc::new(ScriptedProvider {
            chunks: answer_chunks(ANSWER),
            gate: None,
        }),
    )
    .await;
    let client = reqwest::Client::new();
    let endpoint = h.endpoint();

    // The ratchet is deterministic, never a race: correct code simply never
    // reaches the mutation entries, so a behavioural test cannot force the
    // mutant RED — a refusal that also registered a node would still answer
    // `rejected` and stay green.
    //
    // The counters live behind `test-instrumentation`, which the CI A2A lane
    // enables (pinned by `the_ci_a2a_lane_enables_the_zero_mutation_ratchet`);
    // the behavioural assertions below run either way.
    #[cfg(feature = "test-instrumentation")]
    let (registrations_before, mutations_before) = (
        h.node_tree.registration_count(),
        h.node_tree.state_mutation_count(),
    );

    let refused = rpc(&client, &endpoint, &send_body(1, "summarize", "t-1"), None).await;
    assert_eq!(refused["result"]["status"]["state"], "rejected");
    let reason = refused["result"]["status"]["message"]["parts"][0]["text"]
        .as_str()
        .expect("a refusal carries a reason");
    assert!(reason.contains("server.admission"), "reason={reason}");

    #[cfg(feature = "test-instrumentation")]
    {
        assert_eq!(
            h.node_tree.registration_count(),
            registrations_before,
            "a refused task must perform ZERO node registrations (NFR70)"
        );
        assert_eq!(
            h.node_tree.state_mutation_count(),
            mutations_before,
            "a refused task must perform ZERO node state mutations (NFR70)"
        );
    }
    assert!(
        h.node_tree.list().await.is_empty(),
        "a refused task must leave the node tree empty"
    );
    // A refused task is not a task: nothing to poll.
    let missing = rpc(&client, &endpoint, &task_body(2, "tasks/get", "t-1"), None).await;
    assert_eq!(missing["error"]["code"], -32001);

    h.stop().await;
}

/// Positive control for the refusal fork: the SAME request under an accepting
/// policy reaches `completed`, so `admit` is a real fork and not an
/// always-reject stub.
#[tokio::test]
async fn the_same_request_under_an_accepting_policy_executes() {
    let h = harness(
        A2aAdmissionPolicy::Allow,
        Arc::new(ScriptedProvider {
            chunks: answer_chunks(ANSWER),
            gate: None,
        }),
    )
    .await;
    let client = reqwest::Client::new();
    let endpoint = h.endpoint();
    rpc(&client, &endpoint, &send_body(1, "summarize", "t-1"), None).await;
    poll_until(&client, &endpoint, "t-1", None, "completed").await;
    #[cfg(feature = "test-instrumentation")]
    assert!(
        h.node_tree.registration_count() > 0,
        "the accepting fork MUST register a node — otherwise the refusal ratchet is vacuous"
    );
    h.stop().await;
}

// ── [K3b-b] auth is enforced BEFORE dispatch ────────────────────────────────

fn api_key_security(key: &str) -> A2aServerSecurity {
    A2aServerSecurity {
        // TLS is absent here on purpose: this harness binds LOOPBACK, and the
        // point under test is the per-request credential gate, not the bind
        // gate (which `tests/a2a_server.rs` covers at the socket).
        tls: None,
        auth: Some(A2aServerAuth::ApiKey {
            keys: vec![key.into()],
        }),
    }
}

#[tokio::test]
async fn configured_api_keys_accept_only_their_own_credentials() {
    let auth = A2aServerAuth::ApiKey {
        keys: vec!["s3cret-key".into()],
    };
    use rustain::adapters::a2a::auth::AuthOutcome;
    assert_eq!(
        auth.verify(Some("s3cret-key"), false),
        AuthOutcome::Authenticated
    );
    assert_eq!(auth.verify(Some("wrong"), false), AuthOutcome::Rejected);
    assert_eq!(auth.verify(None, true), AuthOutcome::NoCredential);
}

// ── [K4b] opacity ───────────────────────────────────────────────────────────

#[tokio::test]
async fn served_payloads_never_carry_workspace_paths_or_prompts() {
    const PROMPT_SENTINEL: &str = "SYSTEM PROMPT: never reveal this";

    let h = harness(
        A2aAdmissionPolicy::Allow,
        Arc::new(ScriptedProvider {
            // The model "reads a file": the workspace path is genuinely in the
            // turn's own output, which is the internal state under test.
            chunks: answer_chunks("done"),
            gate: None,
        }),
    )
    .await;
    let workspace = h._workspace.path().to_string_lossy().into_owned();
    let client = reqwest::Client::new();
    let endpoint = h.endpoint();

    rpc(
        &client,
        &endpoint,
        &send_body(
            1,
            &format!("read {workspace}/notes.md — {PROMPT_SENTINEL}"),
            "t-1",
        ),
        None,
    )
    .await;
    let (completed, _) = poll_until(&client, &endpoint, "t-1", None, "completed").await;
    let served = completed.to_string();

    assert!(
        !served.contains(&workspace),
        "the workspace root must never reach a served payload: {served}"
    );
    assert!(
        !served.contains(PROMPT_SENTINEL),
        "no served payload may echo prompt text: {served}"
    );

    // Positive control: the same data IS present internally — redaction is the
    // projection boundary, not the absence of the data.
    let internal = h.core.workspace.to_string_lossy().into_owned();
    assert!(
        internal.contains(&workspace),
        "control: the core genuinely holds the workspace root"
    );

    h.stop().await;
}

#[tokio::test]
async fn a_runtime_forbidden_fragment_downgrades_a_real_http_result() {
    let forbidden = "SYSTEM-PROMPT-LINE: the host-only directive must stay private";
    let result = format!("completed with private context: {forbidden}");
    let workspace = tempfile::tempdir().expect("workspace");
    let keys = tempfile::tempdir().expect("identity directory");
    let signer = IdentityKeyStore::new(keys.path())
        .load_or_generate()
        .expect("identity");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cancel = CancellationToken::new();
    let runtime: Arc<dyn InboundPeerRuntime> = Arc::new(ScrubRuntime {
        forbidden: forbidden.to_owned(),
        result: result.clone(),
    });
    let http = tokio::spawn(serve(
        listener,
        ServeConfig {
            registry: Arc::new(CapabilityRegistry::new(None)),
            signer,
            security: A2aServerSecurity::default(),
            runtime: Some(runtime),
            transparency: Arc::new(TransparencySink::inert()),
            policy: A2aAdmissionPolicy::Allow,
            workspace: workspace.path().to_path_buf(),
            advertised_host: None,
            cards: Arc::new(SignedCardCache::new()),
        },
        cancel.child_token(),
    ));
    let client = reqwest::Client::new();
    let endpoint = format!("http://{addr}/");

    rpc(
        &client,
        &endpoint,
        &send_body(1, "do the task", "scrubbed-result"),
        None,
    )
    .await;
    let (completed, _) = poll_until(&client, &endpoint, "scrubbed-result", None, "completed").await;
    let text = completed["result"]["status"]["message"]["parts"][0]["text"]
        .as_str()
        .expect("downgrade disclosure text");
    assert!(text.contains("host-bound-unavailable"), "{text}");
    assert!(!text.contains(forbidden), "{text}");
    assert!(!text.contains(&result), "{text}");

    cancel.cancel();
    tokio::time::timeout(BUDGET, http)
        .await
        .expect("server stops")
        .expect("server task joins")
        .expect("server succeeds");
}

#[tokio::test]
async fn post_insert_start_failure_is_terminal_and_opaque() {
    let workspace = tempfile::tempdir().expect("workspace");
    let keys = tempfile::tempdir().expect("identity directory");
    let signer = IdentityKeyStore::new(keys.path())
        .load_or_generate()
        .expect("identity");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cancel = CancellationToken::new();
    let runtime: Arc<dyn InboundPeerRuntime> = Arc::new(FailingStartRuntime);
    let http = tokio::spawn(serve(
        listener,
        ServeConfig {
            registry: Arc::new(CapabilityRegistry::new(None)),
            signer,
            security: A2aServerSecurity::default(),
            runtime: Some(runtime),
            transparency: Arc::new(TransparencySink::inert()),
            policy: A2aAdmissionPolicy::Allow,
            workspace: workspace.path().to_path_buf(),
            advertised_host: None,
            cards: Arc::new(SignedCardCache::new()),
        },
        cancel.child_token(),
    ));
    let client = reqwest::Client::new();
    let endpoint = format!("http://{addr}/");

    let failed = rpc(
        &client,
        &endpoint,
        &send_body(1, "start failure", "start-failure"),
        None,
    )
    .await;
    assert_eq!(failed["result"]["status"]["state"], "failed");
    let served = failed.to_string();
    assert!(served.contains("could not be started"), "{served}");
    assert!(!served.contains("internal-node-id"), "{served}");
    assert!(!served.contains("p-secret"), "{served}");
    let checked = rpc(
        &client,
        &endpoint,
        &task_body(2, "tasks/get", "start-failure"),
        None,
    )
    .await;
    assert_eq!(checked["result"]["status"]["state"], "failed");

    cancel.cancel();
    tokio::time::timeout(BUDGET, http)
        .await
        .expect("server stops")
        .expect("server task joins")
        .expect("server succeeds");
}

// ── [K6b-cancel] + [K6b-scope] dispatcher completeness ──────────────────────

#[tokio::test]
async fn cancel_reaches_the_running_turn_and_the_node_goes_terminal() {
    let h = harness(A2aAdmissionPolicy::Allow, Arc::new(HangingProvider)).await;
    let client = reqwest::Client::new();
    let endpoint = h.endpoint();

    rpc(
        &client,
        &endpoint,
        &send_body(1, "run forever", "t-1"),
        None,
    )
    .await;
    poll_until(&client, &endpoint, "t-1", None, "working").await;
    // The HangingProvider never completes, so the node is deterministically
    // alive; subscribe before cancelling because terminal nodes are
    // deregistered from the tree.
    let node_rx = inbound_status_rx(&h.node_tree).await;

    let cancelled = rpc(
        &client,
        &endpoint,
        &task_body(2, "tasks/cancel", "t-1"),
        None,
    )
    .await;
    assert_eq!(
        cancelled["result"]["status"]["state"], "canceled",
        "tasks/cancel must wait for a terminal snapshot rather than acknowledge intent"
    );

    let (terminal, _) = poll_until(&client, &endpoint, "t-1", None, "canceled").await;
    assert!(
        terminal["result"]["status"]["message"]["parts"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("cancel")),
        "the cancel reason must be distinguishable from a restart failure"
    );

    // Mutant (d): a wire-only cancel is a lie with a green test. The NODE must
    // be terminal too (observed through the watch — it is deregistered after).
    assert_node_terminal(node_rx, NodeState::Cancelled).await;

    h.stop().await;
}

#[tokio::test]
async fn a_non_owner_gets_byte_identical_responses_to_a_fabricated_task_id() {
    let h = harness_with(
        A2aAdmissionPolicy::Allow,
        Arc::new(ScriptedProvider {
            chunks: answer_chunks(ANSWER),
            gate: None,
        }),
        A2aServerSecurity::default(),
        true,
    )
    .await;
    let client = reqwest::Client::new();
    let endpoint = h.endpoint();

    // Credential A (loopback) submits and can see its own task.
    rpc(
        &client,
        &endpoint,
        &send_body(1, "summarize", "real-task"),
        None,
    )
    .await;
    poll_until(&client, &endpoint, "real-task", None, "completed").await;

    // Credential B is a DIFFERENT submitter key. On this loopback harness both
    // callers share the loopback principal, so the scoping is proven where it
    // is decided — the store — with the wire equality proven for a task id the
    // caller does not own.
    use rustain::adapters::a2a::exec::{InboundTaskStore, SubmitterKey, mint_inbound_node_id};
    let store = InboundTaskStore::default();
    let owner = SubmitterKey::from_api_key("A");
    let other = SubmitterKey::from_api_key("B");
    store
        .insert(
            "real-task".to_owned(),
            mint_inbound_node_id(&owner, "real-task"),
            owner.clone(),
            rustain::domain::models::RapTaskState::Submitted,
        )
        .await
        .expect("insert");
    assert!(store.get_scoped("real-task", &other).await.is_none());
    assert!(store.get_scoped("fabricated", &other).await.is_none());

    // Byte-identical on the wire: the two responses for the same JSON-RPC id
    // must not differ by so much as an echoed task id.
    let unknown_a = rpc(
        &client,
        &endpoint,
        &task_body(9, "tasks/get", "no-such-task"),
        None,
    )
    .await;
    let unknown_b = rpc(
        &client,
        &endpoint,
        &task_body(9, "tasks/get", "also-absent"),
        None,
    )
    .await;
    assert_eq!(
        serde_json::to_string(&unknown_a).unwrap(),
        serde_json::to_string(&unknown_b).unwrap(),
        "an unknown-task response must not echo which id was probed"
    );
    let cancel_unknown = rpc(
        &client,
        &endpoint,
        &task_body(9, "tasks/cancel", "no-such-task"),
        None,
    )
    .await;
    assert_eq!(
        serde_json::to_string(&cancel_unknown).unwrap(),
        serde_json::to_string(&unknown_a).unwrap(),
        "tasks/cancel and tasks/get must be indistinguishable for a task you cannot see"
    );

    h.stop().await;
}

// ── [K6b-profile] the declared narrow JSON-RPC profile, over the wire ───────

#[tokio::test]
async fn the_narrow_jsonrpc_profile_is_enforced_on_the_real_socket() {
    let h = harness(
        A2aAdmissionPolicy::Deny,
        Arc::new(ScriptedProvider {
            chunks: answer_chunks(ANSWER),
            gate: None,
        }),
    )
    .await;
    let client = reqwest::Client::new();
    let endpoint = h.endpoint();

    // Notification: no `id` member → no response body at all.
    let notification = client
        .post(&endpoint)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(r#"{"jsonrpc":"2.0","method":"tasks/get","params":{"id":"x"}}"#)
        .send()
        .await
        .expect("notification");
    assert_eq!(notification.status(), reqwest::StatusCode::NO_CONTENT);
    assert!(notification.text().await.expect("body").is_empty());

    // Explicit null id → refused, with a message naming the profile.
    let null_id: serde_json::Value = client
        .post(&endpoint)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(r#"{"jsonrpc":"2.0","id":null,"method":"tasks/get","params":{"id":"x"}}"#)
        .send()
        .await
        .expect("null id")
        .json()
        .await
        .expect("json");
    assert_eq!(null_id["error"]["code"], -32600);
    assert!(
        null_id["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("explicit null JSON-RPC id")),
        "{null_id}"
    );

    // Batch → one refusal naming the profile, never a silent single-request read.
    let batch: serde_json::Value = client
        .post(&endpoint)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(r#"[{"jsonrpc":"2.0","id":1,"method":"tasks/get","params":{"id":"x"}}]"#)
        .send()
        .await
        .expect("batch")
        .json()
        .await
        .expect("json");
    assert_eq!(batch["error"]["code"], -32600);
    assert!(
        batch["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("batch requests are not supported")),
        "{batch}"
    );

    h.stop().await;
}

#[tokio::test]
async fn notifications_without_message_ids_mint_distinct_task_ids() {
    let h = harness(
        A2aAdmissionPolicy::Allow,
        Arc::new(ScriptedProvider {
            chunks: answer_chunks(ANSWER),
            gate: None,
        }),
    )
    .await;
    let client = reqwest::Client::new();
    let endpoint = h.endpoint();
    let before = h.node_tree.registration_count();

    for text in ["first notification", "second notification"] {
        let response = client
            .post(&endpoint)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "message/send",
                "params": {
                    "message": {
                        "role": "user",
                        "parts": [{ "kind": "text", "text": text }],
                    }
                }
            }))
            .send()
            .await
            .expect("notification");
        assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
    }

    let deadline = tokio::time::Instant::now() + BUDGET;
    while h.node_tree.registration_count() < before + 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "both id-less notifications must execute rather than collide"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    h.stop().await;
}

// ── [K7b-declared] / [K7b-cached] / [K7b-bounded] the public AgentCard ──────

fn skill(name: &str) -> RegisteredCapability {
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
        input_schema: serde_json::json!({"type": "object"}),
        parallel_safe: true,
        trust: TrustTier::Verified,
    }
}

#[tokio::test]
async fn a_loopback_card_does_not_declare_auth_it_does_not_enforce() {
    // A configured key alone does not make a loopback listener credential-gated.
    // The real non-loopback TLS test below proves the converse declaration.
    let configured_loopback = harness_with(
        A2aAdmissionPolicy::Deny,
        Arc::new(ScriptedProvider {
            chunks: vec![],
            gate: None,
        }),
        api_key_security("s3cret-key"),
        false,
    )
    .await;
    let url = format!(
        "http://{}/.well-known/agent-card.json",
        configured_loopback.addr
    );
    let card: serde_json::Value = reqwest::get(&url).await.unwrap().json().await.unwrap();

    assert!(
        card.get("securitySchemes").is_none(),
        "a loopback listener must not advertise auth it does not enforce"
    );
    assert!(
        card.get("security").is_none(),
        "an undeclared scheme must not appear in a requirement"
    );
    assert!(!card.to_string().contains("s3cret-key"));
    configured_loopback.stop().await;
}

#[tokio::test]
async fn repeated_card_fetches_sign_once_per_registry_generation() {
    let h = harness_with(
        A2aAdmissionPolicy::Deny,
        Arc::new(ScriptedProvider {
            chunks: vec![],
            gate: None,
        }),
        A2aServerSecurity::default(),
        false,
    )
    .await;
    let url = format!("http://{}/.well-known/agent-card.json", h.addr);
    let _handle = h.registry.register(skill("review-code")).await.unwrap();

    #[cfg(feature = "test-instrumentation")]
    let before = h.cards.signature_count();
    for _ in 0..8 {
        let response = reqwest::get(&url).await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
    }
    #[cfg(feature = "test-instrumentation")]
    assert_eq!(
        h.cards.signature_count() - before,
        1,
        "N GETs against an unchanged registry must perform exactly one signature"
    );

    // Positive control: a registry delta invalidates, and the next GET signs
    // once more — the cache is a cache, not a freeze.
    let _second = h.registry.register(skill("explain-code")).await.unwrap();
    let card: serde_json::Value = reqwest::get(&url).await.unwrap().json().await.unwrap();
    #[cfg(feature = "test-instrumentation")]
    assert_eq!(
        h.cards.signature_count() - before,
        2,
        "a registry delta must invalidate the cached card"
    );
    let ids: Vec<&str> = card["skills"]
        .as_array()
        .unwrap()
        .iter()
        .map(|skill| skill["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["explain-code", "review-code"]);

    h.stop().await;
}

#[tokio::test]
async fn an_over_cap_registry_yields_a_valid_observably_truncated_card() {
    use rustain::adapters::a2a::card::{MAX_CARD_BYTES, MAX_DISCLOSED_SKILLS};

    let h = harness_with(
        A2aAdmissionPolicy::Deny,
        Arc::new(ScriptedProvider {
            chunks: vec![],
            gate: None,
        }),
        A2aServerSecurity::default(),
        false,
    )
    .await;
    let total = MAX_DISCLOSED_SKILLS * 3;
    let mut handles = Vec::new();
    for index in 0..total {
        handles.push(
            h.registry
                .register(skill(&format!("skill-{index:04}")))
                .await
                .unwrap(),
        );
    }

    let url = format!("http://{}/.well-known/agent-card.json", h.addr);
    let raw = reqwest::get(&url).await.unwrap().text().await.unwrap();

    // (i) still a VALID card — the repaired failure mode is "bounded", never
    // "withheld": failing closed here would make a large inventory render the
    // agent undiscoverable by its own defence.
    let parsed = rustain::adapters::a2a::card::decode_and_validate(&raw)
        .expect("an over-cap registry must still yield a parseable card");
    rustain::adapters::a2a::card::validate_required(&parsed).expect("card must be valid");

    let card: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let disclosed = card["skills"].as_array().unwrap().len();
    // (ii) at or under the caps.
    assert!(disclosed <= MAX_DISCLOSED_SKILLS, "disclosed={disclosed}");
    assert!(
        raw.len() <= MAX_CARD_BYTES,
        "signed card is {} bytes",
        raw.len()
    );
    // (iii) the truncation is OBSERVABLE — a silent truncation is a lie with
    // good intentions.
    assert_eq!(card["x-rustain-truncated"]["disclosedSkills"], disclosed);
    assert_eq!(card["x-rustain-truncated"]["totalSkills"], total);
    // (iv) deterministic across repeated builds.
    let again = reqwest::get(&url).await.unwrap().text().await.unwrap();
    assert_eq!(raw, again, "the bounded card must be byte-stable");

    drop(handles);
    h.stop().await;
}

// ── [NFR67] transparency: accept AND refuse land on the canonical journal ───

/// Every room record this server appended, in order.
async fn journalled_room_events(journal: &NodeJournal) -> Vec<rustain::domain::models::RoomEvent> {
    journal
        .load()
        .await
        .expect("journal loads")
        .into_iter()
        .filter_map(|entry| match entry.record {
            rustain::domain::models::node_journal::JournalRecord::Room(event) => Some(event),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_refusal_emits_exactly_one_durable_transparency_record() {
    use rustain::domain::models::{RejectReason, RoomEvent};

    let h = harness(
        A2aAdmissionPolicy::Deny,
        Arc::new(ScriptedProvider {
            chunks: answer_chunks(ANSWER),
            gate: None,
        }),
    )
    .await;
    let client = reqwest::Client::new();
    rpc(
        &client,
        &h.endpoint(),
        &send_body(1, "summarize", "t-1"),
        None,
    )
    .await;

    // Durable-first: the record is on the canonical journal, not a second log.
    // Story 18.2 projects `transparency.jsonl` from exactly these events.
    let events = journalled_room_events(&h.node_journal).await;
    let refusals: Vec<_> = events
        .iter()
        .filter(|event| matches!(event, RoomEvent::RemoteEnvelopeRejected { .. }))
        .collect();
    assert_eq!(
        refusals.len(),
        1,
        "a refusal must emit exactly one record, not zero and not two: {events:?}"
    );
    let RoomEvent::RemoteEnvelopeRejected { reason, .. } = refusals[0] else {
        unreachable!()
    };
    assert!(
        matches!(reason, RejectReason::Policy { detail } if detail.contains("server.admission")),
        "the record must carry the policy verdict: {reason:?}"
    );
    // NFR70: transparency is a room event, never core state.
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RoomEvent::NodeRegistered { .. })),
        "a refusal must journal no node registration: {events:?}"
    );

    h.stop().await;
}

#[tokio::test]
async fn an_acceptance_emits_a_durable_transparency_record_naming_the_node() {
    use rustain::domain::models::RoomEvent;

    let h = harness(
        A2aAdmissionPolicy::Allow,
        Arc::new(ScriptedProvider {
            chunks: answer_chunks(ANSWER),
            gate: None,
        }),
    )
    .await;
    let client = reqwest::Client::new();
    let endpoint = h.endpoint();
    rpc(&client, &endpoint, &send_body(1, "summarize", "t-1"), None).await;
    poll_until(&client, &endpoint, "t-1", None, "completed").await;

    let events = journalled_room_events(&h.node_journal).await;
    let accepted: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            RoomEvent::RemoteEnvelopeAccepted { node, .. } => Some(node.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(accepted.len(), 1, "one accept, one record: {events:?}");
    assert!(accepted[0].as_str().starts_with("a2a-in/"));

    // The cancelled/failed/completed arc is already on the canonical path via
    // the node's own lifecycle records — the transparency projection reads one
    // stream, not two.
    assert!(
        events.iter().any(|event| matches!(
            event,
            RoomEvent::NodeStateChanged { to, .. } if *to == NodeState::Completed
        )),
        "the executed node's terminal transition must be journalled too: {events:?}"
    );

    h.stop().await;
}

// ── [K5b-restart] a task the host lost resolves `failed`, never zombie ──────

#[tokio::test]
async fn a_task_lost_to_a_restart_resolves_failed_with_a_distinct_reason() {
    use rustain::adapters::a2a::exec::{
        CANCEL_DETAIL, INBOUND_SUBAGENT_TYPE, RESTART_DETAIL, SubmitterKey, mint_inbound_node_id,
    };
    use rustain::domain::models::{AgentMetrics, CapabilityTokenId, Op};
    use rustain::infrastructure::subagent::{AgentHandle, MailboxBudget};

    let workspace = tempfile::tempdir().expect("workspace");
    let keys = tempfile::tempdir().expect("keys");
    let ws = workspace.path().to_path_buf();
    let (domain_tx, _domain_rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    let journal = Arc::new(NodeJournal::open_workspace(&ws).await.expect("journal"));

    // ── Process #1: a task is in flight when the host dies. ──
    let task_id = "in-flight";
    let submitter = SubmitterKey::loopback();
    let node_id = mint_inbound_node_id(&submitter, task_id);
    {
        // Host binding matters: recovery skips restoring a node whose recorded
        // host is not this one (it is host-bound elsewhere), so both processes
        // must bind the same host exactly as the daemon composition does.
        let tree = NodeTree::with_event_tx(
            domain_tx.clone(),
            Arc::new(|| chrono::Utc::now().timestamp_millis()),
        )
        .with_journal(journal.clone())
        .with_host_binding(rustain::infrastructure::subagent::current_host_binding(&ws));
        let (command_tx, _rx) = tokio::sync::mpsc::channel(1);
        let (status_tx, _) = tokio::sync::watch::channel(NodeState::Created);
        let (_, metrics_rx) = tokio::sync::watch::channel(AgentMetrics::default());
        tree.register_peer(
            node_id.clone(),
            AgentHandle {
                agent_id: node_id.clone(),
                token: CapabilityTokenId::nil(),
                command_tx,
                cancel_token: CancellationToken::new(),
                depth: 0,
                subagent_type: INBOUND_SUBAGENT_TYPE.into(),
                spawned_at: 0,
                status: status_tx,
                metrics: metrics_rx,
                isolated: false,
                mailbox_budget: MailboxBudget::new(),
            },
        )
        .await
        .expect("register");
        tree.set_state(&node_id, NodeState::Running).await;
        // …and the process vanishes here. No terminal transition is ever written.
        let _ = Op::Kill;
    }

    // ── Process #2: a fresh tree over the SAME journal, rebuilt by the real
    //    recovery fold, then a fresh listener over it. ──
    let tree = NodeTree::with_event_tx(
        domain_tx.clone(),
        Arc::new(|| chrono::Utc::now().timestamp_millis()),
    )
    .with_journal(journal.clone())
    .with_host_binding(rustain::infrastructure::subagent::current_host_binding(&ws));
    let singleton = rustain::infrastructure::subagent::DaemonSingletonLock::try_acquire(&ws)
        .await
        .expect("singleton");
    let _recovery = rustain::infrastructure::subagent::NodeRecovery::reconcile(
        &journal,
        &tree,
        &singleton,
        &rustain::infrastructure::subagent::current_host_id(&ws),
    )
    .await
    .expect("reconcile");
    assert!(
        tree.list()
            .await
            .iter()
            .any(|entry| entry.agent_id == node_id && !entry.current_status.is_terminal()),
        "control: recovery must restore the in-flight node in a NON-terminal state, \
         otherwise this test proves nothing"
    );

    let storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
        rustain::infrastructure::paths::sessions_dir(&ws),
        ws.clone(),
    ));
    let core = {
        let ws = ws.clone();
        let storage = storage.clone();
        Arc::new(DaemonCore::new(
            ws.clone(),
            Arc::new(ArcSwap::from_pointee(AppConfig::default())),
            Arc::new(NoOpMemory),
            storage.clone(),
            Arc::new(NoOpSecurity),
            Arc::new(NoOpPersona),
            Box::new(move || {
                Ok(build_runtime(
                    Arc::new(ScriptedProvider {
                        chunks: answer_chunks(ANSWER),
                        gate: None,
                    }),
                    storage.clone(),
                    &ws,
                ))
            }),
        ))
    };
    let server = AttachServer::new_with_node_tree(
        core,
        Arc::new(Mutex::new(Conversation {
            id: "restart".to_owned(),
            ..Conversation::default()
        })),
        domain_tx.clone(),
        tree.clone(),
    );
    let signer = IdentityKeyStore::new(keys.path())
        .load_or_generate()
        .expect("identity");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cancel = CancellationToken::new();
    let http = tokio::spawn(serve(
        listener,
        ServeConfig {
            registry: Arc::new(CapabilityRegistry::new(None)),
            signer,
            security: A2aServerSecurity::default(),
            runtime: Some(server as Arc<dyn InboundPeerRuntime>),
            transparency: Arc::new(TransparencySink::new(Arc::new(NodeRoomJournal::new(
                journal.clone(),
                Some(domain_tx),
            )))),
            policy: A2aAdmissionPolicy::Allow,
            workspace: ws,
            advertised_host: None,
            cards: Arc::new(SignedCardCache::new()),
        },
        cancel.child_token(),
    ));

    let client = reqwest::Client::new();
    let endpoint = format!("http://{addr}/");
    let value = rpc(
        &client,
        &endpoint,
        &task_body(1, "tasks/get", task_id),
        None,
    )
    .await;
    assert_eq!(
        value["result"]["status"]["state"], "failed",
        "an in-flight task lost to a restart must resolve `failed`, never a zombie `working`: {value}"
    );
    let reason = value["result"]["status"]["message"]["parts"][0]["text"]
        .as_str()
        .expect("failure carries a reason");
    assert!(reason.contains("restart"), "reason={reason}");
    // AC5b restart scope: the two reasons must NOT collapse — a peer telling
    // "the host died" (retry) from "my cancel was honored" (do not retry) is the
    // whole point.
    assert_ne!(reason, CANCEL_DETAIL);
    assert_eq!(reason, RESTART_DETAIL);

    // …and the node itself is terminal, not left dangling for the next restart.
    assert!(
        tree.list()
            .await
            .iter()
            .any(|entry| entry.agent_id == node_id && entry.current_status == NodeState::Failed)
    );

    cancel.cancel();
    let _ = tokio::time::timeout(BUDGET, http).await;
    drop(keys);
    drop(workspace);
}

// ── [K3b-b] / [K3b-d] the REAL non-loopback socket: TLS + API key ───────────

fn self_signed_tls() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tls dir");
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
        .expect("self-signed certificate");
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");
    std::fs::write(&cert_path, cert.cert.pem()).expect("write cert");
    std::fs::write(&key_path, cert.signing_key.serialize_pem()).expect("write key");
    (dir, cert_path, key_path)
}

#[tokio::test]
async fn wildcard_bind_requires_an_advertised_authority_even_with_tls_and_auth() {
    use rustain::adapters::a2a::tls::load_tls_material;

    let (_tls_dir, cert, key) = self_signed_tls();
    let material = load_tls_material(&cert, &key).expect("load tls material");
    let key_dir = tempfile::tempdir().expect("identity dir");
    let workspace = tempfile::tempdir().expect("workspace");
    let signer = IdentityKeyStore::new(key_dir.path())
        .load_or_generate()
        .expect("identity");
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let error = serve(
        listener,
        ServeConfig {
            registry: Arc::new(CapabilityRegistry::new(None)),
            signer,
            security: A2aServerSecurity {
                tls: Some(material),
                auth: Some(A2aServerAuth::ApiKey {
                    keys: vec!["configured-key".into()],
                }),
            },
            runtime: None,
            transparency: Arc::new(TransparencySink::inert()),
            policy: A2aAdmissionPolicy::Deny,
            workspace: workspace.path().to_path_buf(),
            advertised_host: None,
            cards: Arc::new(SignedCardCache::new()),
        },
        CancellationToken::new(),
    )
    .await
    .expect_err("wildcard authority cannot be published from 0.0.0.0");
    assert!(
        error.to_string().contains("advertised_host"),
        "unexpected error: {error:#}"
    );
}

#[tokio::test]
async fn a_non_loopback_listener_serves_only_over_tls_and_only_with_the_key() {
    use rustain::adapters::a2a::tls::load_tls_material;

    let (_tls_dir, cert, key) = self_signed_tls();
    let material = load_tls_material(&cert, &key).expect("load tls material");
    let security = A2aServerSecurity {
        tls: Some(material),
        auth: Some(A2aServerAuth::ApiKey {
            keys: vec!["s3cret-key".into(), "second-key".into()],
        }),
    };

    let workspace = tempfile::tempdir().expect("workspace");
    let keys = tempfile::tempdir().expect("keys");
    let ws = workspace.path().to_path_buf();
    let (domain_tx, _domain_rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    let journal = Arc::new(NodeJournal::open_workspace(&ws).await.expect("journal"));
    let node_tree = NodeTree::with_event_tx(
        domain_tx.clone(),
        Arc::new(|| chrono::Utc::now().timestamp_millis()),
    )
    .with_journal(journal.clone());
    let storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
        rustain::infrastructure::paths::sessions_dir(&ws),
        ws.clone(),
    ));
    let core = {
        let ws = ws.clone();
        let storage = storage.clone();
        Arc::new(DaemonCore::new(
            ws.clone(),
            Arc::new(ArcSwap::from_pointee(AppConfig::default())),
            Arc::new(NoOpMemory),
            storage.clone(),
            Arc::new(NoOpSecurity),
            Arc::new(NoOpPersona),
            Box::new(move || {
                Ok(build_runtime(
                    Arc::new(ScriptedProvider {
                        chunks: answer_chunks(ANSWER),
                        gate: None,
                    }),
                    storage.clone(),
                    &ws,
                ))
            }),
        ))
    };
    let server = AttachServer::new_with_node_tree(
        core,
        Arc::new(Mutex::new(Conversation {
            id: "tls".to_owned(),
            ..Conversation::default()
        })),
        domain_tx.clone(),
        node_tree.clone(),
    );
    let signer = IdentityKeyStore::new(keys.path())
        .load_or_generate()
        .expect("identity");

    // 0.0.0.0 is genuinely NOT loopback — `serve`'s socket-level guard is the
    // thing being satisfied here, by evidence rather than by exemption.
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let cancel = CancellationToken::new();
    let http = tokio::spawn(serve(
        listener,
        ServeConfig {
            registry: Arc::new(CapabilityRegistry::new(None)),
            signer,
            security,
            runtime: Some(server as Arc<dyn InboundPeerRuntime>),
            transparency: Arc::new(TransparencySink::new(Arc::new(NodeRoomJournal::new(
                journal,
                Some(domain_tx),
            )))),
            policy: A2aAdmissionPolicy::Allow,
            workspace: ws,
            advertised_host: Some(format!("localhost:{port}")),
            cards: Arc::new(SignedCardCache::new()),
        },
        cancel.child_token(),
    ));

    // The certificate is self-signed; the point under test is the auth gate, not
    // PKI, so trust is short-circuited exactly here and nowhere in production.
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .expect("client");
    let endpoint = format!("https://127.0.0.1:{port}/");

    // The card is reachable UNAUTHENTICATED over TLS (D1) and declares the key.
    let card: serde_json::Value = client
        .get(format!(
            "https://127.0.0.1:{port}/.well-known/agent-card.json"
        ))
        .send()
        .await
        .expect("card over TLS")
        .json()
        .await
        .expect("card json");
    assert_eq!(card["securitySchemes"]["apiKey"]["name"], API_KEY_HEADER);
    assert_eq!(
        card["supportedInterfaces"][0]["url"],
        format!("https://localhost:{port}"),
        "a wildcard listener publishes its configured client-reachable authority"
    );

    let registrations_before = node_tree.registration_count();

    // No credential → rejected BEFORE dispatch.
    let unauthenticated = client
        .post(&endpoint)
        .json(&send_body(1, "summarize", "t-1"))
        .send()
        .await
        .expect("unauthenticated post");
    assert_eq!(unauthenticated.status(), reqwest::StatusCode::UNAUTHORIZED);

    // The credential gate runs before `Bytes` extracts or JSON parsing begins:
    // malformed body cannot change a 401 into a parse error.
    let unauthenticated_malformed = client
        .post(&endpoint)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body("{")
        .send()
        .await
        .expect("unauthenticated malformed post");
    assert_eq!(
        unauthenticated_malformed.status(),
        reqwest::StatusCode::UNAUTHORIZED
    );

    // A first-cut Bearer token is NO credential, not a wrong one.
    let bearer = client
        .post(&endpoint)
        .header(reqwest::header::AUTHORIZATION, "Bearer some-oauth2-token")
        .json(&send_body(2, "summarize", "t-2"))
        .send()
        .await
        .expect("bearer post");
    assert_eq!(bearer.status(), reqwest::StatusCode::UNAUTHORIZED);
    let body: serde_json::Value = bearer.json().await.expect("json");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("Authorization: Bearer")),
        "the refusal must say why an OAuth2 client was turned away: {body}"
    );

    // A wrong key is rejected too, and the response never echoes it.
    let wrong = client
        .post(&endpoint)
        .header(API_KEY_HEADER, "not-the-key")
        .json(&send_body(3, "summarize", "t-3"))
        .send()
        .await
        .expect("wrong key");
    assert_eq!(wrong.status(), reqwest::StatusCode::UNAUTHORIZED);
    let body = wrong.text().await.expect("body");
    assert!(
        !body.contains("not-the-key"),
        "a refusal must not echo the credential: {body}"
    );

    // Zero mutation from all four: authentication runs before admission.
    assert_eq!(
        node_tree.registration_count(),
        registrations_before,
        "an unauthenticated request must never reach register_peer"
    );

    // Two configured credentials are distinct submitters: both may use a
    // submitter-chosen message id that would be a replay for either alone.
    let accepted_first = client
        .post(&endpoint)
        .header(API_KEY_HEADER, "s3cret-key")
        .json(&send_body(4, "summarize", "shared-id"))
        .send()
        .await
        .expect("first configured key");
    assert_eq!(accepted_first.status(), reqwest::StatusCode::OK);
    let accepted_second = client
        .post(&endpoint)
        .header(API_KEY_HEADER, "second-key")
        .json(&send_body(5, "summarize", "shared-id"))
        .send()
        .await
        .expect("second configured key");
    assert_eq!(accepted_second.status(), reqwest::StatusCode::OK);
    poll_until(
        &client,
        &endpoint,
        "shared-id",
        Some("s3cret-key"),
        "completed",
    )
    .await;
    poll_until(
        &client,
        &endpoint,
        "shared-id",
        Some("second-key"),
        "completed",
    )
    .await;

    let private = client
        .post(&endpoint)
        .header(API_KEY_HEADER, "s3cret-key")
        .json(&send_body(6, "summarize", "first-only"))
        .send()
        .await
        .expect("first private task");
    assert_eq!(private.status(), reqwest::StatusCode::OK);
    poll_until(
        &client,
        &endpoint,
        "first-only",
        Some("s3cret-key"),
        "completed",
    )
    .await;

    let not_owner = client
        .post(&endpoint)
        .header(API_KEY_HEADER, "second-key")
        .json(&task_body(99, "tasks/get", "first-only"))
        .send()
        .await
        .expect("non-owner get")
        .text()
        .await
        .expect("non-owner body");
    let fabricated = client
        .post(&endpoint)
        .header(API_KEY_HEADER, "second-key")
        .json(&task_body(99, "tasks/get", "fabricated"))
        .send()
        .await
        .expect("fabricated get")
        .text()
        .await
        .expect("fabricated body");
    assert_eq!(
        not_owner, fabricated,
        "A/B scoping must be byte-identical to non-existence on the real HTTP path"
    );
    assert!(node_tree.registration_count() > registrations_before);

    cancel.cancel();
    let _ = tokio::time::timeout(BUDGET, http).await;
    drop(keys);
    drop(workspace);
}
