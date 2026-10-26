//! Story 19.18b `K01` — an outbound model delegation parks on `input-required`.
//!
//! Front door: `A2aEgress::compose` → `provider().invoke` (the composite
//! toolset's call), against a `wiremock` third-party agent that serves an
//! AgentCard and scripted JSON-RPC. The kept `NodeTree` clone observes the node;
//! `runtime().parked_count()` is the Rule 4 ratchet (`test-instrumentation`,
//! enabled by the CI a2a lane). Waits are structural: the peer's request log is
//! a `watch` channel and settlement is awaited on the runtime's own
//! `MessageRefused` receipt — never a sleep.
//!
//! The shared rail/peer helpers live here and are `pub(super)` for `K02`/`K03`.
//!
//! ⛔ Forbidden bypasses, none used: `delegate`/`run_lifecycle`/`message_send`
//! called directly, hand-built parked entries, a cancelled token standing in
//! for a kill.

#![cfg(feature = "test-instrumentation")]

use std::collections::{HashMap, VecDeque};

use rustain::domain::models::{
    Authorship, CapabilityId, ContentHash, CorrelationId, DispatchAct, NodeState, RedactedUrl,
    RefuseReason, SendAuthorization, SubagentEnvelope, SubagentEvent, WaitReason,
};
use rustain::infrastructure::subagent::RegistryEntry;
use serde_json::{Value, json};
use tokio::sync::watch;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

/// The asked peer's alias and its one skill (the tool is `a2a::<peer>::clarify`).
pub(super) const PEER: &str = "clarifier";
pub(super) const OTHER: &str = "other";
pub(super) const SKILL: &str = "clarify";
/// The id a third-party agent mints for its own task, and its context.
pub(super) const TASK: &str = "task-billing-7";
pub(super) const CONTEXT: &str = "ctx-billing-7";
pub(super) const DELEGATION: &str = "migrate the billing tables";
pub(super) const QUESTION: &str = "Which database should I migrate: main or replica?";
pub(super) const ITEM_CAPABILITY: &str = "x-rustain-item-capability";
pub(super) const REPLY_CAPABILITY: &str = "x-rustain-reply-capability";
/// The body of every scripted HTTP failure; it must never reach a journal row.
pub(super) const HTTP_CANARY: &str = "PEER_HTTP_BODY_MUST_NEVER_REACH_THE_JOURNAL";
/// AC2, verbatim.
pub(super) const ANSWER_INSTRUCTIONS: &str = "The remote agent asked for input. Answer by calling this tool again with \"continue\" set to this value and \"message\" set to your answer. The remote task stays open until you answer or this node is cancelled.";
pub(super) const UNKNOWN: &str = "continuation handle is unknown or already used";
pub(super) const OTHER_PEER: &str = "continuation handle belongs to another peer";
pub(super) const NEEDS_MESSAGE: &str = "continuing a remote task needs a message";

/// How the scripted agent names its task.
#[derive(Clone, Copy, Debug)]
pub(super) enum TaskIds {
    /// A spec third-party agent: its own id ([`TASK`]).
    Minted,
    /// The task id is the first `messageId` this host sent (rustain's own
    /// shape), so the host's own capability binding applies to it.
    OfMessage,
}

/// One scripted JSON-RPC answer.
#[derive(Clone, Copy, Debug)]
pub(super) enum Answer {
    /// A task in this state, without a status message.
    State(&'static str),
    /// `input-required` with this question as `status.message` text.
    Ask(&'static str),
    /// A JSON-RPC error object.
    RpcError(i64, &'static str),
    /// A bare HTTP failure whose body is [`HTTP_CANARY`].
    Http(u16),
}

struct PeerState {
    ids: TaskIds,
    first_message: parking_lot::Mutex<Option<String>>,
    script: parking_lot::Mutex<HashMap<String, VecDeque<Answer>>>,
    log: watch::Sender<Vec<Value>>,
}

impl PeerState {
    fn task_id(&self) -> String {
        match self.ids {
            TaskIds::Minted => TASK.to_owned(),
            TaskIds::OfMessage => self
                .first_message
                .lock()
                .clone()
                .expect("the task id is the first message id; nothing was sent yet"),
        }
    }

    fn answer(&self, request: &wiremock::Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).expect("a JSON-RPC request");
        let rpc_method = body["method"].as_str().unwrap_or_default().to_owned();
        if rpc_method == "message/send" {
            let message_id = body["params"]["message"]["messageId"]
                .as_str()
                .expect("message/send carries a messageId")
                .to_owned();
            let mut first = self.first_message.lock();
            if first.is_none() {
                *first = Some(message_id);
            }
        }
        let id = body["id"].clone();
        self.log.send_modify(|log| log.push(body));
        let answer = self
            .script
            .lock()
            .get_mut(&rpc_method)
            .and_then(VecDeque::pop_front);
        let envelope = |payload: (&str, Value)| {
            let mut envelope = json!({ "jsonrpc": "2.0", "id": id });
            envelope[payload.0] = payload.1;
            ResponseTemplate::new(200).set_body_json(envelope)
        };
        match answer {
            Some(Answer::State(state)) => {
                envelope(("result", task_body(&self.task_id(), state, None)))
            }
            Some(Answer::Ask(question)) => envelope((
                "result",
                task_body(&self.task_id(), "input-required", Some(question)),
            )),
            Some(Answer::RpcError(code, message)) => {
                envelope(("error", json!({ "code": code, "message": message })))
            }
            Some(Answer::Http(status)) => {
                ResponseTemplate::new(status).set_body_string(HTTP_CANARY)
            }
            None => envelope((
                "error",
                json!({ "code": -32603, "message": format!("unscripted {rpc_method}") }),
            )),
        }
    }
}

/// The raw task object the scripted agent answers with.
pub(super) fn task_body(task: &str, state: &str, question: Option<&str>) -> Value {
    let mut status = json!({ "state": state });
    if let Some(question) = question {
        status["message"] = json!({
            "kind": "message",
            "messageId": "peer-question",
            "role": "agent",
            "parts": [{ "kind": "text", "text": question }],
        });
    }
    json!({ "kind": "task", "id": task, "contextId": CONTEXT, "status": status })
}

/// A third-party A2A agent: an AgentCard with skill [`SKILL`] and one
/// scripted JSON-RPC endpoint that records every request it receives.
pub(super) struct ScriptedPeer {
    _server: MockServer,
    pub(super) alias: &'static str,
    uri: String,
    state: Arc<PeerState>,
    log: watch::Receiver<Vec<Value>>,
}

impl ScriptedPeer {
    pub(super) async fn start(alias: &'static str, ids: TaskIds) -> Self {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.well-known/agent-card.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": alias,
                "skills": [{ "id": SKILL, "name": "Clarify", "description": "Asks before acting" }],
                "url": format!("{}/jsonrpc", server.uri()),
                "preferredTransport": "JSONRPC",
            })))
            .mount(&server)
            .await;
        let (log_tx, log) = watch::channel(Vec::new());
        let state = Arc::new(PeerState {
            ids,
            first_message: parking_lot::Mutex::new(None),
            script: parking_lot::Mutex::new(HashMap::new()),
            log: log_tx,
        });
        let responder = state.clone();
        Mock::given(method("POST"))
            .and(path("/jsonrpc"))
            .respond_with(move |request: &wiremock::Request| responder.answer(request))
            .mount(&server)
            .await;
        Self {
            uri: server.uri(),
            _server: server,
            alias,
            state,
            log,
        }
    }

    /// Queue answers for one JSON-RPC method, in order.
    pub(super) fn script(&self, rpc_method: &str, answers: impl IntoIterator<Item = Answer>) {
        self.state
            .script
            .lock()
            .entry(rpc_method.to_owned())
            .or_default()
            .extend(answers);
    }

    pub(super) fn spec(&self) -> A2aPeerSpec {
        A2aPeerSpec::new(
            self.alias,
            RedactedUrl::from(self.uri.clone()),
            A2aPeerSource::Workspace,
        )
    }

    /// The remote task id `T` this agent answers with.
    pub(super) fn task_id(&self) -> String {
        self.state.task_id()
    }

    /// Every JSON-RPC request received, in arrival order.
    pub(super) fn requests(&self) -> Vec<Value> {
        self.log.borrow().clone()
    }

    pub(super) fn methods(&self) -> Vec<String> {
        self.requests()
            .iter()
            .map(|request| request["method"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    pub(super) fn requests_for(&self, rpc_method: &str) -> Vec<Value> {
        self.requests()
            .into_iter()
            .filter(|request| request["method"].as_str() == Some(rpc_method))
            .collect()
    }

    /// Structural wait: resolve once `count` `rpc_method` requests arrived.
    pub(super) async fn await_requests(&self, rpc_method: &str, count: usize) -> Vec<Value> {
        let mut log = self.log.clone();
        tokio::time::timeout(
            SETTLE,
            log.wait_for(|log| {
                log.iter()
                    .filter(|request| request["method"].as_str() == Some(rpc_method))
                    .count()
                    >= count
            }),
        )
        .await
        .unwrap_or_else(|_| panic!("the peer never received {count} {rpc_method}"))
        .expect("the peer log stays open");
        self.requests_for(rpc_method)
    }
}

/// The model rail, composed as production composes it, keeping the tree.
pub(super) struct ModelRail {
    pub(super) egress: A2aEgress,
    pub(super) tree: NodeTree,
    pub(super) journal: Journal,
    events: tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
    _keys: tempfile::TempDir,
}

pub(super) async fn model_rail(peers: Vec<A2aPeerSpec>) -> ModelRail {
    let journal = journal().await;
    let keys = tempfile::tempdir().expect("keys");
    let tree = NodeTree::new();
    let (event_tx, events) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    let egress = A2aEgress::compose(
        peers,
        tree.clone(),
        journal.room.clone(),
        journal.node.clone(),
        event_tx,
        rustain::adapters::a2a::item_capability::load_item_capability_key(Ok(keys
            .path()
            .to_path_buf())),
    )
    .expect("compose the production egress");
    tokio::time::timeout(SETTLE, egress.await_cards_settled())
        .await
        .expect("cards settle");
    ModelRail {
        egress,
        tree,
        journal,
        events,
        _keys: keys,
    }
}

impl ModelRail {
    /// One model tool call, `a2a::<peer>::clarify`, through the provider.
    pub(super) async fn invoke(&self, peer: &str, input: Value) -> Result<String, String> {
        use rustain::domain::ports::CapabilityProvider;
        let id = CapabilityId {
            protocol: "a2a".to_owned(),
            server: peer.to_owned(),
            tool: SKILL.to_owned(),
        };
        tokio::time::timeout(
            SETTLE,
            self.egress
                .provider()
                .invoke(&id, input, CancellationToken::new()),
        )
        .await
        .expect("the model tool settles")
        .map(|result| {
            assert!(!result.is_error, "an Ok tool result is never an error");
            result.content
        })
        .map_err(|error| error.to_string())
    }

    pub(super) async fn rows(&self) -> Vec<(u64, RoomEvent)> {
        room_events(&self.journal).await
    }

    /// The single outbound peer node.
    pub(super) async fn peer_node(&self) -> RegistryEntry {
        let mut nodes = self.peer_nodes().await;
        assert_eq!(nodes.len(), 1, "exactly one delegated node: {nodes:?}");
        nodes.remove(0)
    }

    pub(super) async fn peer_nodes(&self) -> Vec<RegistryEntry> {
        self.tree
            .list()
            .await
            .into_iter()
            .filter(|entry| entry.subagent_type == "a2a-peer")
            .collect()
    }

    pub(super) fn parked(&self) -> usize {
        self.egress.runtime().parked_count()
    }

    /// Structural wait: the runtime's `MessageRefused` receipt, sent after the
    /// settlement row is durable.
    pub(super) async fn await_refused(&mut self) -> SubagentEnvelope {
        tokio::time::timeout(SETTLE, async {
            loop {
                match self
                    .events
                    .recv()
                    .await
                    .expect("the runtime keeps its sender")
                {
                    AppEvent::Subagent(envelope)
                        if matches!(envelope.event, SubagentEvent::MessageRefused { .. }) =>
                    {
                        return envelope;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("a MessageRefused receipt follows the settlement row")
    }

    /// `MessageRefused` receipts already emitted (non-blocking drain).
    pub(super) fn refused_so_far(&mut self) -> usize {
        let mut refused = 0;
        while let Ok(event) = self.events.try_recv() {
            if let AppEvent::Subagent(envelope) = event
                && matches!(envelope.event, SubagentEvent::MessageRefused { .. })
            {
                refused += 1;
            }
        }
        refused
    }
}

/// Everything a refused continuation must leave untouched.
#[derive(Debug, PartialEq)]
pub(super) struct Quiescence {
    requests: Vec<Vec<Value>>,
    rows: Vec<(u64, RoomEvent)>,
    nodes: Vec<RegistryEntry>,
    tainted: Vec<bool>,
    state_mutations: u64,
    parked: usize,
}

pub(super) async fn quiescence(rail: &ModelRail, peers: &[&ScriptedPeer]) -> Quiescence {
    let nodes = rail.tree.list().await;
    let mut tainted = Vec::with_capacity(nodes.len());
    for node in &nodes {
        tainted.push(rail.tree.is_tainted(&node.agent_id).await);
    }
    Quiescence {
        requests: peers.iter().map(|peer| peer.requests()).collect(),
        rows: rail.rows().await,
        nodes,
        tainted,
        state_mutations: rail.tree.state_mutation_count(),
        parked: rail.parked(),
    }
}

/// The exact error text the model receives for `peer`'s tool.
pub(super) fn refusal(peer: &str, reason: &str) -> String {
    format!("capability invocation failed for a2a::{peer}::{SKILL}: {reason}")
}

/// `a2a-continue-<16 lowercase hex>`: host-minted, no peer text.
pub(super) fn assert_handle(handle: &str) {
    let hex = handle
        .strip_prefix("a2a-continue-")
        .unwrap_or_else(|| panic!("handle prefix: {handle:?}"));
    assert!(
        hex.len() == 16
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "handle is 16 lowercase hex: {handle:?}"
    );
}

/// The exact AC2 tool content for a parked question.
pub(super) fn question_content(handle: &str, task: &str, question: &str) -> Value {
    json!({
        "state": "input-required",
        "continue": handle,
        "instructions": ANSWER_INSTRUCTIONS,
        "task": task_body(task, "input-required", Some(question)),
    })
}

/// Parse a tool result and pin that it is pretty JSON.
pub(super) fn parse_content(content: &str) -> Value {
    let parsed: Value = serde_json::from_str(content).expect("JSON tool content");
    assert_eq!(
        content,
        serde_json::to_string_pretty(&parsed).expect("pretty"),
        "the tool content is pretty JSON"
    );
    parsed
}

/// Delegate [`DELEGATION`] and return the handle of the peer's question.
/// The caller scripted `message/send` → `working`, `tasks/get` → `Ask(question)`.
pub(super) async fn park(rail: &ModelRail, peer: &ScriptedPeer, question: &str) -> String {
    let content = rail
        .invoke(peer.alias, json!({ "message": DELEGATION }))
        .await
        .expect("an input-required question is an Ok tool result");
    let parsed = parse_content(&content);
    let handle = parsed["continue"]
        .as_str()
        .expect("a continuation handle")
        .to_owned();
    assert_handle(&handle);
    assert_eq!(parsed, question_content(&handle, &peer.task_id(), question));
    handle
}

/// The continuation arguments.
pub(super) fn answer(handle: &str, message: &str) -> Value {
    json!({ "continue": handle, "message": message })
}

/// The model rail's dispatch row for one sent `message/send` request.
pub(super) fn dispatched(
    spec: &A2aPeerSpec,
    request: &Value,
    endpoint: Option<String>,
) -> RoomEvent {
    let message = &request["params"]["message"];
    RoomEvent::RemoteEnvelopeDispatched {
        peer: spec.resolved_identity(),
        task: Some(message["messageId"].as_str().expect("messageId").to_owned()),
        bytes: message["parts"][0]["text"].as_str().expect("text").len(),
        act: DispatchAct::Task,
        content_hash: None,
        endpoint,
        action: None,
        authorship: Authorship::AgentComposed,
        send_authorization: SendAuthorization::Unknown,
    }
}

/// The settlement row of an outbound refusal or cancellation.
pub(super) fn rejected(spec: &A2aPeerSpec, task: &str, detail: &str) -> RoomEvent {
    RoomEvent::RemoteEnvelopeRejected {
        rail: Some(rustain::domain::models::PeerRail::A2a),
        peer: spec.resolved_identity(),
        reason: rustain::domain::models::RejectReason::Policy {
            detail: detail.to_owned(),
        },
        direction: rustain::domain::models::Direction::Outbound,
        task: Some(task.to_owned()),
    }
}

/// The shipped content hash of an accepted remote result.
pub(super) fn result_hash(value: &Value) -> ContentHash {
    use sha2::Digest as _;
    ContentHash::from_bytes(
        sha2::Sha256::digest(serde_json::to_vec(value).expect("serialize")).into(),
    )
}

/// The `MessageRefused` receipt for `task` on `node`.
pub(super) fn assert_refused(envelope: &SubagentEnvelope, node: &RegistryEntry, task: &str) {
    assert_eq!(envelope.agent_id, node.agent_id);
    assert_eq!(envelope.parent_tool_call_id, SKILL);
    assert_eq!(
        envelope.event,
        SubagentEvent::MessageRefused {
            correlation_id: CorrelationId::new(task),
            reason: RefuseReason::Policy,
        }
    );
}

pub(super) fn only_rows(rows: &[(u64, RoomEvent)]) -> Vec<RoomEvent> {
    rows.iter().map(|(_, row)| row.clone()).collect()
}

/// `K01` — `working` then `input-required`: the tool returns the question as
/// an `Ok` result with a fresh handle, the remote task stays open (zero
/// `tasks/cancel`), and the node waits on the model, tainted.
/// `M01` (cancel input-required again) → RED here.
#[tokio::test]
async fn k01_input_required_parks_the_node_and_returns_the_question() {
    let peer = ScriptedPeer::start(PEER, TaskIds::Minted).await;
    peer.script("message/send", [Answer::State("working")]);
    peer.script("tasks/get", [Answer::Ask(QUESTION)]);
    let mut rail = model_rail(vec![peer.spec()]).await;

    let handle = park(&rail, &peer, QUESTION).await;

    // The peer saw one send and one poll; the open task was never cancelled.
    assert_eq!(peer.methods(), ["message/send", "tasks/get"]);
    assert!(
        peer.requests_for("tasks/cancel").is_empty(),
        "AC1: zero tasks/cancel"
    );
    let requests = peer.requests();
    let send = &requests[0];
    let message = &send["params"]["message"];
    assert_eq!(
        message["parts"],
        json!([{ "kind": "text", "text": DELEGATION }])
    );
    assert!(
        message.get("taskId").is_none() && message.get("contextId").is_none(),
        "a first send carries no continuation: {send}"
    );
    assert!(
        message["metadata"][ITEM_CAPABILITY].is_string(),
        "the model rail's send is capability-bearing: {send}"
    );
    assert!(
        message["metadata"].get(REPLY_CAPABILITY).is_none(),
        "a first send presents no reply capability: {send}"
    );
    // A minted id is not this host's message id: the poll presents nothing.
    assert_eq!(requests[1]["params"], json!({ "id": TASK }));

    // The node waits on the model's answer, tainted by the peer's text.
    let node = rail.peer_node().await;
    assert_eq!(node.current_status, NodeState::Waiting);
    assert_eq!(node.wait_reason, Some(WaitReason::AwaitingHumanInput));
    assert!(
        rail.tree.is_tainted(&node.agent_id).await,
        "peer text entered model context"
    );
    assert_eq!(rail.parked(), 1, "one parked continuation");
    assert!(!handle.contains(TASK) && !handle.contains(CONTEXT));

    // The journal holds the dispatch and nothing that rejects or accepts.
    let rows = only_rows(&rail.rows().await);
    let endpoint = match rows.first() {
        Some(RoomEvent::RemoteEnvelopeDispatched { endpoint, .. }) => endpoint.clone(),
        other => panic!("the first row is the dispatch: {other:?}"),
    };
    assert!(
        endpoint.is_some(),
        "an unpinned peer's dispatch names its origin digest"
    );
    assert_eq!(rows, vec![dispatched(&peer.spec(), send, endpoint)]);
    assert_eq!(
        rail.refused_so_far(),
        0,
        "a parked question is not a refusal"
    );
}
