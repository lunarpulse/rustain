//! Story `agents-thread` `AK01` (AC1, AC3(a), AC4, AC10), and the two-host helpers the other
//! keystones share (`pub(super)`, the `b_k01_model_park` precedent).
//!
//! Sender A is the production model rail (`super::b_k01_model_park::model_rail`, which keeps
//! the `NodeTree`) delegating to a real rustain host B: the 19.18 `Host` under
//! `NotifyAndWait` with an attached operator — 19.18b `K05`'s scenario: B asks for a decision,
//! the rail cancels, and the delegation row stays in A's tree. The Agents tab is fed through
//! the roster seam `refresh_roster` runs (`mission_bridge::apply_roster`, then
//! `agents_thread::refresh_offers_from` over `NodeTree::list()`); keys go through the local
//! front door (`app::handle_input`); `t`'s effect is the event loop's
//! `agents_thread::open`, and its answer is the real `AppEvent::AgentThreadReady` read from
//! A's `EventBus`, applied by `agents_thread::ready` exactly as the loop's arm does. The
//! follow-up keystones reach `transparency_bridge::follow_up_sent` with the
//! `InputAction::ThreadFollowUp` the field's `Enter` returned and settle through the real
//! `AppEvent::TeamSendSettled`.
//!
//! B's request accounting: every served `message/send` and `tasks/*` writes through the served
//! sink (`HostOptions::serve_wrap`), while `x-rustain-items/{list,thread}` write nothing
//! (`items_thread`: "Side-effect free"), so a served-sink row count that does not move proves
//! that no message or task request reached B.
//!
//! ⛔ Never used: constructing `AgentThreadReady`, `read_thread_of_task`, `open_sent` /
//! `fill_sent`, `start_team_send` / `coordinate_send` / `deliver_one`.

#![cfg(feature = "test-instrumentation")]

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use rustain::adapters::tui::mission_control::{self, McTab};
use rustain::adapters::tui::notifications::NoticeSeverity;
use rustain::adapters::tui::presentation::{self, Surface, reducer};
use rustain::adapters::tui::state::{ThreadAccess, ThreadTarget};
use rustain::domain::models::{
    AgentId, AgentThreadOutcome, CapabilityId, RedactedUrl, SendAuthorization, SentThreadOffer,
};
use rustain::infrastructure::runtime::{agents_thread, mission_bridge};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::b_k01_model_park::{ModelRail, SKILL, model_rail};
use super::*;

/// The conversation every front door of these keystones runs under.
pub(super) const CONV: &str = "conv";
/// The model tool a rustain host's card offers.
pub(super) const B_TOOL: &str = "delegate";
/// The operator's Main draft and caret (AC10): never read or written by any of this.
pub(super) const DRAFT: &str = "main draft\nline 2";
pub(super) const CARET: usize = 7;
/// What the operator types into the follow-up field.
pub(super) const TEXT: &str = "follow-up-from-the-frame";

// ── Host B, real ────────────────────────────────────────────────────────────

/// A real rustain host B with its served sink counted.
pub(super) struct Peer {
    pub(super) host: Host,
    served: Arc<AtomicUsize>,
    _operator: tokio::net::UnixStream,
    shutdown: CancellationToken,
}

/// Counts every row a served verb writes.
struct ServedRows {
    inner: Arc<dyn RoomJournal>,
    rows: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl RoomJournal for ServedRows {
    async fn record_event(&self, event: RoomEvent) -> Result<(), RoomJournalError> {
        self.rows.fetch_add(1, Ordering::SeqCst);
        self.inner.record_event(event).await
    }
}

pub(super) async fn rustain_peer() -> Peer {
    let served = Arc::new(AtomicUsize::new(0));
    let mut options = HostOptions::new(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait);
    options.serve_wrap = Some({
        let served = served.clone();
        Arc::new(move |inner| {
            Arc::new(ServedRows {
                inner,
                rows: served.clone(),
            }) as Arc<dyn RoomJournal>
        })
    });
    let mut host = Host::serve_with(options).await;
    // A live writer, so B's turn forwarder runs (19.18b `K05`).
    let (operator, shutdown) = attach_operator(&mut host).await;
    Peer {
        host,
        served,
        _operator: operator,
        shutdown,
    }
}

impl Peer {
    /// Rows B's served verbs wrote so far.
    pub(super) fn served_rows(&self) -> usize {
        self.served.load(Ordering::SeqCst)
    }

    /// Structural wait (19.18b `K05`): B's node for a cancelled delegation is gone, so nothing
    /// of that exchange is still writing.
    pub(super) async fn quiesce(&self) {
        let deadline = tokio::time::Instant::now() + SETTLE;
        let inbound = rustain::adapters::a2a::exec::INBOUND_SUBAGENT_TYPE;
        while self
            .host
            .node_tree
            .list()
            .await
            .iter()
            .any(|entry| entry.subagent_type == inbound)
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "B's delegated node never settled"
            );
            tokio::task::yield_now().await;
        }
    }

    /// The one item B journaled for `task`.
    pub(super) async fn item_of(&self, task: &str) -> String {
        let mut items: Vec<String> = received(&room_events(&self.host.journal).await)
            .into_iter()
            .filter(|(_, minted_for, _)| minted_for == task)
            .map(|(item, _, _)| item)
            .collect();
        assert_eq!(items.len(), 1, "B minted one item for {task}: {items:?}");
        items.remove(0)
    }

    pub(super) fn stop(&self) {
        self.shutdown.cancel();
        self.host.cancel.cancel();
    }
}

// ── A third-party or rustain-shaped peer, scripted ──────────────────────────

/// The id a third-party server mints for its own task.
pub(super) const THIRD_PARTY_TASK: &str = "task-minted-by-the-peer";

/// How the scripted peer answers one `message/send`, in order.
#[derive(Clone, Copy, Debug)]
pub(super) enum SendAnswer {
    /// A completed task whose id is the request's `messageId` (a rustain host's shape).
    Echo,
    /// A completed task with an id of the peer's own (a third-party server).
    Minted,
    /// A delivered rustain item: the `messageId` task naming this item id.
    Item(&'static str),
    /// A completed task whose id is the FIRST `messageId` this peer ever received: a replay
    /// of an earlier exchange as the answer to a later message (AT-02).
    Replay,
    /// A JSON-RPC error after the request was taken: an answer that proves nothing.
    Lost,
}

/// What `x-rustain-items/list` answers: rows `(itemId, task)` shaped like `server.rs`
/// `items_list` (`None` names the first `messageId` this peer received), or an HTTP failure.
pub(super) enum List {
    Rows(Vec<(&'static str, Option<&'static str>)>),
    Http(u16),
}

struct Script {
    first_message: Option<String>,
    sends: VecDeque<SendAnswer>,
    list: List,
    /// The one item turn `x-rustain-items/thread` serves: `(item, content)`.
    thread: Option<(&'static str, &'static str)>,
}

struct Shared {
    script: parking_lot::Mutex<Script>,
    log: parking_lot::Mutex<Vec<Value>>,
}

impl Shared {
    fn answer(&self, request: &wiremock::Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).expect("a JSON-RPC request");
        let id = body["id"].clone();
        let rpc_method = body["method"].as_str().unwrap_or_default().to_owned();
        let message_id = body["params"]["message"]["messageId"]
            .as_str()
            .map(str::to_owned);
        self.log.lock().push(body);
        let envelope = |key: &str, payload: Value| {
            let mut envelope = json!({ "jsonrpc": "2.0", "id": id });
            envelope[key] = payload;
            ResponseTemplate::new(200).set_body_json(envelope)
        };
        let error = |code: i64, message: &str| {
            envelope("error", json!({ "code": code, "message": message }))
        };
        let task = |id: &str, state: &str| json!({ "kind": "task", "id": id, "contextId": "ctx-agents-thread", "status": { "state": state } });
        let mut script = self.script.lock();
        match rpc_method.as_str() {
            "message/send" => {
                let message_id = message_id.expect("message/send carries a messageId");
                if script.first_message.is_none() {
                    script.first_message = Some(message_id.clone());
                }
                match script.sends.pop_front() {
                    Some(SendAnswer::Echo) => envelope("result", task(&message_id, "completed")),
                    Some(SendAnswer::Minted) => {
                        envelope("result", task(THIRD_PARTY_TASK, "completed"))
                    }
                    Some(SendAnswer::Item(item)) => {
                        let mut answer = task(&message_id, "submitted");
                        answer["metadata"] = json!({ "x-rustain-item-id": item });
                        envelope("result", answer)
                    }
                    Some(SendAnswer::Replay) => {
                        let first = script.first_message.clone().unwrap_or_default();
                        envelope("result", task(&first, "completed"))
                    }
                    Some(SendAnswer::Lost) | None => error(-32603, "the answer was lost"),
                }
            }
            "x-rustain-items/list" => {
                let first = script.first_message.clone().unwrap_or_default();
                match &script.list {
                    List::Http(status) => ResponseTemplate::new(*status).set_body_string("down"),
                    List::Rows(rows) => {
                        let items: Vec<Value> = rows
                            .iter()
                            .enumerate()
                            .map(|(ordinal, (item, task))| {
                                json!({
                                    "itemId": item,
                                    "task": (*task).map_or_else(|| first.clone(), str::to_owned),
                                    "state": "received",
                                    "ordinal": ordinal,
                                })
                            })
                            .collect();
                        let listed = json!({ "items": items, "principalCollapsed": true });
                        envelope("result", listed)
                    }
                }
            }
            "x-rustain-items/thread" => match script.thread {
                Some((item, content)) => envelope(
                    "result",
                    json!({
                        "root": item,
                        "turns": [{
                            "turnId": item, "kind": "item",
                            "task": script.first_message.clone().unwrap_or_default(),
                            "state": "received", "content": content, "ordinal": 0,
                        }],
                        "omittedBefore": 0,
                        "principalCollapsed": true,
                    }),
                ),
                None => error(-32001, "Item not found"),
            },
            _ => error(-32001, "Task not found"),
        }
    }
}

/// A `wiremock` A2A peer (labelled fixture shapes, not captures): the 19.18b card with skill
/// [`SKILL`], one JSON-RPC endpoint that logs every request it receives.
pub(super) struct ItemsPeer {
    _server: MockServer,
    alias: &'static str,
    uri: String,
    shared: Arc<Shared>,
}

impl ItemsPeer {
    pub(super) async fn start(
        alias: &'static str,
        sends: impl IntoIterator<Item = SendAnswer>,
    ) -> Self {
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
        let shared = Arc::new(Shared {
            script: parking_lot::Mutex::new(Script {
                first_message: None,
                sends: sends.into_iter().collect(),
                list: List::Rows(Vec::new()),
                thread: None,
            }),
            log: parking_lot::Mutex::new(Vec::new()),
        });
        let responder = shared.clone();
        Mock::given(method("POST"))
            .and(path("/jsonrpc"))
            .respond_with(move |request: &wiremock::Request| responder.answer(request))
            .mount(&server)
            .await;
        Self {
            uri: server.uri(),
            _server: server,
            alias,
            shared,
        }
    }

    pub(super) fn spec(&self) -> A2aPeerSpec {
        A2aPeerSpec::new(
            self.alias,
            RedactedUrl::from(self.uri.clone()),
            A2aPeerSource::Workspace,
        )
    }

    pub(super) fn list(&self, list: List) {
        self.shared.script.lock().list = list;
    }

    pub(super) fn thread(&self, item: &'static str, content: &'static str) {
        self.shared.script.lock().thread = Some((item, content));
    }

    pub(super) fn first_message(&self) -> String {
        self.shared
            .script
            .lock()
            .first_message
            .clone()
            .expect("a message was sent")
    }

    pub(super) fn request_count(&self) -> usize {
        self.shared.log.lock().len()
    }

    pub(super) fn requests_for(&self, rpc_method: &str) -> Vec<Value> {
        self.shared
            .log
            .lock()
            .iter()
            .filter(|request| request["method"].as_str() == Some(rpc_method))
            .cloned()
            .collect()
    }
}

// ── Sender A ────────────────────────────────────────────────────────────────

/// One model delegation, `a2a::<alias>::<tool>` through the provider (the model's tool call).
/// Returns the delegation row's id and its task `T` (A's own dispatch row); the tool result
/// is not the subject here.
pub(super) async fn delegate(rail: &ModelRail, alias: &str, tool: &str) -> (AgentId, String) {
    use rustain::domain::ports::CapabilityProvider;
    let rows_before = rail.peer_nodes().await;
    let tasks_before = own_tasks(&rail.rows().await);
    let id = CapabilityId {
        protocol: "a2a".to_owned(),
        server: alias.to_owned(),
        tool: tool.to_owned(),
    };
    let _answer = tokio::time::timeout(
        SETTLE,
        rail.egress.provider().invoke(
            &id,
            json!({ "message": "migrate the billing tables" }),
            CancellationToken::new(),
        ),
    )
    .await
    .expect("the delegation settles");
    let row = rail
        .peer_nodes()
        .await
        .into_iter()
        .find(|entry| !rows_before.iter().any(|old| old.agent_id == entry.agent_id))
        .expect("the delegation registered its row");
    let mut tasks: Vec<String> = own_tasks(&rail.rows().await)
        .into_iter()
        .filter(|task| !tasks_before.contains(task))
        .collect();
    assert_eq!(tasks.len(), 1, "one own dispatch: {tasks:?}");
    (row.agent_id, tasks.remove(0))
}

fn own_tasks(rows: &[(u64, RoomEvent)]) -> Vec<String> {
    rows.iter()
        .filter_map(|(_, row)| match row {
            RoomEvent::RemoteEnvelopeDispatched {
                task: Some(task),
                act,
                ..
            } if act.is_task() => Some(task.clone()),
            _ => None,
        })
        .collect()
}

/// `(task, authorship, authorization)` of every dispatch row in `rows`.
pub(super) fn dispatches(
    rows: &[(u64, RoomEvent)],
) -> Vec<(String, Authorship, SendAuthorization)> {
    rows.iter()
        .filter_map(|(_, row)| match row {
            RoomEvent::RemoteEnvelopeDispatched {
                task: Some(task),
                authorship,
                send_authorization,
                ..
            } => Some((task.clone(), *authorship, *send_authorization)),
            _ => None,
        })
        .collect()
}

/// A provider that only counts: opening a thread is never a model turn.
struct CountingProvider {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl StreamingProvider for CountingProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<Message>,
        _options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        use futures::StreamExt;
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(futures::stream::empty().boxed())
    }
    async fn abort(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    fn provider_id(&self) -> String {
        "agents-thread-counting".to_owned()
    }
    fn list_models(&self) -> Vec<rustain::domain::models::provider::ModelDescriptor> {
        Vec::new()
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

/// A's interactive session over the rail: its `AppState` (send runtime = the rail's runtime,
/// provider counted) and the receiving end of its `EventBus`.
pub(super) struct Session {
    pub(super) app: AppState,
    events: tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
    provider_calls: Arc<AtomicUsize>,
}

/// One `AppEvent::AgentThreadReady`, as the bus delivered it.
pub(super) struct Ready {
    conversation_id: String,
    generation: u64,
    pub(super) agent_id: AgentId,
    outcome: AgentThreadOutcome,
}

impl Ready {
    /// The event loop's `AgentThreadReady` arm.
    pub(super) fn apply(self, state: &mut TuiState) -> bool {
        agents_thread::ready(
            state,
            &self.conversation_id,
            self.generation,
            &self.agent_id,
            self.outcome,
        )
    }
}

impl Session {
    pub(super) fn over(rail: &ModelRail) -> Self {
        let (mut app, events) = app_state(&rail.journal.path);
        app.a2a_send = Some(rail.egress.runtime().clone());
        let provider_calls = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn StreamingProvider> = Arc::new(CountingProvider {
            calls: provider_calls.clone(),
        });
        app.provider.store(Arc::new(provider));
        Self {
            app,
            events,
            provider_calls,
        }
    }

    pub(super) fn provider_requests(&self) -> usize {
        self.provider_calls.load(Ordering::SeqCst)
    }

    async fn next_event(&mut self) -> AppEvent {
        tokio::time::timeout(SETTLE, self.events.recv())
            .await
            .expect("an event before the deadline")
            .expect("the bus stays open")
    }

    /// The next lookup answer on A's bus.
    pub(super) async fn next_ready(&mut self) -> Ready {
        let event = self.next_event().await;
        let AppEvent::AgentThreadReady {
            conversation_id,
            generation,
            agent_id,
            outcome,
        } = event
        else {
            panic!("a lookup publishes AgentThreadReady, got {event:?}");
        };
        Ready {
            conversation_id,
            generation,
            agent_id,
            outcome,
        }
    }

    /// Nothing else was published.
    pub(super) fn quiet(&mut self) -> bool {
        self.events.try_recv().is_err()
    }

    /// `/team thread <peer> <item>` typed in a fresh TUI of the same session (the parser, the
    /// rail-3 spawn, `TeamThreadReady`, its handler): what AC3(a)'s frame must equal.
    pub(super) async fn typed_thread(&mut self, peer: &str, item: &str) -> TuiState {
        let mut state = TuiState::new(100, 30);
        state.input_buffer = format!("/team thread {peer} {item}");
        let InputAction::ExecuteCommand { name, args } = submit_message_for_test(&mut state) else {
            panic!("/team thread reaches the command dispatcher");
        };
        assert_eq!(name, "team");
        team_command(&mut state, CONV, args.as_deref(), &self.app).await;
        let event = self.next_event().await;
        let AppEvent::TeamThreadReady {
            conversation_id,
            generation,
            message,
            follow_up,
        } = event
        else {
            panic!("a thread read publishes TeamThreadReady, got {event:?}");
        };
        team_thread_ready(
            &mut state,
            &conversation_id,
            generation,
            &message,
            follow_up.as_deref(),
        );
        state
    }

    /// A typed `/team <args>` line through the parser and the command dispatcher.
    pub(super) async fn type_team(&mut self, state: &mut TuiState, line: &str) {
        state.input_buffer = line.to_owned();
        let InputAction::ExecuteCommand { name, args } = submit_message_for_test(state) else {
            panic!("{line:?} reaches the command dispatcher");
        };
        assert_eq!(name, "team");
        team_command(state, CONV, args.as_deref(), &self.app).await;
    }

    /// The next `AppEvent::TeamSendSettled`, applied by the event loop's handler. Returns the
    /// send block it settled.
    pub(super) async fn settle_send(&mut self, state: &mut TuiState) -> String {
        let mut tabs = rustain::domain::models::tab::TabManager::new(CancellationToken::new());
        let event = self.next_event().await;
        let AppEvent::TeamSendSettled {
            conversation_id,
            block_id,
            index,
            outcome,
        } = event
        else {
            panic!("a send settles with TeamSendSettled, got {event:?}");
        };
        team_send_settled(
            CONV,
            state,
            &mut tabs,
            &conversation_id,
            &block_id,
            index,
            outcome,
        );
        block_id
    }
}

// ── The TUI ─────────────────────────────────────────────────────────────────

/// `refresh_roster`'s two steps over A's tree: the roster, then the owner's offers.
pub(super) async fn refresh(state: &mut TuiState, rail: &ModelRail, app: &AppState) {
    let rows: Vec<_> = rail
        .tree
        .list()
        .await
        .iter()
        .map(|entry| entry.to_view())
        .collect();
    mission_bridge::apply_roster(state, rows.clone());
    agents_thread::refresh_offers_from(state, &rows, app.a2a_send.as_deref()).await;
}

/// A roster refresh, then `Ctrl+X s`'s reducer: Mission Control on Agents.
pub(super) async fn show_agents(state: &mut TuiState, rail: &ModelRail, app: &AppState) {
    refresh(state, rail, app).await;
    reducer::open_mission_control(state, McTab::Agents);
}

/// A plain key press through the local front door.
pub(super) fn press(state: &mut TuiState, code: KeyCode) -> InputAction {
    let event = Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
    let domain = rustain::adapters::tui::app::convert_crossterm_event(
        &event,
        &rustain::domain::models::MouseConfig::default(),
    )
    .expect("a key event");
    rustain::adapters::tui::app::handle_input(state, &domain)
}

/// Select `id` on the Agents tab with `k`/`j`.
pub(super) fn select(state: &mut TuiState, id: &AgentId) {
    let rows = state.agent_panel_state.cached_entries.len();
    for _ in 0..rows {
        assert_eq!(press(state, KeyCode::Char('k')), InputAction::Consumed);
    }
    for _ in 0..=rows {
        if state.mission_control.selected_id() == Some(id.as_str()) {
            return;
        }
        assert_eq!(press(state, KeyCode::Char('j')), InputAction::Consumed);
    }
    panic!("{id:?} is not a row of the Agents tab");
}

/// `(peer, item)` of the open sent-thread frame.
pub(super) fn sent_item(state: &TuiState) -> Option<(String, String)> {
    match &state.thread_inspector.as_ref()?.target {
        ThreadTarget::Sent { peer, item } => Some((peer.clone(), item.clone())),
        _ => None,
    }
}

pub(super) fn follow_up(state: &TuiState) -> Option<String> {
    let sent = state.thread_inspector.as_ref()?.sent.as_ref()?;
    sent.follow_up.clone()
}

pub(super) fn field_text(state: &TuiState) -> Option<String> {
    let reply = state.thread_inspector.as_ref()?.reply.as_ref()?;
    Some(reply.field.as_str().to_owned())
}

pub(super) fn last_notice(state: &TuiState) -> Option<(NoticeSeverity, String)> {
    state
        .notifications
        .records()
        .next_back()
        .map(|record| (record.severity, record.message.clone()))
}

pub(super) fn outcome_lines(state: &TuiState, key: &str) -> Vec<String> {
    state
        .thread_outcomes
        .lines(key)
        .map(str::to_owned)
        .collect()
}

/// `f`, the text typed key by key, then `Enter`: what the field asks the host to do.
pub(super) fn write_follow_up(state: &mut TuiState, text: &str) -> InputAction {
    assert_eq!(press(state, KeyCode::Char('f')), InputAction::Consumed);
    assert_eq!(field_text(state).as_deref(), Some(""), "an empty field");
    for c in text.chars() {
        assert_eq!(press(state, KeyCode::Char(c)), InputAction::Consumed);
    }
    assert_eq!(field_text(state).as_deref(), Some(text));
    press(state, KeyCode::Enter)
}

/// `AK01` — AC1: the production refresh offers exactly the delegation row, keyed by its id.
/// AC3(a): `t`'s effect and its bus answer open `Thread · sent · b` on B's journaled item for
/// `T`, a writer frame byte-identical to typed `/team thread b <item>`; B served no message or
/// task request, A journaled nothing but `PeerReplyObserved`, no provider request, the Main
/// draft is byte-identical. AC6: the delegated item is this host's own turn, so the frame's
/// follow-up target is that item, exactly as on the typed frame. AC4: `Esc` returns to Mission
/// Control on the same row. `M6`/`M7`/`M8` → RED here.
#[tokio::test]
async fn ak01_open_thread_reads_the_one_exchange_and_sends_nothing() {
    let b = rustain_peer().await;
    let rail = model_rail(vec![b.host.peer("b")]).await;
    let (row, task) = delegate(&rail, "b", B_TOOL).await;
    b.quiesce().await;
    let item = b.item_of(&task).await;
    let mut session = Session::over(&rail);
    let mut state = TuiState::new(100, 30);
    state.input_buffer = DRAFT.to_owned();
    state.cursor_position = CARET;

    show_agents(&mut state, &rail, &session.app).await;
    assert_eq!(
        state.agent_threads.offers,
        BTreeMap::from([(
            row.clone(),
            SentThreadOffer {
                peer: "b".to_owned(),
                task: task.clone(),
            },
        )])
    );
    select(&mut state, &row);

    let served = b.served_rows();
    let a_rows = rail.rows().await.len();
    agents_thread::open(&mut state, CONV, &session.app, row.clone());
    assert!(
        state
            .agent_threads
            .opening
            .as_ref()
            .is_some_and(|opening| opening.agent == row)
    );
    assert!(session.next_ready().await.apply(&mut state));
    assert!(state.agent_threads.opening.is_none());

    assert_eq!(sent_item(&state), Some(("b".to_owned(), item.clone())));
    assert_eq!(
        state.thread_inspector.as_ref().map(|frame| frame.access),
        Some(ThreadAccess::Writer)
    );
    let top = state.modal_stack.top().expect("a frame on top");
    assert_eq!(top.tool_title.as_deref(), Some("Thread · sent · b"));
    // AC6 (D6): the delegation's dispatch row carries the hash of the text it delegated, so the
    // served item turn is `mine` by the same task-and-hash rule as a typed send, and the frame
    // the Agents row opened targets it. (This assertion was `None` while the model rail's
    // dispatch row was hashless — AT-01.)
    assert_eq!(follow_up(&state), Some(item.clone()));
    let opened = state
        .thread_inspector
        .as_ref()
        .map(|frame| frame.content.clone())
        .expect("the frame");

    assert_eq!(b.served_rows(), served, "no message or task request");
    let new_rows = rail.rows().await.split_off(a_rows);
    assert!(
        new_rows
            .iter()
            .all(|(_, row)| matches!(row, RoomEvent::PeerReplyObserved { .. })),
        "the open journals reply observations only: {new_rows:?}"
    );
    assert_eq!(session.provider_requests(), 0, "never a model turn");
    assert_eq!(state.input_buffer, DRAFT);
    assert_eq!(state.cursor_position, CARET);

    let typed = session.typed_thread("b", &item).await;
    assert_eq!(sent_item(&typed), Some(("b".to_owned(), item.clone())));
    let typed_content = typed.thread_inspector.as_ref().map(|frame| &frame.content);
    assert_eq!(
        typed_content,
        Some(&opened),
        "byte-identical to /team thread b <item>"
    );
    assert_eq!(
        follow_up(&typed),
        Some(item.clone()),
        "the typed frame targets the same own turn"
    );

    assert_eq!(press(&mut state, KeyCode::Esc), InputAction::Consumed);
    assert!(mission_control::is_top(&state));
    assert_eq!(state.mission_control.tab, McTab::Agents);
    assert_eq!(state.mission_control.selected_id(), Some(row.as_str()));
    assert_eq!(press(&mut state, KeyCode::Esc), InputAction::Consumed);
    assert!(!presentation::is_open(&state, Surface::MissionControl));
    assert_eq!(state.input_buffer, DRAFT);
    assert_eq!(state.cursor_position, CARET);
    b.stop();
}
