//! A2A delegation runtime: materialize a `RemotePeer` node, drive its task
//! lifecycle to terminal, project each hop onto `NodeState`, and journal the
//! room events — durable-first.
//!
//! Ruling 5 (Story 17.4b): no new port, no `NodeHandle::Remote`. The node is
//! `NodeHandle::Local` (so `LocalMessageBus` admission and `cascade_kill` work
//! unmodified) with a driver that owns the HTTP client — exactly the RAP
//! peer-delivery precedent. `Op::Kill` and the `CancellationToken` reach the
//! driver via the node's cancel token; the poll loop turns that into a real
//! `tasks/cancel` (Task 4).
//!
//! R-E: this file uses `NodeTree::try_set_state` exclusively — never the silent
//! `set_state` shim (enforced by `a2a_adapter_never_calls_the_silent_set_state_shim`).
//!
//! R-D admission/content split: `TrustTier` is threaded in as an *admission*
//! signal only and is stamped into observability; it NEVER decides what happens
//! to returned content. `delegate` returns byte-identical content for both tiers
//! given the same transcript (proven by `both_trust_tiers_return_identical_content`).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::domain::events::{AppEvent, DomainEventPayload};
use crate::domain::models::{
    A2aPeerSpec, AgentId, AgentMetrics, Authorship, CapabilityTokenId, ContentHash, CorrelationId,
    Direction, MessageKind, NodeState, Op, PeerId, RapTaskState, RecordedStory, RefuseReason,
    RejectReason, RoomEvent, SemanticMessageType, SendActionId, SendActionRecipient, SendActionRef,
    SendAuthorization, SendSettlement, StoryManifest, SubagentEnvelope, SubagentEvent, TrustTier,
    semantic_message_type_metadata,
};
use crate::domain::ports::{RoomJournal, RoomJournalError};
use crate::domain::services::peer_text::sanitize_peer_text_line;
use crate::domain::services::transparency::{
    MAX_PEER_ID_BYTES, MAX_SUMMARY_BYTES, TRUNCATION_MARKER, sanitize_disclosable,
};
use crate::infrastructure::subagent::{AgentHandle, MailboxBudget, NodeTree};

use super::client::A2aClientAdapter;
use super::error::A2aError;
use super::item_capability::{
    ItemCapability, ItemCapabilityKey, ItemCapabilityKeyState, derive_item_capability,
};
use super::jsonrpc::JsonRpcRequest;
use super::lifecycle::{
    A2aTaskTransport, LifecycleOutcome, PollConfig, TaskSnapshot, poll_from_snapshot,
};
use super::task::A2aTaskState;

/// Story 19.18c `A9` — the one `(messageId, capability)` a [`TaskClient`] may
/// present: the message it just sent. ⛔ Never derived for a peer-supplied id.
#[derive(Clone, Debug)]
pub struct OwnTask {
    message_id: String,
    capability: ItemCapability,
}

impl OwnTask {
    #[must_use]
    pub fn message_id(&self) -> &str {
        &self.message_id
    }
}

/// A live JSON-RPC transport bound to one resolved endpoint. Generates a
/// monotonic correlation id per call.
pub struct TaskClient {
    client: Arc<A2aClientAdapter>,
    endpoint: String,
    next_id: AtomicU64,
    own_task: Option<OwnTask>,
}

/// Story 19.18c — `params` of an `x-rustain-items/*` verb: `{"itemId"}` plus
/// `capability` when this host derived one for the item. Unknown fields are
/// ignored by an older host (`AD-1826`), so its items stay legacy.
pub(crate) fn item_verb_params(
    item_id: &str,
    capability: Option<&ItemCapability>,
) -> serde_json::Value {
    let mut params = serde_json::json!({ "itemId": item_id });
    if let Some(capability) = capability {
        params["capability"] = serde_json::Value::from(capability.as_wire_str());
    }
    params
}

impl TaskClient {
    pub fn new(client: Arc<A2aClientAdapter>, endpoint: String) -> Self {
        Self {
            client,
            endpoint,
            next_id: AtomicU64::new(1),
            own_task: None,
        }
    }

    /// Story 19.18c `A9` — bind this transport to the message it sends, so
    /// its `tasks/get`/`tasks/cancel` present the capability **only** for
    /// that message's id. Consuming, set before the transport is shared:
    /// ⛔ no interior mutability.
    #[must_use]
    pub fn with_own_task(mut self, own_task: OwnTask) -> Self {
        self.own_task = Some(own_task);
        self
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// `tasks/*` params: `{"id"}`, plus `metadata[x-rustain-item-capability]`
    /// (`P8`: the A2A extension slot) iff `task_id` is this transport's own
    /// message id (`A9`). A hostile peer that answers another id gets nothing.
    fn task_params(&self, task_id: &str) -> serde_json::Value {
        let mut params = serde_json::json!({ "id": task_id });
        if let Some(own) = &self.own_task
            && own.message_id == task_id
        {
            params["metadata"] = serde_json::json!({
                super::ITEM_CAPABILITY_METADATA_KEY: own.capability.as_wire_str(),
            });
        }
        params
    }

    /// Story 19.18 `AC6` — read one thread on the peer's host
    /// ([`super::ITEMS_THREAD_METHOD`]). Params are `{"itemId"}` plus, when
    /// this host derived one (Story 19.18c `A10`), `capability`: the served
    /// verb derives the principal from the authenticated caller. Sole
    /// production caller: the rail-3 `/team thread` spawn
    /// ([`super::send::read_thread`]).
    pub async fn read_thread(
        &self,
        item_id: &str,
        capability: Option<&ItemCapability>,
    ) -> Result<serde_json::Value, A2aError> {
        let request = JsonRpcRequest::new(
            self.next_id(),
            super::ITEMS_THREAD_METHOD,
            item_verb_params(item_id, capability),
        );
        self.client.post_jsonrpc(&self.endpoint, &request).await
    }

    /// Read this caller's own recipient-item set on the peer (Story 19.16b
    /// `AC1`, [`super::ITEMS_LIST_METHOD`]).
    ///
    /// On the **inherent** impl, not [`A2aTaskTransport`] (Story 19.16f
    /// `AC2`, `P11`): its production callers — the board's per-peer read
    /// ([`super::board::collect_board`]), the retract's confirm-time preview
    /// ([`super::send::preview_item_retract`]) and Story 19.18c's capability
    /// resolver — hold the concrete type, and a trait method made seven
    /// doubles answer for a seam they never exercised.
    pub async fn list_items(&self) -> Result<serde_json::Value, A2aError> {
        // ⛔ No parameters: the served verb derives the principal from the
        // authenticated caller, and a `peerId` parameter would be the
        // cross-peer enumeration oracle `ADR-17-4a-01` R21 forbids.
        let request = JsonRpcRequest::new(
            self.next_id(),
            super::ITEMS_LIST_METHOD,
            serde_json::json!({}),
        );
        self.client.post_jsonrpc(&self.endpoint, &request).await
    }

    /// Mark one item on the peer's host as retracted by its sender (Story
    /// 19.16f `AC2`; the served verb is Story 19.16d's
    /// [`super::ITEMS_RETRACT_METHOD`]). The sole production caller is the
    /// accepted retract card's dispatch ([`super::send::retract_item_on_peer`]).
    ///
    /// `params` come from [`item_verb_params`]: `{"itemId"}` plus, for an item
    /// this host sent under a capability (Story 19.18c `D2`), `capability` —
    /// the principal is the authenticated caller's, derived server-side. The
    /// caller builds them so the journaled dispatch counts the bytes actually
    /// sent. The request carries an `id` (`next_id`): the served arm refuses
    /// the notification form with `-32600`, because a write whose answer
    /// nobody reads is a write nobody can be told the outcome of.
    /// `before_send` runs after every deterministic no-I/O refusal,
    /// immediately before the request can leave this host.
    pub async fn retract_item<F, Fut>(
        &self,
        params: serde_json::Value,
        before_send: F,
    ) -> Result<serde_json::Value, A2aError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(), A2aError>>,
    {
        let request = JsonRpcRequest::new(self.next_id(), super::ITEMS_RETRACT_METHOD, params);
        self.client
            .post_jsonrpc_after(&self.endpoint, &request, before_send)
            .await
    }

    /// Story 19.17: send once, after a fallible durable-first dispatch append.
    pub async fn send_after<F, Fut>(
        &self,
        message: serde_json::Value,
        before_send: F,
    ) -> Result<serde_json::Value, A2aError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(), A2aError>>,
    {
        let request = JsonRpcRequest::new(self.next_id(), "message/send", message);
        self.client
            .post_jsonrpc_after(&self.endpoint, &request, before_send)
            .await
    }
}

#[async_trait]
impl A2aTaskTransport for TaskClient {
    fn own_task(&self) -> Option<OwnTask> {
        self.own_task.clone()
    }

    async fn message_send(
        &self,
        message: serde_json::Value,
    ) -> Result<serde_json::Value, A2aError> {
        let request = JsonRpcRequest::new(self.next_id(), "message/send", message);
        self.client.post_jsonrpc(&self.endpoint, &request).await
    }

    async fn tasks_get(&self, task_id: &str) -> Result<serde_json::Value, A2aError> {
        let request = JsonRpcRequest::new(self.next_id(), "tasks/get", self.task_params(task_id));
        self.client.post_jsonrpc(&self.endpoint, &request).await
    }

    async fn tasks_cancel(&self, task_id: &str) -> Result<serde_json::Value, A2aError> {
        let request =
            JsonRpcRequest::new(self.next_id(), "tasks/cancel", self.task_params(task_id));
        self.client.post_jsonrpc(&self.endpoint, &request).await
    }
}

/// Typed failure surface of a delegation attempt.
#[derive(Debug)]
#[non_exhaustive]
pub enum DelegationError {
    /// The card resolved no usable JSON-RPC endpoint.
    Endpoint(A2aError),
    /// The peer transport failed or spoke an unrecognized dialect.
    Transport(A2aError),
    /// The peer node could not be materialized in the tree.
    Register(String),
    /// A node lifecycle transition failed or could not be persisted.
    State(String),
    /// The owned A2A driver task failed before producing an outcome.
    Driver(String),
    /// The canonical room-event record could not be made durable.
    Journal(RoomJournalError),
    /// The peer/task terminated as failed/rejected.
    Refused { reason: String },
    /// The peer needs authorization or a decision this caller cannot provide.
    /// Its remote task was cancelled before returning this refusal.
    AuthRequired {
        task_id: String,
        context_id: Option<String>,
    },
    /// The delegation was cancelled by the owner.
    Cancelled,
    /// A continuation was refused before dispatch or node mutation.
    Continuation(String),
}

impl std::fmt::Display for DelegationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Endpoint(error) => write!(f, "no reachable A2A endpoint: {error}"),
            Self::Transport(error) => write!(f, "A2A transport failure: {error}"),
            Self::Register(reason) => write!(f, "could not materialize peer node: {reason}"),
            Self::State(reason) => write!(f, "A2A node state failure: {reason}"),
            Self::Driver(reason) => write!(f, "A2A driver task failure: {reason}"),
            Self::Journal(error) => write!(f, "could not record A2A room event: {error}"),
            Self::Refused { reason } => write!(f, "remote task refused/failed: {reason}"),
            Self::AuthRequired { task_id, .. } => write!(
                f,
                "remote agent requires authorization or a decision on its side that this delegation cannot provide (task {task_id} cancelled)"
            ),
            Self::Continuation(reason) => f.write_str(reason),
            Self::Cancelled => write!(f, "delegation cancelled by owner"),
        }
    }
}
pub(crate) type A2aPeerBindings = Arc<[(A2aPeerSpec, Arc<A2aClientAdapter>)]>;

/// Story 19.21 rule 9 — which recipients of which action this process is still
/// sending.
///
/// The registry holds only [`Weak`](std::sync::Weak) lifetime tokens; the one
/// strong `Arc<()>` of every slot is owned by that recipient's in-flight future
/// ([`SendLiveness`]). A recipient is therefore live exactly while its future
/// exists: settlement drops its token, and a cancelled send drops every
/// unfinished future — and their tokens — by ordinary ownership. Nothing
/// mutates shared state in a `Drop`, so the project-default `tokio` lock is
/// enough. Dead slots are pruned lazily by the next insert or read; an entry's
/// mere presence is ⛔ never evidence of liveness.
type LiveSendActions = Arc<
    tokio::sync::Mutex<std::collections::HashMap<SendActionId, Vec<(u32, std::sync::Weak<()>)>>>,
>;

/// One recipient's in-flight proof. Held by that recipient's future from before
/// its first poll until its settlement was attempted, or until the future is
/// dropped (a cancelled send), whichever comes first.
#[derive(Debug)]
pub(crate) struct SendLiveness {
    _token: Arc<()>,
}

/// The proof that one operator send action was journaled.
/// [`Self::into_recipients`] hands each recipient its [`SendActionRef`] and its
/// [`SendLiveness`] exactly once; the handle keeps no strong owner afterwards.
pub(crate) struct SendActionHandle {
    id: SendActionId,
    tokens: Vec<Arc<()>>,
    stories: Vec<Option<serde_json::Value>>,
}

impl SendActionHandle {
    pub(crate) fn id(&self) -> &SendActionId {
        &self.id
    }

    /// One `(reference, liveness, stories)` tuple per recipient, in operator order.
    /// The reference is what the recipient's dispatch row and settlement carry;
    /// the liveness token must be moved into that recipient's future **before**
    /// it is first polled, so a future dropped unpolled still releases it.
    pub(crate) fn into_recipients(
        self,
    ) -> Vec<(SendActionRef, SendLiveness, Option<serde_json::Value>)> {
        let Self {
            id,
            tokens,
            stories,
        } = self;
        tokens
            .into_iter()
            .zip(stories)
            .enumerate()
            .map(|(index, (token, stories))| {
                (
                    SendActionRef {
                        action: id.clone(),
                        index: u32::try_from(index).unwrap_or(u32::MAX),
                    },
                    SendLiveness { _token: token },
                    stories,
                )
            })
            .collect()
    }
}

/// Freeze the validated manifest against the same roster slots as the action.
/// Only actual pins cross hosts; a sender-local alias pseudonym never does.
/// `you` marks every entry held by this recipient's host: the same alias (a
/// repeated slot) or the same actual pin (two aliases for one host).
fn freeze_stories(
    manifest: Option<&StoryManifest>,
    aliases: &[String],
    pins: &[Option<PeerId>],
) -> (Vec<RecordedStory>, Vec<Option<serde_json::Value>>) {
    let Some(manifest) = manifest else {
        return (Vec::new(), aliases.iter().map(|_| None).collect());
    };
    let recorded: Vec<RecordedStory> = manifest
        .stories
        .iter()
        .map(|story| RecordedStory {
            key: story.key.clone(),
            to: story.to.as_ref().map(|alias| {
                let index = aliases
                    .iter()
                    .position(|recipient| recipient == alias)
                    .expect("validated story member is an addressed recipient");
                u32::try_from(index).expect("recipient index fits u32")
            }),
        })
        .collect();
    let carriers = aliases
        .iter()
        .enumerate()
        .map(|(recipient_index, _)| {
            let stories: Vec<serde_json::Value> = recorded
                .iter()
                .map(|story| {
                    let mut entry = serde_json::json!({ "key": story.key.as_str() });
                    if let Some(index) = story.to {
                        let index = usize::try_from(index).expect("recipient index fits usize");
                        let same_host = aliases[index] == aliases[recipient_index]
                            || pins[index]
                                .as_ref()
                                .is_some_and(|pin| pins[recipient_index].as_ref() == Some(pin));
                        if same_host {
                            entry["you"] = serde_json::Value::Bool(true);
                        } else {
                            entry["alias"] = serde_json::Value::String(aliases[index].clone());
                            if let Some(peer) = &pins[index] {
                                entry["peer"] = serde_json::Value::String(peer.as_str().to_owned());
                            }
                        }
                    }
                    entry
                })
                .collect();
            let mut carrier = serde_json::Map::new();
            carrier.insert("stories".to_owned(), serde_json::Value::Array(stories));
            Some(serde_json::Value::Object(carrier))
        })
        .collect();
    (recorded, carriers)
}

/// Drop every slot whose recipient future is gone, and every action left empty.
fn prune_dead_send_slots(
    live: &mut std::collections::HashMap<SendActionId, Vec<(u32, std::sync::Weak<()>)>>,
) {
    live.retain(|_, slots| {
        slots.retain(|(_, token)| token.strong_count() > 0);
        !slots.is_empty()
    });
}

/// Instructions accompany untrusted peer content; the opaque handle is host-minted.
pub const ANSWER_INSTRUCTIONS: &str = "The remote agent asked for input. Answer by calling this tool again with \"continue\" set to this value and \"message\" set to your answer. The remote task stays open until you answer or this node is cancelled.";

struct Parked {
    peer: String,
    node: AgentId,
    task_id: String,
    context_id: Option<String>,
    transport: Arc<dyn A2aTaskTransport>,
    own_task: Option<OwnTask>,
    resume: oneshot::Sender<ResumeRequest>,
}

struct ResumeRequest {
    node: AgentId,
    transport: Arc<dyn A2aTaskTransport>,
    message: serde_json::Value,
    cancel: CancellationToken,
    authorship: Authorship,
    send_authorization: SendAuthorization,
    reply: oneshot::Sender<Result<serde_json::Value, DelegationError>>,
}

#[derive(Debug)]
enum DriverOutcome {
    Finished(serde_json::Value),
    InputRequired(TaskSnapshot),
}

/// Shared A2A delegation runtime. Injected by the composition root with the
/// live node tree, the durable journal, and the domain event sink.
#[derive(Clone)]
pub struct A2aDelegationRuntime {
    node_tree: NodeTree,
    journal: Arc<dyn RoomJournal>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
    peer_bindings: A2aPeerBindings,
    journal_failure_latch: Arc<JournalFailureLatch>,
    parked: Arc<parking_lot::Mutex<std::collections::HashMap<String, Parked>>>,
    /// Last admitted Act 1 board refresh (Story 19.16b `AC5(d)`).
    ///
    /// Held on the runtime rather than a UI state so the stated poll floor
    /// binds every rail that can reach the egress, not just the one that
    /// happens to render. `now` is injected by the caller, so the bound stays a
    /// deterministic ratchet rather than a timing window.
    board_refresh_gate: Arc<tokio::sync::Mutex<Option<std::time::Instant>>>,
    /// Read side of the same journal, for the board's dispatch-ledger probe
    /// (19.16b `AC3(g)`). `RoomJournal` itself is write-only by design, so
    /// the reader is injected beside it — `None` degrades the board to
    /// uncorrelated rows, never to no rows.
    journal_reader: Option<Arc<dyn crate::domain::ports::RoomJournalReader>>,
    /// Held for the DURATION of one board fan-out (19.16b review). The gate
    /// above rate-limits admission starts; this refuses admission while a
    /// collect is still running, so repeated `/team board` cannot stack
    /// overlapping N-peer read storms whose notices land out of order.
    /// `tokio::sync` because the guard spans remote reads (`.await`).
    board_in_flight: Arc<tokio::sync::Mutex<()>>,
    /// The last board this runtime assembled (Story 19.16f `AC10(d)`): a
    /// landed retract re-renders it with the recipient's mark applied, ⛔
    /// never by a second fan-out (the refresh floor would refuse one, and a
    /// retract is not a board read). Story 19.21 `AC3(f)`: it keeps its
    /// **view identity** — an inventory or one action board — so the re-render
    /// is the view the operator opened, never the other.
    last_board: Arc<tokio::sync::Mutex<Option<super::board::RememberedBoard>>>,
    /// Story 19.18 `A13`/`Q2` — the per-process thread ledger: which peer
    /// reply turns this host already journaled, and which `(peer, task,
    /// content hash)` it dispatched. Seeded ONCE per process from the sender
    /// journal on the first thread read (the story's one new whole-journal
    /// read), then kept current by this runtime's own appends — ⛔ never one
    /// read per call. `tokio::sync` because seeding spans the journal read.
    thread_ledger: Arc<tokio::sync::Mutex<Option<ThreadLedger>>>,
    /// Story 19.18c `PQ1` — this host's item-capability key, loaded by the
    /// composition root and injected through `A2aEgress::compose` (`A3`).
    item_capability: ItemCapabilityKeyState,
    /// Story 19.21 rule 9 — the `(action, recipient index)` pairs THIS process
    /// started and has not yet settled. "Live" is per process: the TUI and the
    /// daemon share one workspace journal, so a recipient with no settlement
    /// that is not live here is `no outcome recorded`, never `sending…`.
    live_send_actions: LiveSendActions,
    /// Story 19.21 `AC1(a)` — every action id this runtime knows is recorded
    /// in the workspace journal, seeded ONCE from the journal and kept current
    /// by this runtime's own mints, so a minted id is unique within it.
    recorded_action_ids: Arc<tokio::sync::Mutex<Option<std::collections::HashSet<SendActionId>>>>,
    /// Story 19.21 `AC3(c)` — the wall clock an action board's age is measured
    /// against: the **injected `now`** (`A14`). Defaults to the system clock;
    /// a test seam supplies a controllable one, never a sleep.
    clock: Arc<dyn crate::domain::clock::Clock>,
}

/// One own task dispatch: peer/origin correlation and the optional human-text
/// hash. Model-rail rows grant task capability but never an own-content label.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OwnDispatch {
    peer: String,
    endpoint: Option<String>,
    content_hash: Option<ContentHash>,
}

/// Story 19.18 — what the asker already knows about its threads.
#[derive(Debug, Default)]
struct ThreadLedger {
    /// `(peer identity, turn id)` of every `PeerReplyObserved` row.
    observed: std::collections::HashSet<(String, String)>,
    /// Story 19.18c `A11` — task → every `DispatchAct::Task` row that sent
    /// it, the model rail's included (it carries no content hash).
    own_tasks: std::collections::HashMap<String, Vec<OwnDispatch>>,
    /// `(peer identity, reply-turn id)` → answered item and the binding
    /// observed at read time. An unpinned peer can later be renamed.
    answers: std::collections::HashMap<(String, String), (String, Option<String>)>,
}

impl ThreadLedger {
    fn from_entries(entries: &[crate::domain::models::JournalEntry]) -> Self {
        let mut ledger = Self::default();
        for entry in entries {
            match &entry.record {
                crate::domain::models::JournalRecord::Room(RoomEvent::PeerReplyObserved {
                    peer,
                    turn,
                    answers,
                    endpoint,
                    ..
                }) => {
                    ledger
                        .observed
                        .insert((peer.as_str().to_owned(), turn.clone()));
                    if let Some(item) = answers {
                        ledger.answers.insert(
                            (peer.as_str().to_owned(), turn.clone()),
                            (item.clone(), endpoint.clone()),
                        );
                    }
                }
                crate::domain::models::JournalRecord::Room(
                    RoomEvent::RemoteEnvelopeDispatched {
                        peer,
                        task: Some(task),
                        act: crate::domain::models::DispatchAct::Task,
                        content_hash,
                        endpoint,
                        ..
                    },
                ) => {
                    ledger.record_own(
                        task.clone(),
                        OwnDispatch {
                            peer: peer.as_str().to_owned(),
                            endpoint: endpoint.clone(),
                            content_hash: *content_hash,
                        },
                    );
                }
                _ => {}
            }
        }
        ledger
    }

    fn record_own(&mut self, task: String, row: OwnDispatch) {
        let rows = self.own_tasks.entry(task).or_default();
        if !rows.contains(&row) {
            rows.push(row);
        }
    }

    fn owns(&self, task: &str, current: &A2aPeerSpec, origin_digest: Option<&str>) -> bool {
        self.own_tasks.get(task).is_some_and(|rows| {
            rows.iter().any(|row| {
                dispatch_row_is_own(&row.peer, row.endpoint.as_deref(), current, origin_digest)
            })
        })
    }
}

/// Story 19.18c `PQ2` (B widened) — whether one dispatch row is this host's
/// own send to alias `current`'s present binding. Shared by the capability
/// resolver and hash-checked own-content labeling.
///
/// - A row with **no** `endpoint` (a pinned send, or a pre-19.18c row that
///   granted nothing): own iff its peer is the alias's current **pin** — a
///   pinned peer survives renames and moves; a replaced or removed pin cuts it.
/// - A row **with** an `endpoint` (an unpinned send): own iff its peer is the
///   alias pseudonym (a move or a pin-add survives) **or** its endpoint is the
///   current origin's digest (a rename survives).
///
/// ⚠ Disclosed: an unpinned alias re-pointed at another host, or reused for
/// another peer, receives this host's capabilities for the old host's items;
/// a rename AND a move together orphan them. A later pin-add does not revoke
/// correlation of historical endpoint-bearing rows for the same alias.
pub(crate) fn dispatch_row_is_own(
    row_peer: &str,
    row_endpoint: Option<&str>,
    current: &A2aPeerSpec,
    current_origin_digest: Option<&str>,
) -> bool {
    match row_endpoint {
        None => current
            .pinned_identity()
            .is_some_and(|pin| pin.as_str() == row_peer),
        Some(endpoint) => {
            current.alias_pseudonym().as_str() == row_peer
                || current_origin_digest == Some(endpoint)
        }
    }
}

/// Story 19.18 — one served turn as the asker's ledger needs it: an item turn
/// is checked against the dispatch set, a reply turn is journaled once.
pub(crate) enum ObservedTurn<'a> {
    Item {
        task: Option<&'a str>,
        content_hash: Option<ContentHash>,
    },
    Reply {
        turn: &'a str,
        answers: Option<&'a str>,
        content: Option<&'a str>,
        content_hash: Option<ContentHash>,
    },
}

/// Serializes the running failure count with its notification. Concurrent
/// failed appends can otherwise deliver `2` before `1` to the display even
/// though the counter itself is atomic.
struct JournalFailureLatch {
    failures: AtomicU64,
    emit_lock: Mutex<()>,
}

impl JournalFailureLatch {
    fn new() -> Self {
        Self {
            failures: AtomicU64::new(0),
            emit_lock: Mutex::new(()),
        }
    }
}

impl A2aDelegationRuntime {
    pub fn new(
        node_tree: NodeTree,
        journal: Arc<dyn RoomJournal>,
        event_tx: mpsc::UnboundedSender<AppEvent>,
    ) -> Self {
        Self {
            node_tree,
            journal,
            event_tx,
            peer_bindings: Arc::from([]),
            journal_failure_latch: Arc::new(JournalFailureLatch::new()),
            parked: Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
            board_refresh_gate: Arc::new(tokio::sync::Mutex::new(None)),
            journal_reader: None,
            board_in_flight: Arc::new(tokio::sync::Mutex::new(())),
            last_board: Arc::new(tokio::sync::Mutex::new(None)),
            thread_ledger: Arc::new(tokio::sync::Mutex::new(None)),
            item_capability: ItemCapabilityKeyState::Disabled,
            live_send_actions: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            recorded_action_ids: Arc::new(tokio::sync::Mutex::new(None)),
            clock: Arc::new(crate::domain::clock::SystemClock::default()),
        }
    }
    /// Supply the journal's read side for the board's dispatch-ledger probe
    /// (19.16b `AC3(g)`). Optional: without it the board degrades to
    /// uncorrelated rows rather than reporting nothing.
    pub(crate) fn with_journal_reader(
        mut self,
        reader: Arc<dyn crate::domain::ports::RoomJournalReader>,
    ) -> Self {
        self.journal_reader = Some(reader);
        self
    }
    pub(crate) fn with_peer_bindings(mut self, peer_bindings: A2aPeerBindings) -> Self {
        self.peer_bindings = peer_bindings;
        self
    }
    /// Story 19.18c `A3` — the key state the composition root loaded. Set
    /// once by `A2aEgress::compose`; ⛔ never read from the filesystem here.
    pub(crate) fn with_item_capability(mut self, state: ItemCapabilityKeyState) -> Self {
        self.item_capability = state;
        self
    }

    /// Story 19.18c `PQ1` — the key an outbound `message/send` to `spec`
    /// carries: `Some` when usable, `None` when disabled (pre-19.18c tests) or
    /// when the key is unusable but the peer is **uncredentialed** (only a
    /// no-credential listener can accept that send, and there every caller is
    /// one principal already, 19.16b `A13`).
    ///
    /// # Errors
    ///
    /// [`A2aError::ItemCapabilityKeyUnusable`] for a credentialed peer while
    /// the key cannot be used: the send is refused, never sent legacy.
    pub(crate) fn outbound_item_key(
        &self,
        spec: &A2aPeerSpec,
    ) -> Result<Option<Arc<ItemCapabilityKey>>, A2aError> {
        match &self.item_capability {
            ItemCapabilityKeyState::Ready(key) => Ok(Some(key.clone())),
            ItemCapabilityKeyState::Disabled => Ok(None),
            ItemCapabilityKeyState::Unusable { path, cause } => {
                if spec.auth.is_some() {
                    Err(A2aError::ItemCapabilityKeyUnusable {
                        path: path.clone(),
                        cause: cause.clone(),
                    })
                } else {
                    Ok(None)
                }
            }
        }
    }

    /// Story 19.18c `PQ2` — the dispatch row's `endpoint` for a send to
    /// `spec`: the roster origin's digest for an **unpinned** peer, `None`
    /// for a pinned one. ⛔ Never the raw URL.
    fn dispatch_endpoint(&self, spec: &A2aPeerSpec) -> Option<String> {
        if spec.pinned_identity().is_some() {
            return None;
        }
        self.peer_binding(&spec.id)
            .map(|(_, client)| client.origin_digest())
    }

    /// The per-process ledger, seeded from the sender journal on first use
    /// (19.18 `A13`). `None` when the journal cannot be read.
    async fn seeded_ledger(&self) -> Option<tokio::sync::MutexGuard<'_, Option<ThreadLedger>>> {
        let mut guard = self.thread_ledger.lock().await;
        if guard.is_none() {
            match self.journal_reader.as_ref() {
                Some(reader) => match reader.load_entries().await {
                    Ok(entries) => *guard = Some(ThreadLedger::from_entries(&entries)),
                    Err(error) => {
                        tracing::warn!(%error, "thread ledger unreadable; turns render as another sender, nothing is journaled and no capability is derived this read");
                        return None;
                    }
                },
                None => *guard = Some(ThreadLedger::default()),
            }
        }
        Some(guard)
    }

    /// Story 19.18c `A9`/`A10` step 3 — the capability of `task` on `spec`'s
    /// host, **only** when this host dispatched that task to that peer (the
    /// `PQ2` own-row rule). `None` when the key is not usable, the task is
    /// not own, or it is not a task this host could have minted. On a miss
    /// the journal is re-read once for this call: a sibling process sharing
    /// the key (the TUI or the daemon, `A3`) may have sent it after the seed.
    pub(crate) async fn own_task_capability(
        &self,
        spec: &A2aPeerSpec,
        task: &str,
    ) -> Option<ItemCapability> {
        let ItemCapabilityKeyState::Ready(key) = &self.item_capability else {
            return None;
        };
        // A dispatch row holds the sanitized task; a served task that
        // sanitization would change is not one this host minted.
        if task.is_empty() || sanitize_disclosable(task, MAX_PEER_ID_BYTES) != task {
            return None;
        }
        let origin_digest = self
            .peer_binding(&spec.id)
            .map(|(_, client)| client.origin_digest());
        let mut guard = self.seeded_ledger().await?;
        let own = guard
            .as_ref()
            .is_some_and(|ledger| ledger.owns(task, spec, origin_digest.as_deref()));
        let own = own || {
            let reader = self.journal_reader.as_ref()?;
            let entries = reader.load_entries().await.ok()?;
            let fresh = ThreadLedger::from_entries(&entries);
            let own = fresh.owns(task, spec, origin_digest.as_deref());
            *guard = Some(fresh);
            own
        };
        own.then(|| derive_item_capability(key, task))
    }

    /// Story 19.18c `A10` step 1 — the item a reply turn `turn` served by
    /// `spec` answers, from this host's `PeerReplyObserved` rows. `None` when
    /// this host never observed the turn (read the thread first).
    pub(crate) async fn answered_item(&self, spec: &A2aPeerSpec, turn: &str) -> Option<String> {
        let origin_digest = self
            .peer_binding(&spec.id)
            .map(|(_, client)| client.origin_digest());
        let current_peer = spec.resolved_identity();
        let mut guard = self.seeded_ledger().await?;
        for attempt in 0..2 {
            let ledger = guard.as_ref()?;
            if let Some((_, (item, _))) =
                ledger.answers.iter().find(|((peer, observed_turn), _)| {
                    peer == current_peer.as_str() && observed_turn == turn
                })
            {
                return Some(item.clone());
            }
            let mut candidates =
                ledger
                    .answers
                    .iter()
                    .filter(|((peer, observed_turn), (_, endpoint))| {
                        observed_turn == turn
                            && dispatch_row_is_own(
                                peer,
                                endpoint.as_deref(),
                                spec,
                                origin_digest.as_deref(),
                            )
                    });
            if let Some((_, (item, _))) = candidates.next() {
                // More than one old binding with this peer-minted turn id is
                // ambiguous. Do not select a HashMap iteration-order winner.
                if candidates.next().is_some() {
                    return None;
                }
                return Some(item.clone());
            }
            if attempt == 0 {
                let entries = self.journal_reader.as_ref()?.load_entries().await.ok()?;
                *guard = Some(ThreadLedger::from_entries(&entries));
            }
        }
        None
    }

    /// Whether any key is usable for resolving a capability (`A10`). Under
    /// `Unusable` thread, follow-up and retract resolution present none.
    pub(crate) fn resolves_capabilities(&self) -> bool {
        matches!(self.item_capability, ItemCapabilityKeyState::Ready(_))
    }

    pub(crate) fn peer_binding(
        &self,
        peer_id: &str,
    ) -> Option<(A2aPeerSpec, Arc<A2aClientAdapter>)> {
        self.peer_bindings
            .iter()
            .find(|(spec, _)| spec.id == peer_id)
            .cloned()
    }

    /// Story 19.22 `D4` — the current roster alias a displaced holder must be
    /// addressed under, or `None` when this send already addresses it or no
    /// binding resolves to it (a removed member cannot be addressed).
    ///
    /// The holder is a recorded recipient, so it resolves exactly as
    /// [`Self::eligible_binding`] resolves one: the 19.18c `PQ2` predicate
    /// ([`dispatch_row_is_own`]) — a pin by that pin, an unpinned record by
    /// alias pseudonym or origin digest — so renaming the holder never lapses
    /// the rule. Of several bindings the one still named as at send time wins.
    pub(crate) fn unaddressed_holder_alias(
        &self,
        holder: &SendActionRecipient,
        recipients: &[String],
    ) -> Option<String> {
        let mut named: Option<&str> = None;
        for (spec, client) in self.peer_bindings.iter() {
            if !dispatch_row_is_own(
                holder.peer.as_str(),
                holder.endpoint.as_deref(),
                spec,
                Some(client.origin_digest().as_str()),
            ) {
                continue;
            }
            if recipients.contains(&spec.id) {
                return None;
            }
            if named.is_none() || spec.id == holder.alias {
                named = Some(&spec.id);
            }
        }
        named.map(str::to_owned)
    }

    /// Story 19.21 `AC2` identity precheck — the **current** roster binding a
    /// recorded recipient resolves to, or `None`.
    ///
    /// A recorded **pin** resolves by that pin even after an alias rename; an
    /// unpinned record applies the 19.18c `PQ2` ownership predicate
    /// ([`dispatch_row_is_own`]) against the binding's origin digest. ⛔ Never a
    /// lookup by alias alone: a reused alias, or a changed pin, must resolve to
    /// nothing rather than to an unrelated peer. Of several eligible bindings
    /// the one still named as at send time wins.
    pub(crate) fn eligible_binding(
        &self,
        recipient: &SendActionRecipient,
    ) -> Option<(A2aPeerSpec, Arc<A2aClientAdapter>)> {
        let mut eligible = self.peer_bindings.iter().filter(|(spec, client)| {
            dispatch_row_is_own(
                recipient.peer.as_str(),
                recipient.endpoint.as_deref(),
                spec,
                Some(client.origin_digest().as_str()),
            )
        });
        let first = eligible.next()?;
        let chosen = std::iter::once(first)
            .chain(eligible)
            .find(|(spec, _)| spec.id == recipient.alias)
            .unwrap_or(first);
        Some(chosen.clone())
    }

    pub(crate) fn story_peer_specs(&self) -> impl Iterator<Item = &A2aPeerSpec> + Clone {
        self.peer_bindings.iter().map(|(spec, _)| spec)
    }

    /// Replace the wall clock an action board's age is measured against.
    #[must_use]
    pub(crate) fn with_clock(mut self, clock: Arc<dyn crate::domain::clock::Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// The injected `now`, in unix seconds.
    pub(crate) fn wall_now_unix_secs(&self) -> i64 {
        self.clock.wall_now_ms().div_euclid(1_000)
    }

    pub(crate) fn known_peer_ids(&self) -> Vec<String> {
        let mut ids = self
            .peer_bindings
            .iter()
            .map(|(spec, _)| spec.id.clone())
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids
    }

    /// Admit one Act 1 board refresh, or refuse it with the time left on the
    /// stated floor (Story 19.16b `AC5(d)`).
    ///
    /// ⛔ **A design bound, never a latency claim.** It says how often this
    /// host is willing to poll its peers; it says nothing about how fast an
    /// acknowledgement arrives, and `NFR64`'s 2 s is not re-armed by it.
    ///
    /// `now` is a parameter, not a `Instant::now()` call inside, so the bound
    /// is proven by a deterministic ratchet rather than a sleep: correct code
    /// refuses `t0 + 100ms` and admits `t0 + floor` with no timing window.
    pub(crate) async fn admit_board_refresh(
        &self,
        now: std::time::Instant,
        floor: std::time::Duration,
    ) -> Result<(), super::board::RefreshTooSoon> {
        let mut last = self.board_refresh_gate.lock().await;
        if let Some(previous) = *last {
            let elapsed = now.saturating_duration_since(previous);
            if elapsed < floor {
                return Err(super::board::RefreshTooSoon {
                    remaining: floor - elapsed,
                });
            }
        }
        *last = Some(now);
        Ok(())
    }

    /// Story 19.17: an addressed send that never reached the dispatch hook.
    pub(crate) async fn journal_send_preflight_refusal(
        &self,
        spec: &A2aPeerSpec,
        error: &str,
    ) -> Result<(), DelegationError> {
        self.emit_room(RoomEvent::RemoteEnvelopeRejected {
            rail: Some(crate::domain::models::PeerRail::A2a),
            peer: spec.resolved_identity(),
            reason: RejectReason::Policy {
                detail: sanitize_disclosable(error, MAX_SUMMARY_BYTES),
            },
            direction: Direction::Outbound,
            task: None,
        })
        .await
    }

    /// Story 19.17 + 19.18 `Q2`: the dispatch row carries the outbound
    /// text's size AND its hash — never the text — so a later thread read
    /// can label a served item turn `you` only when both its task and its
    /// content match what this host sent. Story 19.18c `PQ2`: an unpinned
    /// send also records the roster origin's digest (`endpoint`).
    pub(crate) async fn journal_send_dispatch(
        &self,
        spec: &A2aPeerSpec,
        task: &str,
        text: &str,
        action: Option<&SendActionRef>,
        authorship: crate::domain::models::Authorship,
        send_authorization: crate::domain::models::SendAuthorization,
    ) -> Result<(), DelegationError> {
        let peer = spec.resolved_identity();
        let task = sanitize_disclosable(task, MAX_PEER_ID_BYTES);
        let hash = text_content_hash(text);
        let endpoint = self.dispatch_endpoint(spec);
        self.emit_room(RoomEvent::RemoteEnvelopeDispatched {
            peer: peer.clone(),
            task: Some(task.clone()),
            bytes: text.len(),
            act: crate::domain::models::DispatchAct::Task,
            content_hash: Some(hash),
            endpoint: endpoint.clone(),
            action: action.cloned(),
            authorship,
            send_authorization,
        })
        .await?;
        // Kept current by this path's own appends (`A13`): a send after the
        // ledger was seeded is a `you` source and an own task too. Before
        // seeding, the seed reads this row from the journal.
        if let Some(ledger) = self.thread_ledger.lock().await.as_mut() {
            ledger.record_own(
                task,
                OwnDispatch {
                    peer: peer.as_str().to_owned(),
                    endpoint,
                    content_hash: Some(hash),
                },
            );
        }
        Ok(())
    }

    /// Story 19.18 `AC6(b)`/`A13` — settle one served thread against the
    /// asker's ledger. An item is `you` only when task AND content hash match
    /// this host's dispatch, with exact legacy peer or existing peer/origin
    /// correlation. A task-only match is forgeable. Every unobserved reply
    /// turn is journaled
    /// once as `PeerReplyObserved` (FR95), including across an asker restart
    /// — the ledger is seeded from the journal (`M23`). At most
    /// `MAX_THREAD_TURNS` rows per read (`P12`).
    pub(crate) async fn settle_thread(
        &self,
        spec: &A2aPeerSpec,
        turns: &[ObservedTurn<'_>],
    ) -> Vec<bool> {
        let peer = spec.resolved_identity();
        let origin_digest = self
            .peer_binding(&spec.id)
            .map(|(_, client)| client.origin_digest());
        let endpoint = if spec.pinned_identity().is_some() {
            None
        } else {
            origin_digest.as_deref()
        };
        let Some(mut guard) = self.seeded_ledger().await else {
            return vec![false; turns.len()];
        };
        let Some(ledger) = guard.as_mut() else {
            return vec![false; turns.len()];
        };
        let mut journaled = 0usize;
        let mut mine = Vec::with_capacity(turns.len());
        for turn in turns {
            match turn {
                ObservedTurn::Item { task, content_hash } => {
                    mine.push(match (task, content_hash) {
                        (Some(task), Some(hash)) => {
                            ledger.own_tasks.get(*task).is_some_and(|rows| {
                                rows.iter().any(|row| {
                                    row.content_hash == Some(*hash)
                                        && (row.peer == peer.as_str()
                                            || dispatch_row_is_own(
                                                &row.peer,
                                                row.endpoint.as_deref(),
                                                spec,
                                                origin_digest.as_deref(),
                                            ))
                                })
                            })
                        }
                        _ => false,
                    })
                }
                ObservedTurn::Reply {
                    turn,
                    answers,
                    content,
                    content_hash,
                } => {
                    mine.push(false);
                    let key = (peer.as_str().to_owned(), (*turn).to_owned());
                    if ledger.observed.contains(&key)
                        || journaled
                            >= crate::domain::services::recipient_thread_bounds::MAX_THREAD_TURNS
                    {
                        continue;
                    }
                    let appended = self
                        .emit_room(RoomEvent::PeerReplyObserved {
                            peer: peer.clone(),
                            endpoint: endpoint.map(str::to_owned),
                            turn: (*turn).to_owned(),
                            answers: answers.map(str::to_owned),
                            content: content.map(str::to_owned),
                            content_hash: *content_hash,
                        })
                        .await;
                    if appended.is_ok() {
                        journaled += 1;
                        if let Some(item) = answers {
                            ledger.answers.insert(
                                key.clone(),
                                ((*item).to_owned(), endpoint.map(str::to_owned)),
                            );
                        }
                        ledger.observed.insert(key);
                    }
                }
            }
        }
        mine
    }

    pub(crate) async fn journal_send_accepted(
        &self,
        spec: &A2aPeerSpec,
        task: &str,
        answer: &serde_json::Value,
    ) -> Result<(), DelegationError> {
        self.emit_room(RoomEvent::RemoteEnvelopeAccepted {
            rail: Some(crate::domain::models::PeerRail::A2a),
            peer: spec.resolved_identity(),
            node: mint_node_id(&spec.id, task),
            content_hash: content_hash(answer),
            direction: Direction::Outbound,
            task: Some(sanitize_disclosable(task, MAX_PEER_ID_BYTES)),
        })
        .await
    }

    pub(crate) async fn journal_send_rejected(
        &self,
        spec: &A2aPeerSpec,
        task: &str,
        detail: &str,
    ) -> Result<(), DelegationError> {
        self.emit_room(RoomEvent::RemoteEnvelopeRejected {
            rail: Some(crate::domain::models::PeerRail::A2a),
            peer: spec.resolved_identity(),
            reason: RejectReason::Policy {
                detail: sanitize_disclosable(detail, MAX_SUMMARY_BYTES),
            },
            direction: Direction::Outbound,
            task: Some(sanitize_disclosable(task, MAX_PEER_ID_BYTES)),
        })
        .await
    }

    /// Story 19.21 `AC1(a)` — journal the operator's send action: the first
    /// write of the action, **before** any recipient preflight, dispatch or
    /// POST. Each alias freezes the identity [`Self::journal_send_dispatch`]
    /// resolves (pinned identity or alias pseudonym, plus `PQ2`'s unpinned
    /// origin digest). ⛔ No body bytes: the row carries the hash and the byte
    /// count only, and the `subject` label is sender-local.
    ///
    /// On success every recipient slot has a liveness token (rule 9) that its
    /// recipient future owns until its settlement is attempted or it is dropped.
    ///
    /// # Errors
    ///
    /// An unresolved alias, or the append failure (latched like every other
    /// send-path failure). The caller then does no recipient I/O at all.
    pub(crate) async fn record_send_action(
        &self,
        recipients: &[String],
        text: &str,
        message_type: Option<SemanticMessageType>,
        subject: Option<&str>,
        stories: Option<&StoryManifest>,
    ) -> Result<SendActionHandle, DelegationError> {
        let mut frozen = Vec::with_capacity(recipients.len());
        let mut pins = if stories.is_some() {
            Vec::with_capacity(recipients.len())
        } else {
            Vec::new()
        };
        for alias in recipients {
            let (spec, _) = self.peer_binding(alias).ok_or_else(|| {
                DelegationError::Driver(format!("no roster binding for recipient `{alias}`"))
            })?;
            frozen.push(SendActionRecipient {
                alias: alias.clone(),
                peer: spec.resolved_identity(),
                endpoint: self.dispatch_endpoint(&spec),
            });
            if stories.is_some() {
                pins.push(spec.pinned_identity());
            }
        }
        let (recorded_stories, story_carriers) = freeze_stories(stories, recipients, &pins);
        let action = self.mint_send_action_id().await?;
        self.emit_room(RoomEvent::TeamSendActionRecorded {
            action: action.clone(),
            recipients: frozen,
            message_type: message_type.unwrap_or(SemanticMessageType::Unknown),
            subject: subject.map(str::to_owned),
            body_hash: text_content_hash(text),
            body_bytes: text.len(),
            stories: recorded_stories,
        })
        .await?;
        let tokens: Vec<Arc<()>> = recipients.iter().map(|_| Arc::new(())).collect();
        {
            let mut live = self.live_send_actions.lock().await;
            prune_dead_send_slots(&mut live);
            live.insert(
                action.clone(),
                tokens
                    .iter()
                    .enumerate()
                    .map(|(index, token)| {
                        (
                            u32::try_from(index).unwrap_or(u32::MAX),
                            Arc::downgrade(token),
                        )
                    })
                    .collect(),
            );
        }
        Ok(SendActionHandle {
            id: action,
            tokens,
            stories: story_carriers,
        })
    }

    /// A fresh id, unique among every id this workspace journal records
    /// (`AC1(a)`). The recorded set is seeded once from the journal and kept
    /// current by this runtime's own mints. If the journal cannot be read the
    /// set cannot be established: ⛔ no id is minted and ⛔ nothing is cached,
    /// so the next send reads again. (A runtime composed without a reader is a
    /// test-only shape; `A2aEgress::compose` requires one.)
    async fn mint_send_action_id(&self) -> Result<SendActionId, DelegationError> {
        let mut known = self.recorded_action_ids.lock().await;
        if known.is_none() {
            let mut seeded = std::collections::HashSet::new();
            if let Some(reader) = self.journal_reader.as_ref() {
                let entries = reader.load_entries().await.map_err(|error| {
                    tracing::warn!(%error, "send-action ids unreadable; the action cannot be recorded");
                    DelegationError::Driver(format!("the send journal could not be read: {error}"))
                })?;
                seeded.extend(
                    crate::domain::services::send_action_ledger::fold_send_actions(&entries)
                        .into_iter()
                        .map(|action| action.id),
                );
            }
            *known = Some(seeded);
        }
        let known = known.as_mut().expect("seeded above");
        loop {
            let id = SendActionId::mint();
            if known.insert(id.clone()) {
                return Ok(id);
            }
        }
    }

    /// Story 19.21 `AC1(d)` — journal one recipient's typed settlement, after
    /// every row its delivery path emitted. Durable or not, the recipient's
    /// liveness ends with its [`SendLiveness`] token, which the coordinator
    /// drops as soon as this returns: a failed append leaves the recipient
    /// durably unsettled (reconciled through its dispatch correlation), ⛔ never
    /// rewritten as `NotSent`.
    ///
    /// # Errors
    ///
    /// The append failure, already latched. The caller then does **not**
    /// publish the settle.
    pub(crate) async fn journal_send_settlement(
        &self,
        reference: &SendActionRef,
        settlement: SendSettlement,
    ) -> Result<(), DelegationError> {
        self.emit_room(RoomEvent::TeamSendRecipientSettled {
            action: reference.action.clone(),
            index: reference.index,
            settlement,
        })
        .await
    }

    /// Whether this process is still sending `index` of `action` (rule 9): the
    /// slot's token still has a living owner. Prunes the action's dead slots,
    /// and the action itself once none is left.
    pub(crate) async fn send_recipient_is_live(&self, action: &SendActionId, index: u32) -> bool {
        let mut live = self.live_send_actions.lock().await;
        let Some(slots) = live.get_mut(action) else {
            return false;
        };
        slots.retain(|(_, token)| token.strong_count() > 0);
        let is_live = slots.iter().any(|(slot, _)| *slot == index);
        if slots.is_empty() {
            live.remove(action);
        }
        is_live
    }

    /// How many `(action, slot)` entries the live-send registry still holds,
    /// dead or alive — the pruning oracle's observation point. ⛔ Never a
    /// liveness answer: [`Self::send_recipient_is_live`] is.
    #[cfg(test)]
    pub(crate) async fn live_send_slot_entries(&self) -> usize {
        self.live_send_actions
            .lock()
            .await
            .values()
            .map(Vec::len)
            .sum()
    }

    /// This workspace journal's recorded send actions, **oldest first**, or
    /// `None` when no reader is injected or the journal cannot be read. ⛔ Never
    /// this host's inbound projection: the action board answers for what this
    /// host sent.
    pub(crate) async fn send_action_ledger(
        &self,
    ) -> Option<Vec<crate::domain::services::send_action_ledger::SendActionView>> {
        let reader = self.journal_reader.as_ref()?;
        match reader.load_entries().await {
            Ok(entries) => {
                Some(crate::domain::services::send_action_ledger::fold_send_actions(&entries))
            }
            Err(error) => {
                tracing::warn!(%error, "send-action ledger unreadable; no action board can be shown");
                None
            }
        }
    }

    /// Read the sender's current holder statements once, never an inbound
    /// projection. A read failure must not permit a silent displacement.
    pub(crate) async fn story_holders(
        &self,
    ) -> Result<
        std::collections::BTreeMap<
            crate::domain::models::StoryKey,
            Option<crate::domain::models::SendActionRecipient>,
        >,
        crate::domain::ports::RoomJournalError,
    > {
        self.story_selection_entries()
            .await
            .map(|entries| crate::domain::services::send_action_ledger::story_holders(&entries))
    }

    /// A single snapshot for both received and own holder statements.
    pub(crate) async fn story_selection_entries(
        &self,
    ) -> Result<Vec<crate::domain::models::JournalEntry>, crate::domain::ports::RoomJournalError>
    {
        let reader = self.journal_reader.as_ref().ok_or_else(|| {
            crate::domain::ports::RoomJournalError::Read(
                "room journal reader is unavailable".to_owned(),
            )
        })?;
        reader.load_entries().await
    }

    /// Remember the board just assembled (Story 19.16f `AC10(d)`).
    pub(crate) async fn remember_board(&self, board: super::board::RememberedBoard) {
        *self.last_board.lock().await = Some(board);
    }

    /// Apply a landed retract's mark to the remembered board and return that
    /// same view re-rendered, or `None` when no board was assembled this
    /// session or the item is not on it (then nothing re-renders). Story 19.21
    /// `AC3(f)`: an inventory re-renders as the inventory, an action board as
    /// that action's board.
    pub(crate) async fn mark_board_item_retracted(
        &self,
        peer: &str,
        item_id: &str,
        retracted_at_ms: i64,
        now_unix_secs: i64,
    ) -> Option<String> {
        let mut last = self.last_board.lock().await;
        let board = last.as_mut()?;
        board
            .apply_retract_mark(peer, item_id, retracted_at_ms)
            .then(|| board.render(now_unix_secs))
    }

    /// Journal one item-retract dispatch (Story 19.16f `AC5`, owner gate item
    /// 4 = B): the shipped `RemoteEnvelopeDispatched` with its additive `act`
    /// discriminator, durable **before** the POST. `item` is peer-minted text,
    /// stripped on write. Goes through `emit_room`, so a journal failure is
    /// latched exactly as the send path latches it.
    ///
    /// Story 19.21 `A15`: **returns the append result** — a retract whose
    /// dispatch row could not be made durable must not POST (`AD-1803`).
    pub(crate) async fn journal_item_retract_dispatch(
        &self,
        spec: &A2aPeerSpec,
        task: Option<&str>,
        item: &str,
        bytes: usize,
    ) -> Result<(), DelegationError> {
        self.emit_room(RoomEvent::RemoteEnvelopeDispatched {
            peer: spec.resolved_identity(),
            task: task.map(|task| sanitize_disclosable(task, MAX_PEER_ID_BYTES)),
            bytes,
            act: crate::domain::models::DispatchAct::ItemRetract {
                item: sanitize_disclosable(item, MAX_PEER_ID_BYTES),
            },
            content_hash: None,
            endpoint: None,
            action: None,
            authorship: crate::domain::models::Authorship::Unknown,
            send_authorization: crate::domain::models::SendAuthorization::HumanDecided,
        })
        .await
    }

    /// Journal a retract whose outcome PROVES nothing was marked (Story
    /// 19.16f `AC5(a)`). ⛔ Never called for a landed, already-marked or
    /// unknown outcome: a rejection for a write that may have landed is a
    /// false claim.
    pub(crate) async fn journal_item_retract_refusal(
        &self,
        spec: &A2aPeerSpec,
        task: Option<&str>,
        detail: &str,
    ) {
        let _ = self
            .emit_room(RoomEvent::RemoteEnvelopeRejected {
                rail: Some(crate::domain::models::PeerRail::A2a),
                peer: spec.resolved_identity(),
                reason: RejectReason::Policy {
                    detail: sanitize_disclosable(detail, MAX_SUMMARY_BYTES),
                },
                direction: Direction::Outbound,
                task: task.map(|task| sanitize_disclosable(task, MAX_PEER_ID_BYTES)),
            })
            .await;
    }

    /// Claim the board fan-out slot, or fail when one is already running
    /// (19.16b review). The returned guard is held for the whole collect and
    /// dropped when it finishes, so a second `/team board` issued while the
    /// first still reads is refused with the same `RefreshTooSoon` shape the
    /// poll floor uses — ⛔ never stacked, never silently queued.
    pub(crate) fn begin_board_collect(
        &self,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, super::board::RefreshTooSoon> {
        Arc::clone(&self.board_in_flight)
            .try_lock_owned()
            .map_err(|_| super::board::RefreshTooSoon {
                remaining: super::board::board_refresh_floor(),
            })
    }

    /// The sender-side dispatch ledger, read once per board refresh: peer →
    /// the task ids this host durably dispatched (`RemoteEnvelopeDispatched`,
    /// journaled before the POST, keyed by the peer's resolved identity).
    /// This is the correlation half of Story 19.16b `AC3(g)` — the board's
    /// rows answer for **this sender's** sends, which is the only truthful
    /// attribution under a collapsed principal (where the peer's set mixes
    /// every local caller's items).
    ///
    /// `None` means no reader is injected or the ledger could not be read:
    /// the caller must then degrade to the uncorrelated read rather than
    /// report nothing.
    pub(crate) async fn dispatched_tasks_by_peer(
        &self,
    ) -> Option<std::collections::HashMap<String, std::collections::HashSet<String>>> {
        use std::collections::{HashMap, HashSet};

        let reader = self.journal_reader.as_ref()?;
        let entries = match reader.load_entries().await {
            Ok(entries) => entries,
            Err(error) => {
                tracing::warn!(%error, "dispatch ledger unreadable; board degrades to uncorrelated rows");
                return None;
            }
        };
        let mut dispatched: HashMap<String, HashSet<String>> = HashMap::new();
        for entry in &entries {
            if let crate::domain::models::JournalRecord::Room(
                crate::domain::models::RoomEvent::RemoteEnvelopeDispatched {
                    peer,
                    task: Some(task),
                    // Story 19.16f `AC5(b)`: a retract row carries the item's
                    // task but dispatched no task — it must not widen the
                    // board's correlation set.
                    act: crate::domain::models::DispatchAct::Task,
                    ..
                },
            ) = &entry.record
            {
                dispatched
                    .entry(peer.as_str().to_owned())
                    .or_default()
                    .insert(task.clone());
            }
        }
        Some(dispatched)
    }

    /// Delegate one task to a discovered peer and drive it to terminal.
    ///
    /// `trust` is admission-only (R-D): it is stamped into observability but
    /// never decides content handling. On success the returned value is the raw
    /// remote task result; the peer node is left in a terminal `NodeState` for
    /// the panel.
    pub async fn delegate(
        &self,
        spec: &A2aPeerSpec,
        trust: TrustTier,
        parent_tool_call_id: &str,
        transport: Arc<dyn A2aTaskTransport>,
        message: serde_json::Value,
        cancel: CancellationToken,
        authorship: Authorship,
        send_authorization: crate::domain::models::SendAuthorization,
    ) -> Result<serde_json::Value, DelegationError> {
        let runtime = self.clone();
        let spec = spec.clone();
        let parent_tool_call_id = parent_tool_call_id.to_owned();
        tokio::spawn(async move {
            runtime
                .delegate_inner(
                    spec,
                    trust,
                    parent_tool_call_id,
                    transport,
                    message,
                    cancel,
                    authorship,
                    send_authorization,
                )
                .await
        })
        .await
        .map_err(|error| DelegationError::Driver(error.to_string()))?
    }

    async fn delegate_inner(
        &self,
        spec: A2aPeerSpec,
        trust: TrustTier,
        parent_tool_call_id: String,
        transport: Arc<dyn A2aTaskTransport>,
        message: serde_json::Value,
        cancel: CancellationToken,
        authorship: Authorship,
        send_authorization: crate::domain::models::SendAuthorization,
    ) -> Result<serde_json::Value, DelegationError> {
        let peer = spec.resolved_identity();
        // Admission signal only — stamped into the dispatch log, never used to
        // branch on content (R-D).
        tracing::info!(peer = %spec.id, ?trust, "dispatching A2A delegation");

        let (submitted_task, submitted_bytes) = outbound_message_fact(&message);
        let endpoint = self.dispatch_endpoint(&spec);
        self.emit_room(RoomEvent::RemoteEnvelopeDispatched {
            peer: peer.clone(),
            task: submitted_task.clone(),
            bytes: submitted_bytes,
            act: crate::domain::models::DispatchAct::Task,
            // The model rail carries no content hash (19.17 `AC3(e)`): no
            // `you` label reads its rows. Story 19.18c: they are own tasks.
            content_hash: None,
            endpoint: endpoint.clone(),
            // The model rail records no operator send action (19.21 `AC1(i)`).
            action: None,
            authorship,
            send_authorization,
        })
        .await?;
        if let Some(task) = submitted_task.clone()
            && let Some(ledger) = self.thread_ledger.lock().await.as_mut()
        {
            ledger.record_own(
                task,
                OwnDispatch {
                    peer: peer.as_str().to_owned(),
                    endpoint,
                    content_hash: None,
                },
            );
        }

        // The submit fact is durable before the POST. A first-hop transport
        // failure has no node yet, so journal the existing rejection vocabulary
        // directly rather than losing the attempt through `reject()`. The
        // detail carries remote-influenced content (AC8): sanitize exactly as
        // `reject()` does for the post-node arms.
        let first_value = match transport.message_send(message).await {
            Ok(value) => value,
            Err(error) => {
                self.emit_room(RoomEvent::RemoteEnvelopeRejected {
                    rail: Some(crate::domain::models::PeerRail::A2a),
                    peer,
                    reason: RejectReason::Policy {
                        detail: sanitize_disclosable(
                            &format!("A2A transport failure: {error}"),
                            MAX_SUMMARY_BYTES,
                        ),
                    },
                    direction: Direction::Outbound,
                    task: submitted_task,
                })
                .await?;
                return Err(DelegationError::Transport(error));
            }
        };
        let first = match TaskSnapshot::from_result(first_value) {
            Ok(first) => first,
            Err(error) => {
                self.emit_room(RoomEvent::RemoteEnvelopeRejected {
                    rail: Some(crate::domain::models::PeerRail::A2a),
                    peer,
                    reason: RejectReason::Policy {
                        detail: sanitize_disclosable(
                            &format!("A2A transport failure: {error}"),
                            MAX_SUMMARY_BYTES,
                        ),
                    },
                    direction: Direction::Outbound,
                    task: submitted_task,
                })
                .await?;
                return Err(DelegationError::Transport(error));
            }
        };
        let raw_task_id = first.id.clone();
        if raw_task_id.len() > MAX_PEER_ID_BYTES {
            let reason = "remote task id exceeds the supported size".to_owned();
            self.emit_room(RoomEvent::RemoteEnvelopeRejected {
                rail: Some(crate::domain::models::PeerRail::A2a),
                peer,
                reason: RejectReason::Policy {
                    detail: reason.clone(),
                },
                direction: Direction::Outbound,
                task: Some(disclosable_task_id(&raw_task_id)),
            })
            .await?;
            // The raw id never enters a node id or a room event, but it remains
            // available for wire cleanup. A remote non-terminal task must not
            // be stranded merely because its identifier is too large to retain.
            if !matches!(
                first.state,
                A2aTaskState::Completed
                    | A2aTaskState::Failed
                    | A2aTaskState::Canceled
                    | A2aTaskState::Rejected
            ) {
                transport
                    .tasks_cancel(&raw_task_id)
                    .await
                    .map_err(DelegationError::Transport)?;
            }
            return Err(DelegationError::Refused { reason });
        }
        let node_id = mint_node_id(&spec.id, &raw_task_id);
        tracing::info!(node = %node_id, ?trust, "A2A task dispatched");

        let (node_cancel, command_rx) = self.materialize(&node_id).await?;
        self.node_tree
            .try_set_state(&node_id, NodeState::Running)
            .await
            .map_err(|error| DelegationError::State(error.to_string()))?;

        let (reply, answer) = oneshot::channel();
        let runtime = self.clone();
        tokio::spawn(async move {
            runtime
                .run_live_driver(
                    spec,
                    node_id,
                    peer,
                    parent_tool_call_id,
                    transport,
                    first,
                    cancel,
                    node_cancel,
                    command_rx,
                    reply,
                )
                .await;
        });
        answer
            .await
            .map_err(|error| DelegationError::Driver(error.to_string()))?
    }

    /// Re-drive a peer node that a restart recovered as `Suspended` (Task 8 /
    /// AC5). The node id carries the peer + remote task id, so reconciliation
    /// re-issues `tasks/get` against the peer — NOT memory — and drives
    /// `Suspended -> Running -> terminal`. An unreachable peer leaves the node
    /// `Suspended`, never speculatively `Running` (NFR70(c)).
    pub async fn reconcile_suspended(
        &self,
        spec: &A2aPeerSpec,
        node_id: &AgentId,
        transport: &dyn A2aTaskTransport,
        cancel: CancellationToken,
    ) -> Result<(), DelegationError> {
        let Some((_peer_str, task_id)) = parse_a2a_node_id(node_id) else {
            return Ok(());
        };
        let peer = spec.resolved_identity();
        let value = match transport.tasks_get(&task_id).await {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(
                    node = %node_id,
                    %error,
                    "A2A peer unreachable on restart; leaving node Suspended"
                );
                return Ok(());
            }
        };
        let snapshot = TaskSnapshot::from_result(value).map_err(DelegationError::Transport)?;
        // Suspended -> Running (legal), then drive to terminal via the shared loop.
        self.node_tree
            .try_set_state(node_id, NodeState::Running)
            .await
            .map_err(|error| DelegationError::State(error.to_string()))?;
        match self
            .run_lifecycle(
                node_id,
                &peer,
                "restart-reconcile",
                transport,
                snapshot,
                cancel,
            )
            .await?
        {
            DriverOutcome::Finished(_) => Ok(()),
            DriverOutcome::InputRequired(task) => {
                // Recovered tasks have no live model call to receive a handle, and
                // continuation durability across restart is out of scope. Settle it
                // like a parked kill: journal whether the peer confirmed the cancel.
                self.cancel_parked(node_id, &peer, "restart-reconcile", transport, &task.id)
                    .await?;
                Err(DelegationError::Refused {
                    reason: "remote agent requested input during restart reconciliation; \
                             no live caller can answer it"
                        .to_owned(),
                })
            }
        }
    }

    /// One process-owned session retains the node control plane across every park.
    /// Only the active call observes its invocation token; parked questions survive it.
    async fn run_live_driver(
        &self,
        spec: A2aPeerSpec,
        node_id: AgentId,
        peer: PeerId,
        parent_tool_call_id: String,
        transport: Arc<dyn A2aTaskTransport>,
        mut first: TaskSnapshot,
        mut owner_cancel: CancellationToken,
        node_cancel: CancellationToken,
        mut command_rx: mpsc::Receiver<Op>,
        mut reply: oneshot::Sender<Result<serde_json::Value, DelegationError>>,
    ) {
        let mut commands_open = true;
        // The node token or a bare `Op::Kill` ends the session. A kill consumed
        // while another branch wins is remembered here, never dropped.
        let killed = node_cancel.child_token();
        loop {
            let lifecycle_cancel = CancellationToken::new();
            let outcome = {
                let drive = self.run_lifecycle(
                    &node_id,
                    &peer,
                    &parent_tool_call_id,
                    transport.as_ref(),
                    first,
                    lifecycle_cancel.clone(),
                );
                tokio::pin!(drive);
                loop {
                    tokio::select! {
                        result = &mut drive => break result,
                        () = owner_cancel.cancelled(), if !lifecycle_cancel.is_cancelled() => lifecycle_cancel.cancel(),
                        () = killed.cancelled(), if !lifecycle_cancel.is_cancelled() => lifecycle_cancel.cancel(),
                        command = command_rx.recv(), if commands_open => {
                            match command {
                                Some(Op::Kill) => {
                                    killed.cancel();
                                    lifecycle_cancel.cancel();
                                }
                                None => commands_open = false,
                                _ => {}
                            }
                        }
                    }
                }
            };
            let task = match outcome {
                Ok(DriverOutcome::Finished(value)) => {
                    let _ = reply.send(Ok(value));
                    return;
                }
                Err(error) => {
                    let _ = reply.send(Err(error));
                    return;
                }
                Ok(DriverOutcome::InputRequired(task)) => task,
            };
            // A question that races the active call's cancellation or a kill is
            // not parked: nobody would hold its handle.
            if lifecycle_cancel.is_cancelled() {
                let result = self
                    .cancel_parked(
                        &node_id,
                        &peer,
                        &parent_tool_call_id,
                        transport.as_ref(),
                        &task.id,
                    )
                    .await
                    .and(Err(DelegationError::Cancelled));
                let _ = reply.send(result);
                return;
            }
            let parked = async {
                self.node_tree
                    .try_set_state(&node_id, NodeState::Waiting)
                    .await
                    .map_err(|error| DelegationError::State(error.to_string()))?;
                self.node_tree
                    .stamp_wait_reason(
                        &node_id,
                        Some(crate::domain::models::WaitReason::AwaitingHumanInput),
                    )
                    .await
                    .map_err(|error| DelegationError::State(error.to_string()))?;
                self.node_tree.mark_tainted(&node_id).await;
                let (resume, request) = oneshot::channel();
                let handle = {
                    use rand_core::RngCore;
                    let mut registry = self.parked.lock();
                    loop {
                        let mut bytes = [0u8; 8];
                        rand_core::OsRng.fill_bytes(&mut bytes);
                        let handle = format!("a2a-continue-{}", hex::encode(bytes));
                        if !registry.contains_key(&handle) {
                            registry.insert(
                                handle.clone(),
                                Parked {
                                    peer: spec.id.clone(),
                                    node: node_id.clone(),
                                    task_id: task.id.clone(),
                                    context_id: task.context_id.clone(),
                                    transport: transport.clone(),
                                    own_task: transport.own_task(),
                                    resume,
                                },
                            );
                            break handle;
                        }
                    }
                };
                Ok::<_, DelegationError>((handle, request))
            }
            .await;
            let (handle, request) = match parked {
                Ok(parked) => parked,
                Err(error) => {
                    // An unparked question has no owner: settle it like a kill.
                    let error = if killed.is_cancelled() {
                        DelegationError::Cancelled
                    } else {
                        error
                    };
                    let result = self
                        .cancel_parked(
                            &node_id,
                            &peer,
                            &parent_tool_call_id,
                            transport.as_ref(),
                            &task.id,
                        )
                        .await
                        .and(Err(error));
                    let _ = reply.send(result);
                    return;
                }
            };
            let question = serde_json::json!({
                "state": "input-required", "continue": handle,
                "instructions": ANSWER_INSTRUCTIONS, "task": task.result,
            });
            if reply.send(Ok(question)).is_err() {
                // The caller is gone and can never answer this handle.
                self.parked.lock().remove(&handle);
                if let Err(error) = self
                    .cancel_parked(
                        &node_id,
                        &peer,
                        &parent_tool_call_id,
                        transport.as_ref(),
                        &task.id,
                    )
                    .await
                {
                    tracing::error!(%error, node = %node_id, "settling an undelivered A2A question failed");
                }
                return;
            }
            let Some(request) = self
                .await_parked_resume(
                    &handle,
                    &node_id,
                    &peer,
                    &parent_tool_call_id,
                    transport.as_ref(),
                    &task.id,
                    &killed,
                    &mut command_rx,
                    &mut commands_open,
                    request,
                )
                .await
            else {
                return;
            };
            let ResumeRequest {
                node: answer_node,
                transport: answer_transport,
                message,
                cancel,
                authorship,
                send_authorization,
                reply: answer_reply,
            } = request;
            owner_cancel = cancel;
            reply = answer_reply;
            if killed.is_cancelled() {
                let result = self
                    .cancel_parked(
                        &node_id,
                        &peer,
                        &parent_tool_call_id,
                        transport.as_ref(),
                        &task.id,
                    )
                    .await
                    .and(Err(DelegationError::Cancelled));
                let _ = reply.send(result);
                return;
            }
            let dispatched = async {
                let (submitted_task, bytes) = outbound_message_fact(&message);
                self.emit_room(RoomEvent::RemoteEnvelopeDispatched {
                    peer: peer.clone(),
                    task: submitted_task,
                    bytes,
                    act: crate::domain::models::DispatchAct::Task,
                    content_hash: None,
                    endpoint: self.dispatch_endpoint(&spec),
                    action: None,
                    authorship,
                    send_authorization,
                })
                .await?;
                self.node_tree
                    .try_set_state(&answer_node, NodeState::Running)
                    .await
                    .map_err(|error| DelegationError::State(error.to_string()))
            }
            .await;
            if let Err(error) = dispatched {
                // Nothing was POSTed and the node never left Waiting (a racing
                // kill may have made it Cancelled): the question is still open.
                let error = if killed.is_cancelled() {
                    DelegationError::Cancelled
                } else {
                    error
                };
                let result = self
                    .cancel_parked(
                        &node_id,
                        &peer,
                        &parent_tool_call_id,
                        transport.as_ref(),
                        &task.id,
                    )
                    .await
                    .and(Err(error));
                let _ = reply.send(result);
                return;
            }
            let next = async {
                let value = answer_transport
                    .message_send(message)
                    .await
                    .map_err(DelegationError::Transport)?;
                TaskSnapshot::from_result(value).map_err(DelegationError::Transport)
            }
            .await;
            match next {
                Ok(snapshot) => first = snapshot,
                Err(error) => {
                    if killed.is_cancelled() {
                        let result = self
                            .cancel_parked(
                                &node_id,
                                &peer,
                                &parent_tool_call_id,
                                transport.as_ref(),
                                &task.id,
                            )
                            .await
                            .and(Err(DelegationError::Cancelled));
                        let _ = reply.send(result);
                        return;
                    }
                    let rejected = self
                        .reject(
                            &node_id,
                            &peer,
                            &parent_tool_call_id,
                            &task.id,
                            &error.to_string(),
                        )
                        .await;
                    let _ = reply.send(Err(rejected.err().unwrap_or(error)));
                    return;
                }
            }
        }
    }

    async fn await_parked_resume(
        &self,
        handle: &str,
        node: &AgentId,
        peer: &PeerId,
        parent_tool_call_id: &str,
        transport: &dyn A2aTaskTransport,
        task_id: &str,
        killed: &CancellationToken,
        command_rx: &mut mpsc::Receiver<Op>,
        commands_open: &mut bool,
        mut request: oneshot::Receiver<ResumeRequest>,
    ) -> Option<ResumeRequest> {
        let claimed = loop {
            tokio::select! {
                result = &mut request => break result.ok(),
                () = killed.cancelled() => {}
                command = command_rx.recv(), if *commands_open => {
                    match command {
                        // Remembered: a resume that already claimed the handle
                        // still sees this kill before it dispatches.
                        Some(Op::Kill) => killed.cancel(),
                        None => { *commands_open = false; continue; }
                        _ => continue,
                    }
                }
            }
            // Removal is the resume/kill linearization point. A claimed resume
            // still delivers its request; the caller settles a remembered kill.
            break if self.parked.lock().remove(handle).is_some() {
                None
            } else {
                (&mut request).await.ok()
            };
        };
        if claimed.is_none() {
            // Killed, or the claimed sender vanished: nobody can answer anymore.
            self.parked.lock().remove(handle);
            if let Err(error) = self
                .cancel_parked(node, peer, parent_tool_call_id, transport, task_id)
                .await
            {
                tracing::error!(%error, %node, "settling parked A2A cancellation failed");
            }
        }
        claimed
    }

    async fn cancel_parked(
        &self,
        node: &AgentId,
        peer: &PeerId,
        parent: &str,
        transport: &dyn A2aTaskTransport,
        task_id: &str,
    ) -> Result<(), DelegationError> {
        // A cascade_kill may already have checkpointed Cancelled; never move a
        // terminal node. A lost race must not skip the remote cancel or its row.
        let live = self.node_tree.status_rx(node).await.is_some_and(|status| {
            matches!(*status.borrow(), NodeState::Waiting | NodeState::Running)
        });
        if live
            && let Err(error) = self
                .node_tree
                .try_set_state(node, NodeState::Cancelled)
                .await
        {
            tracing::warn!(%error, %node, "parked A2A node did not move to Cancelled");
        }
        let detail = match transport
            .tasks_cancel(task_id)
            .await
            .and_then(TaskSnapshot::from_result)
        {
            Ok(task) if task.state == A2aTaskState::Canceled => {
                "peer reported terminal state canceled".to_owned()
            }
            Ok(task) => format!(
                "remote task cancel was not confirmed: peer returned {:?}",
                task.state
            ),
            Err(error) => format!("remote task cancel was not confirmed: {error}"),
        };
        let task_correlation = disclosable_task_id(task_id);
        self.emit_room(RoomEvent::RemoteEnvelopeRejected {
            rail: Some(crate::domain::models::PeerRail::A2a),
            peer: peer.clone(),
            reason: RejectReason::Policy {
                detail: sanitize_disclosable(&detail, MAX_SUMMARY_BYTES),
            },
            direction: Direction::Outbound,
            task: Some(task_correlation.clone()),
        })
        .await?;
        self.emit_refused(node, parent, &task_correlation, RefuseReason::Policy);
        Ok(())
    }

    #[cfg(any(test, feature = "test-instrumentation"))]
    #[must_use]
    pub fn parked_count(&self) -> usize {
        self.parked.lock().len()
    }

    /// Claim a same-peer handle once, before journal or transport effects.
    pub async fn resume(
        &self,
        spec: &A2aPeerSpec,
        mut input: serde_json::Value,
        cancel: CancellationToken,
        authorship: Authorship,
        send_authorization: SendAuthorization,
    ) -> Result<serde_json::Value, DelegationError> {
        let invalid = |text: &str| DelegationError::Continuation(text.to_owned());
        let handle = input
            .get("continue")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| invalid("continue must be a non-empty string"))?;
        input
            .get("message")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| invalid("continuing a remote task needs a message"))?;
        input
            .as_object_mut()
            .expect("validated object arguments")
            .remove("continue");
        // Every fallible step runs before the claim: once the entry is removed,
        // the only step left is handing the request to the parked owner.
        let (parked, message) = {
            let mut registry = self.parked.lock();
            let entry = registry
                .get(&handle)
                .ok_or_else(|| invalid("continuation handle is unknown or already used"))?;
            if entry.peer != spec.id {
                return Err(invalid("continuation handle belongs to another peer"));
            }
            let item_key = self
                .outbound_item_key(spec)
                .map_err(DelegationError::Transport)?;
            let reply_capability = entry
                .own_task
                .as_ref()
                .filter(|own| own.message_id == entry.task_id)
                .map(|own| &own.capability);
            let (message, _) = build_message(
                &input,
                MessageCarriers {
                    continuation: Some(Continuation {
                        task_id: &entry.task_id,
                        context_id: entry.context_id.as_deref(),
                    }),
                    reply_to: None,
                    item_key: item_key.as_deref(),
                    reply_capability,
                    stories: None,
                    selected_for: None,
                    authorship,
                    send_authorization,
                },
            );
            let parked = registry.remove(&handle).expect("same-lock checked handle");
            (parked, message)
        };
        let (reply, answer) = oneshot::channel();
        parked
            .resume
            .send(ResumeRequest {
                node: parked.node,
                transport: parked.transport,
                message,
                cancel,
                authorship,
                send_authorization,
                reply,
            })
            .map_err(|_| DelegationError::Driver("parked A2A owner closed".to_owned()))?;
        answer
            .await
            .map_err(|error| DelegationError::Driver(error.to_string()))?
    }

    /// Shared drive-to-terminal loop: live `NodeState` projection + poll +
    /// outcome handling. Used by both fresh delegation and restart reconciliation.
    async fn run_lifecycle(
        &self,
        node_id: &AgentId,
        peer: &PeerId,
        parent_tool_call_id: &str,
        transport: &dyn A2aTaskTransport,
        first: TaskSnapshot,
        cancel: CancellationToken,
    ) -> Result<DriverOutcome, DelegationError> {
        // Live NodeState projection: the poll loop's sync callback forwards each
        // legal RAP hop; a concurrent task applies it via `try_set_state`.
        let (proj_tx, mut proj_rx) = mpsc::unbounded_channel::<RapTaskState>();
        let proj_tree = self.node_tree.clone();
        let proj_node = node_id.clone();
        let projector = tokio::spawn(async move {
            while let Some(rap) = proj_rx.recv().await {
                if let Some(state) = project_rap_to_node_state(rap) {
                    proj_tree
                        .try_set_state(&proj_node, state)
                        .await
                        .map_err(|error| error.to_string())?;
                }
            }
            Ok::<(), String>(())
        });

        let dispatched_task_id = first.id.clone();
        let config = PollConfig::default();
        let outcome = poll_from_snapshot(transport, first, &config, &cancel, |rap| {
            let _ = proj_tx.send(rap);
        })
        .await;
        drop(proj_tx);
        projector
            .await
            .map_err(|error| DelegationError::Driver(error.to_string()))?
            .map_err(DelegationError::State)?;

        match outcome {
            Ok(LifecycleOutcome::Terminal { state, task }) => match state {
                RapTaskState::Completed => {
                    // Remote content enters local context: taint it (never trust a
                    // peer's answer) and record the accepted envelope durably.
                    self.emit_room(RoomEvent::RemoteEnvelopeAccepted {
                        rail: Some(crate::domain::models::PeerRail::A2a),
                        peer: peer.clone(),
                        node: node_id.clone(),
                        content_hash: content_hash(&task.result),
                        direction: Direction::Outbound,
                        task: Some(disclosable_task_id(&task.id)),
                    })
                    .await?;
                    self.node_tree.mark_tainted(node_id).await;
                    Ok(DriverOutcome::Finished(task.result))
                }
                RapTaskState::Canceled => {
                    // A cancellation is a terminal outcome: journal it like
                    // every other terminal state so the outbound `dispatched`
                    // row is never orphaned (AC2 — attempt AND outcome). The
                    // node is already `Cancelled` (the projector's own
                    // terminal for this state — owner-kill and peer-report
                    // both land here), so unlike `reject()` we must not force
                    // `Failed`: `Cancelled -> Failed` is an illegal FSM hop.
                    let task_correlation = disclosable_task_id(&task.id);
                    self.emit_room(RoomEvent::RemoteEnvelopeRejected {
                        rail: Some(crate::domain::models::PeerRail::A2a),
                        peer: peer.clone(),
                        reason: RejectReason::Policy {
                            detail: sanitize_disclosable(
                                "peer reported terminal state canceled",
                                MAX_SUMMARY_BYTES,
                            ),
                        },
                        direction: Direction::Outbound,
                        task: Some(task_correlation.clone()),
                    })
                    .await?;
                    self.emit_refused(
                        node_id,
                        parent_tool_call_id,
                        &task_correlation,
                        RefuseReason::Policy,
                    );
                    Err(DelegationError::Cancelled)
                }
                _ => {
                    let peer_reason = task
                        .result
                        .pointer("/status/message/parts")
                        .and_then(serde_json::Value::as_array)
                        .and_then(|parts| {
                            parts.iter().find_map(|part| {
                                part.get("text").and_then(serde_json::Value::as_str)
                            })
                        });
                    let reason = match peer_reason {
                        Some(detail) => {
                            format!("peer reported terminal state {}: {detail}", state.as_str())
                        }
                        None => format!("peer reported terminal state {}", state.as_str()),
                    };
                    self.reject(node_id, peer, parent_tool_call_id, &task.id, &reason)
                        .await?;
                    Err(DelegationError::Refused {
                        reason: sanitize_peer_text_line(&reason).into_owned(),
                    })
                }
            },
            Ok(LifecycleOutcome::InputRequired { task }) => Ok(DriverOutcome::InputRequired(task)),
            Ok(LifecycleOutcome::AuthRequired { task }) => {
                // Authorization cannot be supplied in-band; lifecycle cancelled it.
                self.node_tree
                    .try_set_state(node_id, NodeState::Failed)
                    .await
                    .map_err(|error| DelegationError::State(error.to_string()))?;
                // AC8 — `task.id` and `task.context_id` are chosen by the
                // REMOTE agent and this string reaches the journal, the chat
                // transcript, and `rustain team log`'s stdout. Strip control
                // bytes and cap length before any of that: the inbound side
                // has bounded ids since 18.1b, the outbound side had nothing.
                let task_correlation = disclosable_task_id(&task.id);
                let detail = format!(
                    "remote agent requires authorization (task {}, context {}); cancelled",
                    task_correlation,
                    task.context_id
                        .as_deref()
                        .map(|id| sanitize_disclosable(id, MAX_PEER_ID_BYTES))
                        .unwrap_or_else(|| "—".to_owned()),
                );
                self.emit_room(RoomEvent::RemoteEnvelopeRejected {
                    rail: Some(crate::domain::models::PeerRail::A2a),
                    peer: peer.clone(),
                    reason: RejectReason::Policy { detail },
                    direction: Direction::Outbound,
                    task: Some(task_correlation.clone()),
                })
                .await?;
                self.emit_refused(
                    node_id,
                    parent_tool_call_id,
                    &task_correlation,
                    RefuseReason::Policy,
                );
                Err(DelegationError::AuthRequired {
                    task_id: task.id,
                    context_id: task.context_id,
                })
            }
            Err(error) => {
                let reason = error.to_string();
                self.reject(
                    node_id,
                    peer,
                    parent_tool_call_id,
                    &dispatched_task_id,
                    &reason,
                )
                .await?;
                Err(DelegationError::Transport(error))
            }
        }
    }

    async fn materialize(
        &self,
        node_id: &AgentId,
    ) -> Result<(CancellationToken, mpsc::Receiver<Op>), DelegationError> {
        let (command_tx, command_rx) = mpsc::channel(1);
        let cancel_token = CancellationToken::new();
        let (status_tx, _) = watch::channel(NodeState::Created);
        let (_, metrics_rx) = watch::channel(AgentMetrics::default());
        self.node_tree
            .register_peer(
                node_id.clone(),
                AgentHandle {
                    agent_id: node_id.clone(),
                    token: CapabilityTokenId::nil(),
                    command_tx,
                    cancel_token: cancel_token.clone(),
                    depth: 0,
                    subagent_type: "a2a-peer".into(),
                    spawned_at: 0,
                    status: status_tx,
                    metrics: metrics_rx,
                    isolated: false,
                    mailbox_budget: MailboxBudget::new(),
                },
            )
            .await
            .map_err(|error| DelegationError::Register(error.to_string()))?;
        Ok((cancel_token, command_rx))
    }

    async fn reject(
        &self,
        node_id: &AgentId,
        peer: &PeerId,
        parent_tool_call_id: &str,
        task_id: &str,
        reason: &str,
    ) -> Result<(), DelegationError> {
        self.node_tree
            .try_set_state(node_id, NodeState::Failed)
            .await
            .map_err(|error| DelegationError::State(error.to_string()))?;
        let task_correlation = disclosable_task_id(task_id);
        self.emit_room(RoomEvent::RemoteEnvelopeRejected {
            rail: Some(crate::domain::models::PeerRail::A2a),
            peer: peer.clone(),
            reason: RejectReason::Policy {
                // AC8 — `reason` reaches here from `error.to_string()` on the
                // transport path, which carries remote-influenced content.
                detail: sanitize_disclosable(reason, MAX_SUMMARY_BYTES),
            },
            direction: Direction::Outbound,
            task: Some(task_correlation.clone()),
        })
        .await?;
        self.emit_refused(
            node_id,
            parent_tool_call_id,
            &task_correlation,
            RefuseReason::Policy,
        );
        Ok(())
    }

    /// Durable-first room emission. `RoomJournal` owns the append-then-bus
    /// ordering; a failed append is surfaced to both the delegation caller and
    /// the same latched operator condition used by inbound transparency.
    async fn emit_room(&self, event: RoomEvent) -> Result<(), DelegationError> {
        if let Err(error) = self.journal.record_event(event).await {
            self.latch_journal_failure(&error).await;
            return Err(DelegationError::Journal(error));
        }
        Ok(())
    }

    async fn latch_journal_failure(&self, error: &RoomJournalError) {
        let _emit_guard = self.journal_failure_latch.emit_lock.lock().await;
        let failures = self
            .journal_failure_latch
            .failures
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        tracing::error!(
            %error,
            failures,
            "failed to journal outbound A2A transparency record; records are missing from the audit log"
        );
        let payload = DomainEventPayload::TransparencyJournalFailed {
            failures,
            detail: error.to_string(),
        };
        let _ = self.event_tx.send(AppEvent::DomainEvent(payload)); // CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS: 18-2 AC1 — latched journal-failure notice; this adapter holds an event_tx, not an EventBus
    }

    fn emit_refused(
        &self,
        node_id: &AgentId,
        parent_tool_call_id: &str,
        correlation: &str,
        reason: RefuseReason,
    ) {
        let envelope = SubagentEnvelope::new(
            parent_tool_call_id.to_owned(),
            node_id.clone(),
            MessageKind::PeerMessage,
            SubagentEvent::MessageRefused {
                correlation_id: CorrelationId::new(correlation),
                reason,
            },
        );
        let receipt = AppEvent::Subagent(envelope);
        let _ = self.event_tx.send(receipt);
    }
}

/// Mint a peer node id that durably and reversibly encodes `(peer, remote task
/// id)` as three safe `AgentId` path segments. Both values cross configuration
/// or network trust boundaries, so neither may be interpolated into an
/// `AgentId` before encoding.
fn mint_node_id(peer: &str, task_id: &str) -> AgentId {
    let peer = URL_SAFE_NO_PAD.encode(peer.as_bytes());
    let task = URL_SAFE_NO_PAD.encode(task_id.as_bytes());
    AgentId::from_validated(format!("a2a/p-{peer}/t-{task}"))
}

/// The only form of a remote task id that can reach a room event or a local
/// refusal receipt. The unmodified id remains in [`TaskSnapshot`] for wire
/// polling and cancellation. Reserve the truncation marker inside the bound
/// when rejecting an oversized remote id.
pub(crate) fn disclosable_task_id(task_id: &str) -> String {
    let limit = if task_id.len() > MAX_PEER_ID_BYTES {
        MAX_PEER_ID_BYTES.saturating_sub(TRUNCATION_MARKER.len())
    } else {
        MAX_PEER_ID_BYTES
    };
    sanitize_disclosable(task_id, limit)
}

/// Recover `(peer, task_id)` from a node id minted by [`mint_node_id`]. Returns
/// `None` for malformed, empty, or non-A2A node ids.
fn parse_a2a_node_id(node_id: &AgentId) -> Option<(String, String)> {
    let rest = node_id.as_str().strip_prefix("a2a/")?;
    let (peer, task) = rest.split_once('/')?;
    if task.contains('/') {
        return None;
    }
    let peer = peer.strip_prefix("p-")?;
    let task = task.strip_prefix("t-")?;
    let peer = String::from_utf8(URL_SAFE_NO_PAD.decode(peer).ok()?).ok()?;
    let task = String::from_utf8(URL_SAFE_NO_PAD.decode(task).ok()?).ok()?;
    if peer.is_empty() || task.is_empty() {
        return None;
    }
    Some((peer, task))
}

/// Project a RAP task state onto the coarse node lifecycle. `input-required` /
/// `auth-required` are handled by the delegation outcome (R-C), not projected as
/// a live hop.
fn project_rap_to_node_state(state: RapTaskState) -> Option<NodeState> {
    match state {
        RapTaskState::Submitted | RapTaskState::Working => Some(NodeState::Running),
        RapTaskState::Completed => Some(NodeState::Completed),
        RapTaskState::Failed | RapTaskState::Rejected => Some(NodeState::Failed),
        RapTaskState::Canceled => Some(NodeState::Cancelled),
        RapTaskState::InputRequired | RapTaskState::AuthRequired => None,
    }
}

fn content_hash<T: serde::Serialize + ?Sized>(value: &T) -> ContentHash {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    ContentHash::from_bytes(Sha256::digest(&bytes).into())
}

/// Story 19.18 `Q2` — the shipped [`content_hash`] of one outbound or served
/// text: the same bytes as hashing `Value::String(text)`, with no copy. The
/// asker computes it for its dispatch row and for a served item turn, so the
/// two agree exactly when the texts do.
pub(crate) fn text_content_hash(text: &str) -> ContentHash {
    content_hash(text)
}

fn outbound_message_fact(message: &serde_json::Value) -> (Option<String>, usize) {
    let task = message
        .pointer("/message/messageId")
        .and_then(serde_json::Value::as_str)
        .map(disclosable_task_id);
    let bytes = message
        .pointer("/message/parts/0/text")
        .and_then(serde_json::Value::as_str)
        .map_or(0, str::len);
    (task, bytes)
}

/// Story 19.17: one process-wide source for both model delegation and
/// addressed sends. A clock step backwards cannot reuse an issued id.
static LAST_MESSAGE_NANOS: AtomicU64 = AtomicU64::new(0);

fn next_message_nanos(now: u64, last: &AtomicU64) -> u64 {
    let mut previous = last.load(Ordering::Relaxed);
    loop {
        let next = now.max(previous.checked_add(1).expect("message id space exhausted"));
        match last.compare_exchange_weak(previous, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(observed) => previous = observed,
        }
    }
}

/// Story 19.18c `A21` — one outbound message id, `rustain-{nanos}-{r}` with
/// `r` 64 random bits as 16 lowercase hex. The nanos cell is process-local,
/// but the TUI and the daemon share the item-capability key (`A3`) and a
/// clock can step back across a restart: without `r` two recipients could
/// receive items with the SAME capability. Pure over its cell and RNG so a
/// unit can feed two independent cells one clock reading.
fn mint_message_id(now: u64, last: &AtomicU64, rng: &mut impl rand_core::RngCore) -> String {
    let nanos = next_message_nanos(now, last);
    format!("rustain-{nanos}-{:016x}", rng.next_u64())
}

/// Host-owned remote task/context addressing for an in-band answer.
#[derive(Clone, Copy, Debug)]
pub struct Continuation<'a> {
    pub task_id: &'a str,
    pub context_id: Option<&'a str>,
}

/// The carriers [`build_message`] writes from **Rust parameters only** —
/// ⛔ never from `input`, which carries model-controlled tool arguments.
#[derive(Clone, Copy, Debug)]
pub struct MessageCarriers<'a> {
    pub continuation: Option<Continuation<'a>>,
    /// Story 19.18 `AC7(b)` — the id a follow-up answers
    /// ([`super::REPLY_TO_METADATA_KEY`]); the model rail passes `None`.
    pub reply_to: Option<&'a str>,
    /// Story 19.18c `A2` — this host's key: the message carries
    /// `derive(key, messageId)` ([`super::ITEM_CAPABILITY_METADATA_KEY`]).
    pub item_key: Option<&'a ItemCapabilityKey>,
    /// Story 19.18c `A10` — the capability of the reference's nearest item
    /// ([`super::REPLY_CAPABILITY_METADATA_KEY`]).
    pub reply_capability: Option<&'a ItemCapability>,
    /// Story 19.21d `AC3` — the frozen per-recipient story list; never a follow-up.
    pub stories: Option<&'a serde_json::Value>,
    /// Only the keys that selected this recipient; absent on follow-ups.
    pub selected_for: Option<&'a serde_json::Value>,
    pub authorship: Authorship,
    pub send_authorization: SendAuthorization,
}

/// Build an A2A `message/send` params object from the tool input. The JSON-RPC
/// binding uses `kind`-tagged parts.
///
/// D3: the closed semantic-type domain holds on the emit side too. A recognised
/// token is re-emitted under its canonical spelling; absence/empty omits the
/// carrier key entirely (the receiver resolves absence to Unknown); any other
/// token is dropped with a warning — never forwarded raw.
///
/// Story 19.18 `AC7(b)` / 19.18c `A18`: every `x-rustain-*` carrier comes
/// from [`MessageCarriers`], never from `input`. With a key the message is
/// capability-bearing and the returned [`OwnTask`] binds the transport that
/// sends it (`A9`); without one the wire is 19.18's.
pub fn build_message(
    input: &serde_json::Value,
    carriers: MessageCarriers<'_>,
) -> (serde_json::Value, Option<OwnTask>) {
    let text = input
        .get("message")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| input.to_string());
    let message_type = input
        .get("message_type")
        .and_then(serde_json::Value::as_str)
        .filter(|token| !token.is_empty())
        .and_then(|token| match SemanticMessageType::parse(token) {
            SemanticMessageType::Unknown => {
                tracing::warn!("dropping unrecognized a2a message_type token {token:?}");
                None
            }
            kind => Some(semantic_message_type_metadata(kind).token),
        });
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).expect("epoch nanoseconds fit message id"))
        .unwrap_or(0);
    let message_id = mint_message_id(nanos, &LAST_MESSAGE_NANOS, &mut rand_core::OsRng);
    let mut metadata = serde_json::Map::new();
    if !carriers.authorship.is_unknown() {
        metadata.insert(
            super::AUTHORSHIP_METADATA_KEY.to_owned(),
            serde_json::Value::String(carriers.authorship.wire_name().to_owned()),
        );
    }
    if !carriers.send_authorization.is_unknown() {
        metadata.insert(
            super::SEND_AUTHORIZATION_METADATA_KEY.to_owned(),
            serde_json::Value::String(carriers.send_authorization.wire_name().to_owned()),
        );
    }
    if let Some(token) = message_type {
        metadata.insert(
            super::MESSAGE_TYPE_METADATA_KEY.to_owned(),
            serde_json::Value::String(token.to_owned()),
        );
    }
    if let Some(reply_to) = carriers.reply_to {
        metadata.insert(
            super::REPLY_TO_METADATA_KEY.to_owned(),
            serde_json::Value::String(reply_to.to_owned()),
        );
    }
    if carriers.reply_to.is_none()
        && let Some(stories) = carriers.stories
    {
        metadata.insert(super::STORIES_METADATA_KEY.to_owned(), stories.clone());
    }
    if carriers.reply_to.is_none()
        && let Some(selected_for) = carriers.selected_for
    {
        metadata.insert(
            super::SELECTED_FOR_METADATA_KEY.to_owned(),
            selected_for.clone(),
        );
    }
    if let Some(reply_capability) = carriers.reply_capability {
        metadata.insert(
            super::REPLY_CAPABILITY_METADATA_KEY.to_owned(),
            serde_json::Value::String(reply_capability.as_wire_str().to_owned()),
        );
    }
    let own_task = carriers.item_key.map(|key| {
        let capability = derive_item_capability(key, &message_id);
        metadata.insert(
            super::ITEM_CAPABILITY_METADATA_KEY.to_owned(),
            serde_json::Value::String(capability.as_wire_str().to_owned()),
        );
        OwnTask {
            message_id: message_id.clone(),
            capability,
        }
    });
    let mut message = serde_json::json!({
            "message": {
                "kind": "message",
                "messageId": message_id,
                "role": "user",
                "parts": [{ "kind": "text", "text": text }],
                "metadata": metadata,
            }
    });
    if let Some(continuation) = carriers.continuation {
        message["message"]["taskId"] = continuation.task_id.into();
        if let Some(context_id) = continuation.context_id {
            message["message"]["contextId"] = context_id.into();
        }
    }
    (message, own_task)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    use parking_lot::Mutex;
    use tracing_test::traced_test;

    use super::*;
    use crate::domain::models::{JournalRecord, RedactedUrl};
    use crate::infrastructure::subagent::{NodeJournal, NodeRoomJournal};

    #[test]
    fn message_ids_are_unique_at_one_clock_tick_and_after_clock_regression() {
        let last = AtomicU64::new(0);
        assert_eq!(next_message_nanos(42, &last), 42);
        assert_eq!(next_message_nanos(42, &last), 43);
        assert_eq!(next_message_nanos(10, &last), 44);
        assert_eq!(next_message_nanos(90, &last), 90);
    }

    /// Story 19.18c `K14` (id half, `A21`) — two INDEPENDENT nanos cells (the
    /// TUI and the daemon share the key but not the cell) fed one clock
    /// reading still mint different ids. `M27` → RED: drop the suffix.
    #[test]
    fn k14_two_processes_at_one_clock_reading_mint_distinct_ids() {
        let (first, second) = (AtomicU64::new(0), AtomicU64::new(0));
        let a = mint_message_id(1_000, &first, &mut rand_core::OsRng);
        let b = mint_message_id(1_000, &second, &mut rand_core::OsRng);
        assert_ne!(a, b);
        for id in [&a, &b] {
            assert!(is_minted_message_id(id), "{id}");
        }
    }

    /// `^rustain-\d+-[0-9a-f]{16}$`, without a regex dependency in the lib.
    fn is_minted_message_id(id: &str) -> bool {
        let Some((nanos, suffix)) = id
            .strip_prefix("rustain-")
            .and_then(|rest| rest.split_once('-'))
        else {
            return false;
        };
        !nanos.is_empty()
            && nanos.bytes().all(|byte| byte.is_ascii_digit())
            && suffix.len() == 16
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }

    #[test]
    fn build_message_emits_the_exact_semantic_type_metadata_key() {
        let (typed, _) = build_message(
            &serde_json::json!({
                "message": "review this",
                "message_type": "consultation"
            }),
            MessageCarriers {
                continuation: None,
                selected_for: None,
                reply_to: None,
                item_key: None,
                reply_capability: None,
                stories: None,
                authorship: Authorship::Unknown,
                send_authorization: SendAuthorization::Unknown,
            },
        );
        assert_eq!(
            typed.pointer("/message/metadata/x-rustain-message-type"),
            Some(&serde_json::Value::String("consultation".to_owned()))
        );

        let (untyped, _) = build_message(
            &serde_json::json!({ "message": "review this" }),
            MessageCarriers {
                continuation: None,
                selected_for: None,
                reply_to: None,
                item_key: None,
                reply_capability: None,
                stories: None,
                authorship: Authorship::Unknown,
                send_authorization: SendAuthorization::Unknown,
            },
        );
        assert_eq!(
            untyped.pointer("/message/metadata/x-rustain-message-type"),
            None,
            "D3: absence already resolves to Unknown on receipt, so the key is omitted"
        );
    }

    #[test]
    fn build_message_carries_explicit_provenance_and_omits_unknown() {
        for (authorship, send_authorization) in [
            (Authorship::HumanWritten, SendAuthorization::HumanDecided),
            (Authorship::AgentComposed, SendAuthorization::HumanDecided),
            (Authorship::AgentComposed, SendAuthorization::Automatic),
            (Authorship::AgentComposed, SendAuthorization::Unknown),
            (Authorship::Unknown, SendAuthorization::Unknown),
        ] {
            let (message, _) = build_message(
                &serde_json::json!({"message": "provenance"}),
                MessageCarriers {
                    continuation: None,
                    selected_for: None,
                    reply_to: None,
                    item_key: None,
                    reply_capability: None,
                    stories: None,
                    authorship,
                    send_authorization,
                },
            );
            let metadata = &message["message"]["metadata"];
            assert_eq!(
                metadata
                    .get("x-rustain-authorship")
                    .and_then(serde_json::Value::as_str),
                (!authorship.is_unknown()).then_some(authorship.wire_name()),
            );
            assert_eq!(
                metadata
                    .get("x-rustain-send-authorization")
                    .and_then(serde_json::Value::as_str),
                (!send_authorization.is_unknown()).then_some(send_authorization.wire_name()),
            );
        }
    }

    /// Story 19.18 `AC7(b)`, amended by Story 19.18c `A18` (19.17 `AC3(e)`'s
    /// byte-identity pin) — every carrier comes ONLY from `MessageCarriers`.
    /// Model-controlled `input` naming any carrier never
    /// produces one; without a key the model rail's metadata is `{}`; with a
    /// key it is exactly the item capability of the message it minted.
    #[test]
    fn every_carrier_is_written_only_from_its_rust_parameter() {
        let key = ItemCapabilityKey::from_file_bytes(&[5u8; 32]);
        let hostile = serde_json::json!({
            "message": "review this",
            "x-rustain-in-reply-to": "rr_forged",
            "inReplyTo": "rr_forged",
            "x-rustain-item-capability": "forged",
            "x-rustain-reply-capability": "forged",
            "x-rustain-stories": { "stories": [{ "key": "FORGED", "you": true }] },
            "x-rustain-authorship": "agent-composed",
            "x-rustain-send-authorization": "automatic",
            "authorship": "agent-composed",
            "send_authorization": "automatic",
        });
        let (model, own) = build_message(
            &hostile,
            MessageCarriers {
                continuation: None,
                selected_for: None,
                reply_to: None,
                item_key: None,
                reply_capability: None,
                stories: None,
                authorship: Authorship::Unknown,
                send_authorization: SendAuthorization::Unknown,
            },
        );
        assert!(own.is_none());
        assert_eq!(
            model.pointer("/message/metadata"),
            Some(&serde_json::json!({}))
        );
        let (keyed, own) = build_message(
            &hostile,
            MessageCarriers {
                continuation: None,
                selected_for: None,
                item_key: Some(&key),
                reply_to: None,
                reply_capability: None,
                stories: None,
                authorship: Authorship::Unknown,
                send_authorization: SendAuthorization::Unknown,
            },
        );
        let message_id = keyed["message"]["messageId"].as_str().expect("id");
        assert_eq!(
            keyed.pointer("/message/metadata"),
            Some(&serde_json::json!({
                "x-rustain-item-capability": derive_item_capability(&key, message_id).as_wire_str(),
            })),
            "with a key, exactly the derived carrier — nothing from `input`"
        );
        assert_eq!(own.expect("bound").message_id(), message_id);
        let reply = derive_item_capability(&key, "rustain-0-0000000000000000");
        let (follow_up, _) = build_message(
            &serde_json::json!({ "message": "and retries?" }),
            MessageCarriers {
                continuation: None,
                selected_for: None,
                reply_to: Some("rr_1"),
                reply_capability: Some(&reply),
                item_key: None,
                stories: None,
                authorship: Authorship::Unknown,
                send_authorization: SendAuthorization::Unknown,
            },
        );
        assert_eq!(
            follow_up.pointer("/message/metadata"),
            Some(&serde_json::json!({
                "x-rustain-in-reply-to": "rr_1",
                "x-rustain-reply-capability": reply.as_wire_str(),
            }))
        );
    }

    /// Story 19.18c `K02` (message half, `P13`) — the carrier equals
    /// `derive(key, messageId)` for the id `build_message` minted, and that
    /// WIRE id has the `A21` shape (a suffix helper the builder never called
    /// would pass `K14`). `M03` → RED: read the carrier from `input`.
    #[test]
    fn k02_the_wire_message_id_is_suffixed_and_carries_its_capability() {
        let key = ItemCapabilityKey::from_file_bytes(&[6u8; 32]);
        let (message, own) = build_message(
            &serde_json::json!({ "message": "q" }),
            MessageCarriers {
                continuation: None,
                selected_for: None,
                item_key: Some(&key),
                reply_to: None,
                reply_capability: None,
                stories: None,
                authorship: Authorship::Unknown,
                send_authorization: SendAuthorization::Unknown,
            },
        );
        let id = message["message"]["messageId"].as_str().expect("id");
        assert!(is_minted_message_id(id), "{id}");
        assert_eq!(
            message["message"]["metadata"]["x-rustain-item-capability"],
            derive_item_capability(&key, id).as_wire_str()
        );
        let own = own.expect("bound");
        assert_eq!(
            own.capability.as_wire_str(),
            derive_item_capability(&key, id).as_wire_str()
        );
    }

    #[test]
    #[traced_test]
    fn build_message_drops_an_unrecognized_type_token_and_warns_its_name() {
        // D3: the closed domain holds on emit too — an unrecognized token is
        // never forwarded raw; the carrier key is omitted so the receiver
        // resolves Unknown, and the drop warns once, naming the token.
        let (dropped, _) = build_message(
            &serde_json::json!({
                "message": "review this",
                "message_type": "urgent_escalation"
            }),
            MessageCarriers {
                continuation: None,
                selected_for: None,
                reply_to: None,
                item_key: None,
                reply_capability: None,
                stories: None,
                authorship: Authorship::Unknown,
                send_authorization: SendAuthorization::Unknown,
            },
        );
        assert_eq!(
            dropped.pointer("/message/metadata/x-rustain-message-type"),
            None,
            "an unrecognized token must never reach the wire"
        );
        assert!(
            logs_contain("urgent_escalation"),
            "the warn must name the dropped token"
        );

        let (empty, _) = build_message(
            &serde_json::json!({
                "message": "review this",
                "message_type": ""
            }),
            MessageCarriers {
                continuation: None,
                selected_for: None,
                reply_to: None,
                item_key: None,
                reply_capability: None,
                stories: None,
                authorship: Authorship::Unknown,
                send_authorization: SendAuthorization::Unknown,
            },
        );
        assert_eq!(
            empty.pointer("/message/metadata/x-rustain-message-type"),
            None,
            "an empty token is absence, not a type"
        );
    }

    /// Story 19.18c `K06e` (`PQ2` = B widened) — the own-row predicate, pure.
    /// Production caller: the capability resolver (`own_task_capability`).
    /// `M33` (endpoint arm), `M34` (pseudonym arm) and `M35` (a row without
    /// `endpoint` matching the current origin) each turn a row here RED.
    #[test]
    fn k06e_a_dispatch_row_is_own_by_pin_pseudonym_or_origin() {
        use crate::domain::models::{A2aPeerSource, PinnedKey, PinnedKeyAlgorithm};
        let pinned = |alias: &str, byte: u8| {
            A2aPeerSpec::new(
                alias,
                RedactedUrl::new("https://peer.example/a2a".to_owned()),
                A2aPeerSource::Workspace,
            )
            .with_pinned_key(Some(PinnedKey::new(
                PinnedKeyAlgorithm::EdDsa,
                URL_SAFE_NO_PAD.encode([byte; 32]),
                None,
            )))
        };
        let unpinned = |alias: &str| {
            A2aPeerSpec::new(
                alias,
                RedactedUrl::new("https://peer.example/a2a".to_owned()),
                A2aPeerSource::Workspace,
            )
        };
        let pin_b = pinned("b", 7).resolved_identity().to_string();
        let pseudonym_b = unpinned("b").resolved_identity().to_string();
        // Unpinned rows (they carry the origin digest).
        assert!(
            dispatch_row_is_own(&pseudonym_b, Some("o1"), &unpinned("b"), Some("o2")),
            "a move keeps the alias pseudonym"
        );
        assert!(
            dispatch_row_is_own(&pseudonym_b, Some("o1"), &pinned("b", 7), Some("o1")),
            "a pin-add admits the unpinned row by pseudonym"
        );
        assert!(
            dispatch_row_is_own(&pseudonym_b, Some("o1"), &unpinned("b2"), Some("o1")),
            "a rename at the same origin keeps the endpoint"
        );
        assert!(
            !dispatch_row_is_own(&pseudonym_b, Some("o1"), &unpinned("b2"), Some("o2")),
            "a rename AND a move orphan the row"
        );
        // Pinned rows (no endpoint).
        assert!(
            dispatch_row_is_own(&pin_b, None, &pinned("b2", 7), Some("o9")),
            "a pinned row survives a rename and a move under the same pin"
        );
        assert!(
            !dispatch_row_is_own(&pin_b, None, &pinned("b", 8), Some("o1")),
            "a replaced pin at the same origin cuts it — never matched by endpoint"
        );
        assert!(
            !dispatch_row_is_own(&pin_b, None, &unpinned("b"), Some("o1")),
            "a removed pin cuts it"
        );
        assert!(
            !dispatch_row_is_own(&pseudonym_b, None, &unpinned("b"), Some("o1")),
            "a row without endpoint (pre-19.18c) never grants by pseudonym or origin"
        );
    }

    fn spec(id: &str, verified: bool) -> A2aPeerSpec {
        use crate::domain::models::{A2aPeerSource, PinnedKey, PinnedKeyAlgorithm};
        A2aPeerSpec::new(
            id,
            RedactedUrl::new("https://peer.example/a2a".to_owned()),
            A2aPeerSource::Workspace,
        )
        .with_pinned_key(verified.then(|| {
            PinnedKey::new(
                PinnedKeyAlgorithm::EdDsa,
                URL_SAFE_NO_PAD.encode([7u8; 32]),
                None,
            )
        }))
    }

    struct Scripted {
        script: Mutex<VecDeque<serde_json::Value>>,
        cancels: Mutex<Vec<String>>,
    }

    impl Scripted {
        fn new(states: &[&str]) -> Self {
            Self::with_task_id("task-1", states)
        }

        fn with_task_id(task_id: impl Into<String>, states: &[&str]) -> Self {
            let task_id = task_id.into();
            let script = states
                .iter()
                .map(|state| {
                    serde_json::json!({
                        "kind": "task",
                        "id": task_id.clone(),
                        "contextId": "ctx-1",
                        "status": {"state": state},
                        "artifacts": [{"parts": [{"kind": "text", "text": "result"}]}]
                    })
                })
                .collect();
            Self {
                script: Mutex::new(script),
                cancels: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl A2aTaskTransport for Scripted {
        async fn message_send(
            &self,
            _message: serde_json::Value,
        ) -> Result<serde_json::Value, A2aError> {
            Ok(self.script.lock().pop_front().expect("script exhausted"))
        }
        async fn tasks_get(&self, _task_id: &str) -> Result<serde_json::Value, A2aError> {
            Ok(self.script.lock().pop_front().expect("script exhausted"))
        }
        async fn tasks_cancel(&self, task_id: &str) -> Result<serde_json::Value, A2aError> {
            self.cancels.lock().push(task_id.to_owned());
            Ok(serde_json::json!({"kind":"task","id":task_id,"status":{"state":"canceled"}}))
        }
    }

    struct DeadTransport;

    #[async_trait]
    impl A2aTaskTransport for DeadTransport {
        async fn message_send(
            &self,
            _message: serde_json::Value,
        ) -> Result<serde_json::Value, A2aError> {
            Err(A2aError::Request("connection refused".to_owned()))
        }

        async fn tasks_get(&self, _task_id: &str) -> Result<serde_json::Value, A2aError> {
            panic!("transport failure must not poll")
        }

        async fn tasks_cancel(&self, _task_id: &str) -> Result<serde_json::Value, A2aError> {
            panic!("transport failure must not cancel")
        }
    }

    /// In-memory room-journal port for driver tests that need the same
    /// durable-first bus contract without a filesystem fixture.
    struct TestRoomJournal {
        event_tx: mpsc::UnboundedSender<AppEvent>,
    }

    #[async_trait]
    impl RoomJournal for TestRoomJournal {
        async fn record_event(&self, event: RoomEvent) -> Result<(), RoomJournalError> {
            let _ = self.event_tx.send(AppEvent::DomainEvent(event.into())); // CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS: 18-2 AC1 — RoomJournal test double; a fake has no EventBus
            Ok(())
        }
    }

    /// Models a failed append after scheduling, so concurrent driver futures
    /// contend at the failure latch rather than completing serially.
    struct BrokenRoomJournal;

    #[async_trait]
    impl RoomJournal for BrokenRoomJournal {
        async fn record_event(&self, _event: RoomEvent) -> Result<(), RoomJournalError> {
            tokio::task::yield_now().await;
            Err(RoomJournalError::Append("disk full".to_owned()))
        }
    }
    /// A journal that counts `RemoteEnvelopeDispatched` appends so a mock
    /// transport can prove the submit fact was durable **before** the POST —
    /// the ordering a final-events assertion cannot distinguish (AC2(a)'s
    /// named mutant moves the append after `message_send` and keeps the
    /// final order).
    struct CountingJournal {
        inner: Arc<dyn RoomJournal>,
        dispatched: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl RoomJournal for CountingJournal {
        async fn record_event(&self, event: RoomEvent) -> Result<(), RoomJournalError> {
            if matches!(event, RoomEvent::RemoteEnvelopeDispatched { .. }) {
                self.dispatched.fetch_add(1, Ordering::Relaxed);
            }
            self.inner.record_event(event).await
        }
    }

    /// Transport that asserts, at `message_send` entry, that the dispatch
    /// fact is already journaled.
    struct JournalObservingTransport {
        script: Scripted,
        dispatched: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl A2aTaskTransport for JournalObservingTransport {
        async fn message_send(
            &self,
            message: serde_json::Value,
        ) -> Result<serde_json::Value, A2aError> {
            assert!(
                self.dispatched.load(Ordering::Relaxed) >= 1,
                "the submit fact must be journaled BEFORE the POST leaves"
            );
            self.script.message_send(message).await
        }
        async fn tasks_get(&self, task_id: &str) -> Result<serde_json::Value, A2aError> {
            self.script.tasks_get(task_id).await
        }
        async fn tasks_cancel(&self, task_id: &str) -> Result<serde_json::Value, A2aError> {
            self.script.tasks_cancel(task_id).await
        }
    }

    fn counting_runtime() -> (
        A2aDelegationRuntime,
        NodeTree,
        mpsc::UnboundedReceiver<AppEvent>,
        Arc<AtomicUsize>,
    ) {
        let tree = NodeTree::new();
        let (tx, rx) = mpsc::unbounded_channel();
        let dispatched = Arc::new(AtomicUsize::new(0));
        let room: Arc<dyn RoomJournal> = Arc::new(CountingJournal {
            inner: Arc::new(TestRoomJournal {
                event_tx: tx.clone(),
            }),
            dispatched: dispatched.clone(),
        });
        (
            A2aDelegationRuntime::new(tree.clone(), room, tx),
            tree,
            rx,
            dispatched,
        )
    }

    fn runtime() -> (
        A2aDelegationRuntime,
        NodeTree,
        mpsc::UnboundedReceiver<AppEvent>,
    ) {
        let tree = NodeTree::new();
        let (tx, rx) = mpsc::unbounded_channel();
        let room: Arc<dyn RoomJournal> = Arc::new(TestRoomJournal {
            event_tx: tx.clone(),
        });
        (A2aDelegationRuntime::new(tree.clone(), room, tx), tree, rx)
    }

    /// Story 19.16b AC5(d) — the board's stated poll ceiling is ENFORCED on
    /// the configured interval.
    ///
    /// ⛔ **A design bound, never an NFR64 latency claim, and never tested as
    /// one** (`amendment:163`): nothing here measures a round trip, and
    /// `prd.md:2604`'s 2 s is not re-armed. The number is the shipped
    /// substrate's own `PollConfig::default().interval`, ⛔ not invented.
    ///
    /// **Ratchet (Rule 4):** `now` is injected, so the bound is proven by a
    /// deterministic step — ⛔ never a `sleep` or a timing window.
    ///
    /// **Mutant → RED:** drop the elapsed check (the mid-floor refresh is
    /// admitted), or seed the gate as already-admitted (the first refresh is
    /// refused).
    #[tokio::test]
    async fn the_board_refresh_floor_is_enforced_on_the_configured_poll_interval() {
        use crate::adapters::a2a::board::board_refresh_floor;

        let (rt, _tree, _rx) = runtime();
        let floor = board_refresh_floor();
        assert_eq!(
            floor,
            super::super::lifecycle::PollConfig::default().interval,
            "the ceiling is DERIVED from the shipped substrate, not invented"
        );
        let t0 = std::time::Instant::now();

        assert!(
            rt.admit_board_refresh(t0, floor).await.is_ok(),
            "positive control: a first refresh is admitted, so the refusal \
             below is a bound and not a permanently closed door"
        );
        let refused = rt
            .admit_board_refresh(t0 + floor / 5, floor)
            .await
            .expect_err("a refresh inside the stated floor is refused");
        assert!(refused.remaining > std::time::Duration::ZERO);
        assert!(refused.remaining < floor);
        assert!(
            rt.admit_board_refresh(t0 + floor, floor).await.is_ok(),
            "once the floor has elapsed the board refreshes again"
        );
    }
    async fn wait_for_peer_node(tree: &NodeTree) -> AgentId {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(node) = tree
                    .list()
                    .await
                    .into_iter()
                    .find(|entry| entry.subagent_type == "a2a-peer")
                {
                    break node.agent_id;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("peer node materialized")
    }

    #[tokio::test]
    async fn completed_delegation_taints_node_and_emits_accepted() {
        let (rt, tree, mut rx) = runtime();
        let transport = Arc::new(Scripted::new(&["submitted", "working", "completed"]));
        let result = rt
            .delegate(
                &spec("planets", false),
                TrustTier::Unverified,
                "call-1",
                transport.clone(),
                serde_json::json!({
                    "message": {
                        "messageId": "sender-message-1",
                        "parts": [{ "kind": "text", "text": "ping" }]
                    }
                }),
                CancellationToken::new(),
                Authorship::AgentComposed,
                crate::domain::models::SendAuthorization::Unknown,
            )
            .await
            .expect("completed delegation returns content");
        assert_eq!(result["status"]["state"], "completed");

        let entry = tree
            .list()
            .await
            .into_iter()
            .find(|e| e.subagent_type == "a2a-peer")
            .expect("peer node materialized");
        assert_eq!(entry.current_status, NodeState::Completed);
        assert_eq!(entry.ownership, crate::domain::models::OwnershipKind::Peer);

        let room_events = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|event| {
                let AppEvent::DomainEvent(DomainEventPayload::Room(event)) = event else {
                    return None;
                };
                Some(event)
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            room_events.first(),
            Some(RoomEvent::RemoteEnvelopeDispatched {
                task: Some(task),
                bytes: 4,
                ..
            }) if task == "sender-message-1"
        ));
        assert!(matches!(
            room_events.get(1),
            Some(RoomEvent::RemoteEnvelopeAccepted {
                rail: Some(crate::domain::models::PeerRail::A2a),
                task: Some(task),
                direction: Direction::Outbound,
                ..
            }) if task == "task-1"
        ));
    }

    #[tokio::test]
    async fn submit_fact_is_durable_before_the_post_leaves() {
        // AC2(a)'s named mutant moves the dispatch append to after
        // `message_send`; final-order assertions stay green under it. Only a
        // transport that inspects the journal at POST entry catches it.
        let (rt, _tree, mut rx, dispatched) = counting_runtime();
        let transport = Arc::new(JournalObservingTransport {
            script: Scripted::new(&["completed"]),
            dispatched: dispatched.clone(),
        });
        let result = rt
            .delegate(
                &spec("observed", false),
                TrustTier::Unverified,
                "call-obs",
                transport,
                serde_json::json!({
                    "message": {
                        "messageId": "sender-observed-1",
                        "parts": [{ "kind": "text", "text": "ping" }]
                    }
                }),
                CancellationToken::new(),
                Authorship::AgentComposed,
                crate::domain::models::SendAuthorization::Unknown,
            )
            .await
            .expect("journal-observing transport completes");
        assert_eq!(result["status"]["state"], "completed");
        assert_eq!(dispatched.load(Ordering::Relaxed), 1);

        // The bus still carries the ordered pair.
        let mut kinds = 0;
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::DomainEvent(DomainEventPayload::Room(event)) = event {
                if matches!(
                    (kinds, &event),
                    (
                        0,
                        RoomEvent::RemoteEnvelopeDispatched {
                            task: Some(task),
                            ..
                        }
                    ) if task == "sender-observed-1"
                ) || matches!(
                    (kinds, &event),
                    (
                        1,
                        RoomEvent::RemoteEnvelopeAccepted {
                            direction: Direction::Outbound,
                            ..
                        }
                    )
                ) {
                    kinds += 1;
                }
            }
        }
        assert_eq!(kinds, 2, "dispatched then accepted, in order");
    }

    #[tokio::test]
    async fn peer_cancellation_is_journaled_as_a_terminal_refusal() {
        // AC2: the attempt AND the outcome are journaled — a canceled task
        // must not leave the dispatched row orphaned.
        let (rt, tree, mut rx) = runtime();
        let error = rt
            .delegate(
                &spec("canceller", false),
                TrustTier::Unverified,
                "call-cancel",
                Arc::new(Scripted::new(&["canceled"])),
                serde_json::json!({
                    "message": {
                        "messageId": "sender-cancel-1",
                        "parts": [{ "kind": "text", "text": "ping" }]
                    }
                }),
                CancellationToken::new(),
                Authorship::AgentComposed,
                crate::domain::models::SendAuthorization::Unknown,
            )
            .await
            .expect_err("canceled task must fail");
        assert!(matches!(error, DelegationError::Cancelled));

        let node = wait_for_peer_node(&tree).await;
        let entry = tree
            .list()
            .await
            .into_iter()
            .find(|e| e.agent_id == node)
            .expect("peer node exists");
        assert_eq!(entry.current_status, NodeState::Cancelled);

        let room_events = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|event| {
                let AppEvent::DomainEvent(DomainEventPayload::Room(event)) = event else {
                    return None;
                };
                Some(event)
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            room_events.first(),
            Some(RoomEvent::RemoteEnvelopeDispatched { .. })
        ));
        assert!(matches!(
            room_events.last(),
            Some(RoomEvent::RemoteEnvelopeRejected {
                rail: Some(crate::domain::models::PeerRail::A2a),
                direction: Direction::Outbound,
                ..
            })
        ));
        assert_eq!(room_events.len(), 2);
    }

    #[tokio::test]
    async fn peer_refusal_reason_is_carried_into_the_rejection() {
        // docs/a2a.md: "a remote refusal renders the peer's reason" — the
        // terminal arm must read status.message, not discard it.
        let (rt, _tree, mut rx) = runtime();
        struct RefusingTransport;
        #[async_trait]
        impl A2aTaskTransport for RefusingTransport {
            async fn message_send(
                &self,
                _message: serde_json::Value,
            ) -> Result<serde_json::Value, A2aError> {
                // The peer answers the POST itself with a terminal rejection
                // carrying its own human-readable reason.
                Ok(serde_json::json!({
                    "kind": "task",
                    "id": "task-r",
                    "status": {
                        "state": "rejected",
                        "message": {
                            "parts": [{ "kind": "text", "text": "quota \u{001b}[2Jexceeded\r\n[urgent] ┆ on peer" }]
                        }
                    }
                }))
            }
            async fn tasks_get(&self, _task_id: &str) -> Result<serde_json::Value, A2aError> {
                unreachable!("terminal first snapshot must not poll")
            }
            async fn tasks_cancel(&self, _task_id: &str) -> Result<serde_json::Value, A2aError> {
                Ok(serde_json::json!({}))
            }
        }
        let error = rt
            .delegate_inner(
                spec("refuser", false),
                TrustTier::Unverified,
                "call-refuse".to_owned(),
                Arc::new(RefusingTransport),
                serde_json::json!({
                    "message": {
                        "messageId": "sender-refuse-1",
                        "parts": [{ "kind": "text", "text": "ping" }]
                    }
                }),
                CancellationToken::new(),
                Authorship::AgentComposed,
                crate::domain::models::SendAuthorization::Unknown,
            )
            .await
            .expect_err("rejected task must fail");
        let DelegationError::Refused { reason } = &error else {
            panic!("expected a refusal, got {error:?}");
        };
        assert!(
            reason.contains("quota exceeded[urgent] ┆ on peer"),
            "{reason}"
        );
        assert!(reason.contains("rejected"), "{reason}");
        assert!(
            !reason.chars().any(char::is_control),
            "peer refusal reached the human-facing error with controls: {reason:?}"
        );
        assert!(!reason.contains("[2J"), "{reason}");

        let room_events = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|event| {
                let AppEvent::DomainEvent(DomainEventPayload::Room(event)) = event else {
                    return None;
                };
                Some(event)
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            room_events.last(),
            Some(RoomEvent::RemoteEnvelopeRejected {
                reason: RejectReason::Policy { detail },
                ..
            }) if detail.contains("quota [2Jexceeded[urgent] ┆ on peer")
        ));
    }

    #[tokio::test]
    async fn first_transport_failure_is_journaled_as_one_outbound_refusal_without_a_node() {
        let (rt, tree, mut rx) = runtime();
        let error = rt
            .delegate_inner(
                spec("dead", false),
                TrustTier::Unverified,
                "call-dead".to_owned(),
                Arc::new(DeadTransport),
                serde_json::json!({
                    "message": {
                        "messageId": "sender-dead-1",
                        "parts": [{ "kind": "text", "text": "private text" }]
                    }
                }),
                CancellationToken::new(),
                Authorship::AgentComposed,
                crate::domain::models::SendAuthorization::Unknown,
            )
            .await
            .expect_err("dead transport must fail");
        assert!(matches!(error, DelegationError::Transport(_)));
        assert!(
            tree.list().await.is_empty(),
            "no peer node exists before POST"
        );

        let room_events = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|event| {
                let AppEvent::DomainEvent(DomainEventPayload::Room(event)) = event else {
                    return None;
                };
                Some(event)
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            room_events.first(),
            Some(RoomEvent::RemoteEnvelopeDispatched {
                task: Some(task),
                ..
            }) if task == "sender-dead-1"
        ));
        assert_eq!(
            room_events
                .iter()
                .filter(|event| matches!(event, RoomEvent::RemoteEnvelopeRejected { .. }))
                .count(),
            1
        );
        assert!(matches!(
            room_events.last(),
            Some(RoomEvent::RemoteEnvelopeRejected {
                rail: Some(crate::domain::models::PeerRail::A2a),
                direction: Direction::Outbound,
                task: Some(task),
                ..
            }) if task == "sender-dead-1"
        ));
        assert!(
            !format!("{room_events:?}").contains("private text"),
            "journal events must never contain sent text"
        );
    }

    #[tokio::test]
    async fn outbound_success_does_not_escape_after_room_append_failure() {
        let tree = NodeTree::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let rt = A2aDelegationRuntime::new(tree, Arc::new(BrokenRoomJournal), tx);

        let result = rt
            .delegate(
                &spec("planets", false),
                TrustTier::Unverified,
                "call-journal-failure",
                Arc::new(Scripted::new(&["working", "completed"])),
                serde_json::json!({}),
                CancellationToken::new(),
                Authorship::AgentComposed,
                crate::domain::models::SendAuthorization::Unknown,
            )
            .await;
        assert!(matches!(
            result,
            Err(DelegationError::Journal(RoomJournalError::Append(_)))
        ));

        let failure_counts: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|event| match event {
                AppEvent::DomainEvent(DomainEventPayload::TransparencyJournalFailed {
                    failures,
                    ..
                }) => Some(failures),
                _ => None,
            })
            .collect();
        assert_eq!(failure_counts, vec![1]);
    }

    #[tokio::test]
    async fn oversized_outbound_task_id_is_not_materialized_and_is_cancelled_raw() {
        let (rt, tree, mut rx) = runtime();
        let raw_task_id = "x".repeat(MAX_PEER_ID_BYTES + 1);
        let transport = Arc::new(Scripted::with_task_id(raw_task_id.clone(), &["working"]));

        let error = rt
            .delegate(
                &spec("planets", false),
                TrustTier::Unverified,
                "call-oversized-id",
                transport.clone(),
                serde_json::json!({}),
                CancellationToken::new(),
                Authorship::AgentComposed,
                crate::domain::models::SendAuthorization::Unknown,
            )
            .await
            .expect_err("an oversized remote task id is refused before materialization");
        assert!(matches!(error, DelegationError::Refused { .. }));
        assert_eq!(transport.cancels.lock().as_slice(), [raw_task_id.as_str()]);
        assert!(
            tree.list()
                .await
                .into_iter()
                .all(|entry| entry.subagent_type != "a2a-peer"),
            "the oversized id must not become a local node"
        );

        let recorded_task = std::iter::from_fn(|| rx.try_recv().ok()).find_map(|event| {
            let AppEvent::DomainEvent(DomainEventPayload::Room(
                RoomEvent::RemoteEnvelopeRejected { task, .. },
            )) = event
            else {
                return None;
            };
            task
        });
        let recorded_task = recorded_task.expect("the refusal has canonical task correlation");
        assert_eq!(recorded_task, disclosable_task_id(&raw_task_id));
        assert!(recorded_task.len() <= MAX_PEER_ID_BYTES);
    }

    #[tokio::test]
    async fn retract_journal_rows_bound_and_strip_the_peer_supplied_task() {
        let (rt, _tree, mut rx) = runtime();
        let peer = spec("jun-dev", false);
        let raw_task = format!("task\n{}", "x".repeat(MAX_PEER_ID_BYTES + 10));

        rt.journal_item_retract_dispatch(&peer, Some(&raw_task), "ri_x", 17)
            .await
            .expect("the dispatch row is durable");
        rt.journal_item_retract_refusal(&peer, Some(&raw_task), "item retract refused: test")
            .await;

        let tasks: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|event| {
                let AppEvent::DomainEvent(DomainEventPayload::Room(event)) = event else {
                    return None;
                };
                match event {
                    RoomEvent::RemoteEnvelopeDispatched { task, .. }
                    | RoomEvent::RemoteEnvelopeRejected { task, .. } => task,
                    _ => None,
                }
            })
            .collect();
        let expected = sanitize_disclosable(&raw_task, MAX_PEER_ID_BYTES);
        assert_eq!(tasks, vec![expected.clone(), expected]);
        assert!(!tasks[0].contains('\n'));
    }

    #[tokio::test]
    async fn concurrent_outbound_journal_failures_emit_monotonic_latch_counts() {
        let tree = NodeTree::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let rt = A2aDelegationRuntime::new(tree, Arc::new(BrokenRoomJournal), tx);

        let left_spec = spec("left", false);
        let right_spec = spec("right", false);
        let left = rt.delegate(
            &left_spec,
            TrustTier::Unverified,
            "call-left",
            Arc::new(Scripted::new(&["working", "completed"])),
            serde_json::json!({}),
            CancellationToken::new(),
            Authorship::AgentComposed,
            crate::domain::models::SendAuthorization::Unknown,
        );
        let right = rt.delegate(
            &right_spec,
            TrustTier::Unverified,
            "call-right",
            Arc::new(Scripted::new(&["working", "completed"])),
            serde_json::json!({}),
            CancellationToken::new(),
            Authorship::AgentComposed,
            crate::domain::models::SendAuthorization::Unknown,
        );
        let (left, right) = tokio::join!(left, right);
        assert!(matches!(
            left,
            Err(DelegationError::Journal(RoomJournalError::Append(_)))
        ));
        assert!(matches!(
            right,
            Err(DelegationError::Journal(RoomJournalError::Append(_)))
        ));

        let failure_counts: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|event| match event {
                AppEvent::DomainEvent(DomainEventPayload::TransparencyJournalFailed {
                    failures,
                    ..
                }) => Some(failures),
                _ => None,
            })
            .collect();
        assert_eq!(failure_counts, vec![1, 2]);
    }
    #[tokio::test]
    async fn dropped_caller_still_cancels_remote_task_and_terminalizes_node() {
        let (rt, tree, _rx) = runtime();
        let transport = Arc::new(Scripted::new(&["working", "working"]));
        let cancel = CancellationToken::new();
        let runner = tokio::spawn({
            let rt = rt.clone();
            let transport = transport.clone();
            let cancel = cancel.clone();
            async move {
                rt.delegate(
                    &spec("planets", false),
                    TrustTier::Unverified,
                    "call-cancel",
                    transport,
                    serde_json::json!({}),
                    cancel,
                    Authorship::AgentComposed,
                    crate::domain::models::SendAuthorization::Unknown,
                )
                .await
            }
        });

        let node_id = wait_for_peer_node(&tree).await;
        cancel.cancel();
        runner.abort();

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if transport.cancels.lock().as_slice() == ["task-1"]
                    && status_of(&tree.list().await, &node_id) == NodeState::Cancelled
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached driver must finish remote and local cancellation");
    }

    #[tokio::test]
    async fn cascade_kill_reaches_driver_and_cancels_remote_task() {
        let (rt, tree, _rx) = runtime();
        let transport = Arc::new(Scripted::new(&["working", "working"]));
        let delegate = tokio::spawn({
            let transport = transport.clone();
            async move {
                rt.delegate(
                    &spec("planets", false),
                    TrustTier::Unverified,
                    "call-kill",
                    transport,
                    serde_json::json!({}),
                    CancellationToken::new(),
                    Authorship::AgentComposed,
                    crate::domain::models::SendAuthorization::Unknown,
                )
                .await
            }
        });

        let node_id = wait_for_peer_node(&tree).await;
        let killed = tree
            .cascade_kill(&node_id, Duration::from_secs(1))
            .await
            .expect("cooperative A2A kill succeeds");
        assert!(killed.contains(&node_id));
        assert_eq!(transport.cancels.lock().as_slice(), ["task-1"]);
        let result = delegate.await.expect("delegate task joins");
        assert!(matches!(result, Err(DelegationError::Cancelled)));
    }

    #[tokio::test]
    async fn both_trust_tiers_return_identical_content() {
        // R-D keystone: TrustTier gates admission, never content. The same
        // transcript must yield byte-identical content for Verified and Unverified.
        // Distinct runtimes so the deterministic (peer, task-id) node id does not
        // collide across the two runs.
        let (rt_v, _tv, _rxv) = runtime();
        let verified = rt_v
            .delegate(
                &spec("planets", true),
                TrustTier::Verified,
                "call-v",
                Arc::new(Scripted::new(&["working", "completed"])),
                serde_json::json!({}),
                CancellationToken::new(),
                Authorship::AgentComposed,
                crate::domain::models::SendAuthorization::Unknown,
            )
            .await
            .unwrap();
        let (rt_u, _tu, _rxu) = runtime();
        let unverified = rt_u
            .delegate(
                &spec("planets", false),
                TrustTier::Unverified,
                "call-u",
                Arc::new(Scripted::new(&["working", "completed"])),
                serde_json::json!({}),
                CancellationToken::new(),
                Authorship::AgentComposed,
                crate::domain::models::SendAuthorization::Unknown,
            )
            .await
            .unwrap();
        assert_eq!(verified, unverified);
    }

    #[tokio::test]
    async fn input_required_parks_without_cancelling_or_rejecting() {
        let tree = NodeTree::new();
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(NodeJournal::open_workspace(dir.path()).await.unwrap());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let room: Arc<dyn RoomJournal> =
            Arc::new(NodeRoomJournal::new(journal.clone(), Some(tx.clone())));
        let rt = A2aDelegationRuntime::new(tree.clone(), room, tx);
        let transport = Arc::new(Scripted::new(&["working", "input-required"]));

        let result = rt
            .delegate(
                &spec("planets", false),
                TrustTier::Unverified,
                "call-ir",
                transport.clone(),
                serde_json::json!({}),
                CancellationToken::new(),
                Authorship::AgentComposed,
                crate::domain::models::SendAuthorization::Unknown,
            )
            .await
            .expect("input-required returns the question to the model");
        assert_eq!(result["state"], "input-required");
        assert!(
            result["continue"]
                .as_str()
                .unwrap()
                .starts_with("a2a-continue-")
        );
        assert!(transport.cancels.lock().is_empty());

        let node = tree
            .list()
            .await
            .into_iter()
            .find(|e| e.subagent_type == "a2a-peer")
            .unwrap();
        assert_eq!(node.current_status, NodeState::Waiting);

        let records = journal.load().await.unwrap();
        assert!(records.iter().any(|entry| matches!(
            entry.record,
            JournalRecord::Room(RoomEvent::RemoteEnvelopeDispatched { .. })
        )));
        assert!(!records.iter().any(|entry| matches!(
            entry.record,
            JournalRecord::Room(RoomEvent::RemoteEnvelopeRejected { .. })
        )));

        // A parked question never emits a MessageRefused receipt.
        let mut saw_refused = false;
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::Subagent(env) = event
                && matches!(env.event, SubagentEvent::MessageRefused { .. })
            {
                saw_refused = true;
            }
        }
        assert!(!saw_refused, "a parked question is not a refusal");
    }

    #[tokio::test]
    async fn input_required_resume_is_single_use_and_peer_bound() {
        let (rt, tree, _events) = runtime();
        let peer = spec("clarifier", false);
        let transport = Arc::new(Scripted::new(&[
            "input-required",
            "input-required",
            "completed",
        ]));
        let origin = CancellationToken::new();
        let first = rt
            .delegate(
                &peer,
                TrustTier::Unverified,
                "clarify",
                transport.clone(),
                serde_json::json!({}),
                origin.clone(),
                Authorship::AgentComposed,
                SendAuthorization::Unknown,
            )
            .await
            .unwrap();
        origin.cancel();
        let handle = first["continue"].as_str().unwrap();
        assert_eq!(rt.parked_count(), 1);
        for (target, input, want) in [
            (
                spec("other", false),
                serde_json::json!({"continue":handle,"message":"main"}),
                "continuation handle belongs to another peer",
            ),
            (
                peer.clone(),
                serde_json::json!({"continue":handle}),
                "continuing a remote task needs a message",
            ),
            (
                peer.clone(),
                serde_json::json!({"continue":"unknown","message":"main"}),
                "continuation handle is unknown or already used",
            ),
        ] {
            let error = rt
                .resume(
                    &target,
                    input,
                    CancellationToken::new(),
                    Authorship::AgentComposed,
                    SendAuthorization::Unknown,
                )
                .await
                .unwrap_err();
            assert_eq!(error.to_string(), want);
            assert_eq!(rt.parked_count(), 1);
            assert_eq!(transport.script.lock().len(), 2);
        }
        let second = rt
            .resume(
                &peer,
                serde_json::json!({"continue":handle,"message":"main"}),
                CancellationToken::new(),
                Authorship::AgentComposed,
                SendAuthorization::Unknown,
            )
            .await
            .unwrap();
        let next_handle = second["continue"].as_str().unwrap();
        assert_ne!(handle, next_handle);
        let result = rt
            .resume(
                &peer,
                serde_json::json!({"continue":next_handle,"message":"yes"}),
                CancellationToken::new(),
                Authorship::AgentComposed,
                SendAuthorization::Unknown,
            )
            .await
            .unwrap();
        assert_eq!(result["status"]["state"], "completed");
        assert_eq!(rt.parked_count(), 0);
        let error = rt
            .resume(
                &peer,
                serde_json::json!({"continue":next_handle,"message":"again"}),
                CancellationToken::new(),
                Authorship::AgentComposed,
                SendAuthorization::Unknown,
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "continuation handle is unknown or already used"
        );
        assert_eq!(tree.list().await[0].current_status, NodeState::Completed);
        assert!(transport.cancels.lock().is_empty());
    }

    #[tokio::test]
    async fn cascade_kill_removes_a_parked_handle_and_cancels_without_running() {
        let (rt, tree, _events) = runtime();
        let peer = spec("clarifier", false);
        let transport = Arc::new(Scripted::new(&["input-required"]));
        let first = rt
            .delegate(
                &peer,
                TrustTier::Unverified,
                "clarify",
                transport.clone(),
                serde_json::json!({}),
                CancellationToken::new(),
                Authorship::AgentComposed,
                SendAuthorization::Unknown,
            )
            .await
            .unwrap();
        let node = tree.list().await[0].agent_id.clone();
        let status = tree.status_rx(&node).await.unwrap();
        tree.cascade_kill(&node, Duration::from_secs(1))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while transport.cancels.lock().is_empty() || rt.parked_count() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("parked owner observes kill");
        assert_eq!(transport.cancels.lock().as_slice(), ["task-1"]);
        assert_eq!(*status.borrow(), NodeState::Cancelled);
        let error = rt
            .resume(
                &peer,
                serde_json::json!({"continue":first["continue"],"message":"late"}),
                CancellationToken::new(),
                Authorship::AgentComposed,
                SendAuthorization::Unknown,
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "continuation handle is unknown or already used"
        );
    }

    #[tokio::test]
    async fn bare_kill_op_settles_a_parked_question_without_running() {
        let (rt, tree, _events) = runtime();
        let peer = spec("clarifier", false);
        let transport = Arc::new(Scripted::new(&["input-required"]));
        let first = rt
            .delegate(
                &peer,
                TrustTier::Unverified,
                "clarify",
                transport.clone(),
                serde_json::json!({}),
                CancellationToken::new(),
                Authorship::AgentComposed,
                SendAuthorization::Unknown,
            )
            .await
            .unwrap();
        let node = tree.list().await[0].agent_id.clone();
        let status = tree.status_rx(&node).await.unwrap();
        tree.send_op(&node, Op::Kill).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while transport.cancels.lock().is_empty() || rt.parked_count() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("parked owner observes a bare kill");
        assert_eq!(transport.cancels.lock().as_slice(), ["task-1"]);
        assert_eq!(*status.borrow(), NodeState::Cancelled);
        let error = rt
            .resume(
                &peer,
                serde_json::json!({"continue":first["continue"],"message":"late"}),
                CancellationToken::new(),
                Authorship::AgentComposed,
                SendAuthorization::Unknown,
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "continuation handle is unknown or already used"
        );
    }

    #[tokio::test]
    async fn reconcile_cancels_a_recovered_question_and_journals_the_answer() {
        let (rt, tree, _rx) = runtime();
        let node_id = mint_node_id("planets", "task-1");
        register_suspended(&tree, &node_id).await;
        let transport = Scripted::new(&["working", "input-required"]);
        let error = rt
            .reconcile_suspended(
                &spec("planets", false),
                &node_id,
                &transport,
                CancellationToken::new(),
            )
            .await
            .expect_err("a recovered question has no live caller");
        assert!(matches!(error, DelegationError::Refused { .. }));
        assert_eq!(transport.cancels.lock().as_slice(), ["task-1"]);
        assert_eq!(
            status_of(&tree.list().await, &node_id),
            NodeState::Cancelled
        );
    }

    /// A peer whose task/context ids carry terminal escape sequences.
    ///
    /// Both ids are **remote-chosen** and both land in
    /// `RejectReason::Policy { detail }`, which Story 18.2 renders into the
    /// chat transcript and prints to stdout via `rustain team log`. That last
    /// one is a `println!` — a genuine escape-injection sink that this story
    /// creates.
    struct HostileIds {
        script: Mutex<VecDeque<serde_json::Value>>,
    }

    impl HostileIds {
        fn new() -> Self {
            let make = |state: &str| {
                serde_json::json!({
                    "kind": "task",
                    "id": "task-\u{1b}[2K\rEVIL",
                    "contextId": "ctx-\u{9b}31m\u{7}",
                    "status": {"state": state},
                    "artifacts": [{"parts": [{"kind": "text", "text": "result"}]}]
                })
            };
            Self {
                script: Mutex::new(VecDeque::from([make("working"), make("auth-required")])),
            }
        }
    }

    #[async_trait]
    impl A2aTaskTransport for HostileIds {
        async fn message_send(
            &self,
            _message: serde_json::Value,
        ) -> Result<serde_json::Value, A2aError> {
            Ok(self.script.lock().pop_front().expect("script exhausted"))
        }
        async fn tasks_get(&self, _task_id: &str) -> Result<serde_json::Value, A2aError> {
            Ok(self.script.lock().pop_front().expect("script exhausted"))
        }
        async fn tasks_cancel(&self, task_id: &str) -> Result<serde_json::Value, A2aError> {
            Ok(serde_json::json!({"kind":"task","id":task_id,"status":{"state":"canceled"}}))
        }
    }

    /// AC8 — the outbound sink, entered through the production front door
    /// (`delegate`), asserted on the JOURNAL BYTES.
    #[tokio::test]
    async fn a_hostile_peer_task_id_reaches_the_journal_stripped_and_bounded() {
        use crate::domain::services::transparency::transparency_row;

        let tree = NodeTree::new();
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(NodeJournal::open_workspace(dir.path()).await.unwrap());
        let (tx, _rx) = mpsc::unbounded_channel();
        let room: Arc<dyn RoomJournal> =
            Arc::new(NodeRoomJournal::new(journal.clone(), Some(tx.clone())));
        let rt = A2aDelegationRuntime::new(tree, room, tx);

        let error = rt
            .delegate(
                &spec("planets", false),
                TrustTier::Unverified,
                "call-hostile",
                Arc::new(HostileIds::new()),
                serde_json::json!({}),
                CancellationToken::new(),
                Authorship::AgentComposed,
                crate::domain::models::SendAuthorization::Unknown,
            )
            .await
            .expect_err("auth-required is a refusal");
        assert!(matches!(error, DelegationError::AuthRequired { .. }));

        let entries = journal.load().await.unwrap();
        let rejection = entries
            .iter()
            .find(|entry| {
                matches!(
                    &entry.record,
                    JournalRecord::Room(RoomEvent::RemoteEnvelopeRejected { .. })
                )
            })
            .expect("the refusal is journaled");
        let JournalRecord::Room(RoomEvent::RemoteEnvelopeRejected {
            reason: RejectReason::Policy { detail },
            direction,
            task,
            ..
        }) = &rejection.record
        else {
            unreachable!()
        };

        // Strip-on-WRITE: the bytes on disk are already clean.
        assert!(
            !detail
                .chars()
                .any(|ch| ch.is_control() || ('\u{80}'..='\u{9f}').contains(&ch)),
            "no C0/C1 byte may be journaled: {detail:?}"
        );
        assert!(
            detail.contains("EVIL") && detail.contains("31m"),
            "only the control bytes are removed — the text itself is evidence: {detail}"
        );
        // AC2: this is the outbound half of the direction field.
        assert_eq!(*direction, Direction::Outbound);
        let task = task
            .as_deref()
            .expect("production rejection carries the remote task id");
        assert!(task.len() <= MAX_PEER_ID_BYTES);
        assert!(
            !task
                .chars()
                .any(|ch| ch.is_control() || ('\u{80}'..='\u{9f}').contains(&ch)),
            "task correlation is sanitized before journaling: {task:?}"
        );

        // Strip-on-READ too: a record written by 18.1b would not have been
        // sanitized on the way in, so the projection cleans it again.
        let row = transparency_row(rejection).expect("a rejection projects");
        assert!(
            !row.one_line()
                .chars()
                .any(|ch| ch.is_control() || ('\u{80}'..='\u{9f}').contains(&ch))
        );
        assert_eq!(row.direction, Direction::Outbound);
    }

    /// Positive control for the strip: legitimate text must survive intact.
    /// A sanitizer that mangles unicode is a different bug wearing the fix.
    #[tokio::test]
    async fn ordinary_unicode_survives_the_outbound_strip_unchanged() {
        use crate::domain::services::transparency::{MAX_SUMMARY_BYTES, sanitize_disclosable};

        let legitimate = "تقرير \u{200f}RTL\u{200e} · e\u{301} · 日本語 · 🚀";
        assert_eq!(
            sanitize_disclosable(legitimate, MAX_SUMMARY_BYTES),
            legitimate
        );
    }

    #[tokio::test]
    async fn rejected_state_projects_failed_and_refuses() {
        let (rt, tree, _rx) = runtime();
        let error = rt
            .delegate(
                &spec("planets", false),
                TrustTier::Unverified,
                "call-r",
                Arc::new(Scripted::new(&["working", "rejected"])),
                serde_json::json!({}),
                CancellationToken::new(),
                Authorship::AgentComposed,
                crate::domain::models::SendAuthorization::Unknown,
            )
            .await
            .expect_err("rejected is a refusal");
        assert!(matches!(error, DelegationError::Refused { .. }));
        let node = tree
            .list()
            .await
            .into_iter()
            .find(|e| e.subagent_type == "a2a-peer")
            .unwrap();
        assert_eq!(node.current_status, NodeState::Failed);
    }
    #[tokio::test]
    async fn illegal_projected_state_transition_is_returned() {
        let (rt, _tree, _rx) = runtime();
        let node_id = mint_node_id("planets", "task-1");
        let _control = rt.materialize(&node_id).await.unwrap();
        let peer = spec("planets", false).resolved_identity();
        let transport = Scripted::new(&["completed"]);
        let first =
            TaskSnapshot::from_result(transport.message_send(serde_json::json!({})).await.unwrap())
                .unwrap();

        let error = rt
            .run_lifecycle(
                &node_id,
                &peer,
                "call-illegal",
                &transport,
                first,
                CancellationToken::new(),
            )
            .await
            .expect_err("Created -> Completed must fail loudly");
        assert!(matches!(error, DelegationError::State(_)));
    }

    /// A transport whose `tasks_get` always fails — a peer unreachable after a
    /// restart. `reconcile_suspended` must leave the node `Suspended`.
    struct UnreachableTransport;

    #[async_trait]
    impl A2aTaskTransport for UnreachableTransport {
        async fn message_send(
            &self,
            _message: serde_json::Value,
        ) -> Result<serde_json::Value, A2aError> {
            Err(A2aError::Request("unreachable".to_owned()))
        }
        async fn tasks_get(&self, _task_id: &str) -> Result<serde_json::Value, A2aError> {
            Err(A2aError::Request("unreachable".to_owned()))
        }
        async fn tasks_cancel(&self, _task_id: &str) -> Result<serde_json::Value, A2aError> {
            Err(A2aError::Request("unreachable".to_owned()))
        }
    }

    async fn register_suspended(tree: &NodeTree, node_id: &AgentId) {
        let (command_tx, _rx) = mpsc::channel(1);
        let (status_tx, _) = watch::channel(NodeState::Created);
        let (_, metrics_rx) = watch::channel(AgentMetrics::default());
        tree.register_peer(
            node_id.clone(),
            AgentHandle {
                agent_id: node_id.clone(),
                token: CapabilityTokenId::nil(),
                command_tx,
                cancel_token: CancellationToken::new(),
                depth: 0,
                subagent_type: "a2a-peer".into(),
                spawned_at: 0,
                status: status_tx,
                metrics: metrics_rx,
                isolated: false,
                mailbox_budget: MailboxBudget::new(),
            },
        )
        .await
        .unwrap();
        // Created -> Running -> Suspended, as NodeRecovery leaves an in-flight
        // remote node after a crash restart.
        tree.try_set_state(node_id, NodeState::Running)
            .await
            .unwrap();
        tree.try_set_state(node_id, NodeState::Suspended)
            .await
            .unwrap();
    }

    fn status_of(
        entries: &[crate::infrastructure::subagent::RegistryEntry],
        node: &AgentId,
    ) -> NodeState {
        entries
            .iter()
            .find(|entry| &entry.agent_id == node)
            .map(|entry| entry.current_status)
            .expect("node present")
    }

    #[test]
    fn node_id_round_trips_untrusted_peer_and_task_values() {
        for (peer, task) in [
            ("planets", "task-1"),
            ("org/planets", "root"),
            ("root", "segment//with/slashes"),
        ] {
            let node = mint_node_id(peer, task);
            assert_eq!(
                parse_a2a_node_id(&node),
                Some((peer.to_owned(), task.to_owned()))
            );
        }
        assert_eq!(
            parse_a2a_node_id(&AgentId::from_validated("subagent/x")),
            None
        );
    }

    #[tokio::test]
    async fn reconcile_drives_a_suspended_node_to_terminal() {
        // AC5: after a restart the node recovered as Suspended is reconciled by
        // re-issuing tasks/get and reaches terminal via Suspended -> Running ->
        // Completed (every hop legal).
        let (rt, tree, _rx) = runtime();
        let node_id = mint_node_id("planets", "task-1");
        register_suspended(&tree, &node_id).await;
        assert_eq!(
            status_of(&tree.list().await, &node_id),
            NodeState::Suspended
        );

        let transport = Scripted::new(&["working", "completed"]);
        rt.reconcile_suspended(
            &spec("planets", false),
            &node_id,
            &transport,
            CancellationToken::new(),
        )
        .await
        .expect("reconciliation succeeds");
        assert_eq!(
            status_of(&tree.list().await, &node_id),
            NodeState::Completed
        );
    }

    #[tokio::test]
    async fn reconcile_leaves_node_suspended_when_peer_is_unreachable() {
        // AC5 / NFR70(c): an unreachable peer must NOT flip the node speculatively
        // back to Running — it stays Suspended (honest and observable).
        let (rt, tree, _rx) = runtime();
        let node_id = mint_node_id("planets", "task-1");
        register_suspended(&tree, &node_id).await;
        rt.reconcile_suspended(
            &spec("planets", false),
            &node_id,
            &UnreachableTransport,
            CancellationToken::new(),
        )
        .await
        .expect("unreachable is not an error, it is a deferral");
        assert_eq!(
            status_of(&tree.list().await, &node_id),
            NodeState::Suspended,
            "an unreachable peer must never speculatively resurrect Running"
        );
    }

    /// Story 19.18 `K19`/`K10`/`K13(3)` (in-crate half, recorded as such) —
    /// the asker's ledger over a real journal:
    /// - an item turn renders `you` only when its task AND content hash match
    ///   a dispatch row for THIS peer (`M42` task-only → RED on the altered
    ///   text; `M38` kind-alone → RED on the unrelated task);
    /// - a peer reply turn is journaled once, the second read appends zero
    ///   (`M18`), and a NEW runtime over the same journal — an asker restart —
    ///   appends zero too (`M23`: a process-local set with no durable seed
    ///   journals twice).
    #[tokio::test]
    async fn the_thread_ledger_labels_by_task_and_hash_and_journals_each_turn_once() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(NodeJournal::open_workspace(dir.path()).await.unwrap());
        let runtime = || {
            let (tx, _rx) = mpsc::unbounded_channel();
            let room: Arc<dyn RoomJournal> = Arc::new(NodeRoomJournal::new(journal.clone(), None));
            A2aDelegationRuntime::new(NodeTree::new(), room, tx)
                .with_journal_reader(journal.clone())
        };
        let peer = spec("marcus", false);
        let asker = runtime();
        asker
            .journal_send_dispatch(
                &peer,
                "task-q",
                "question",
                None,
                crate::domain::models::Authorship::Unknown,
                crate::domain::models::SendAuthorization::Unknown,
            )
            .await
            .unwrap();
        let hash = |text: &str| Some(text_content_hash(text));
        let turns = [
            ObservedTurn::Item {
                task: Some("task-q"),
                content_hash: hash("question"),
            },
            ObservedTurn::Item {
                task: Some("task-q"),
                content_hash: hash("words put in the asker's mouth"),
            },
            ObservedTurn::Item {
                task: Some("someone-elses-task"),
                content_hash: hash("question"),
            },
            ObservedTurn::Reply {
                turn: "rr_1",
                answers: Some("ri_q"),
                content: Some("answer"),
                content_hash: hash("answer"),
            },
        ];
        assert_eq!(
            asker.settle_thread(&peer, &turns).await,
            vec![true, false, false, false]
        );
        let observed = |entries: &[crate::domain::models::JournalEntry]| {
            entries
                .iter()
                .filter(|entry| {
                    matches!(
                        &entry.record,
                        JournalRecord::Room(RoomEvent::PeerReplyObserved { turn, content, .. })
                            if turn == "rr_1" && content.as_deref() == Some("answer")
                    )
                })
                .count()
        };
        let reader: Arc<dyn crate::domain::ports::RoomJournalReader> = journal.clone();
        assert_eq!(observed(&reader.load_entries().await.unwrap()), 1);
        asker.settle_thread(&peer, &turns).await;
        assert_eq!(
            observed(&reader.load_entries().await.unwrap()),
            1,
            "second read"
        );
        // A dispatch AFTER the ledger was seeded is a `you` source too.
        asker
            .journal_send_dispatch(
                &peer,
                "task-f",
                "and retries?",
                None,
                crate::domain::models::Authorship::Unknown,
                crate::domain::models::SendAuthorization::Unknown,
            )
            .await
            .unwrap();
        let later = [ObservedTurn::Item {
            task: Some("task-f"),
            content_hash: hash("and retries?"),
        }];
        assert_eq!(asker.settle_thread(&peer, &later).await, vec![true]);
        let restarted = runtime();
        assert_eq!(
            restarted.settle_thread(&peer, &turns).await,
            vec![true, false, false, false]
        );
        assert_eq!(
            observed(&reader.load_entries().await.unwrap()),
            1,
            "an asker restart re-journals nothing"
        );
        // Another peer's dispatch never labels this peer's turn `you`.
        let other = spec("jun", false);
        assert_eq!(
            restarted.settle_thread(&other, &turns[..1]).await,
            vec![false]
        );
    }
}

#[cfg(test)]
mod story_contract_tests {
    use super::*;
    use crate::domain::models::{PeerId, StoryManifest};

    #[test]
    fn story_carriers_match_frozen_recipient_slots() {
        let aliases = vec!["b".to_owned(), "c".to_owned()];
        let pin = PeerId::from_public_key(&[3; 32]).unwrap();
        let manifest = StoryManifest::parse("S1,S2=b,S3=c").unwrap();
        let (recorded, carriers) =
            freeze_stories(Some(&manifest), &aliases, &[None, Some(pin.clone())]);
        assert_eq!(
            serde_json::to_value(recorded).unwrap(),
            serde_json::json!([
                {"key":"S1","to":null},{"key":"S2","to":0},{"key":"S3","to":1}
            ])
        );
        assert_eq!(
            carriers,
            vec![
                Some(
                    serde_json::json!({"stories":[{"key":"S1"},{"key":"S2","you":true},{"key":"S3","alias":"c","peer":pin}]})
                ),
                Some(
                    serde_json::json!({"stories":[{"key":"S1"},{"key":"S2","alias":"b"},{"key":"S3","you":true}]})
                )
            ]
        );
        let (recorded, carriers) = freeze_stories(None, &aliases, &[None, None]);
        assert!(recorded.is_empty());
        assert_eq!(carriers, vec![None, None]);
    }

    #[test]
    fn story_carriers_mark_you_for_every_slot_of_the_holder_host() {
        let pin = PeerId::from_public_key(&[4; 32]).unwrap();
        let other = PeerId::from_public_key(&[5; 32]).unwrap();
        let manifest = StoryManifest::parse("S1=b").unwrap();
        let you = Some(serde_json::json!({"stories":[{"key":"S1","you":true}]}));
        // A repeated alias is one host, whatever its pin.
        let (recorded, carriers) =
            freeze_stories(Some(&manifest), &["b".into(), "b".into()], &[None, None]);
        assert_eq!(
            serde_json::to_value(recorded).unwrap(),
            serde_json::json!([{"key":"S1","to":0}])
        );
        assert_eq!(carriers, vec![you.clone(), you.clone()]);
        // Two aliases pinned to one host both receive `you`.
        let (_, carriers) = freeze_stories(
            Some(&manifest),
            &["b".into(), "c".into()],
            &[Some(pin.clone()), Some(pin.clone())],
        );
        assert_eq!(carriers, vec![you.clone(), you.clone()]);
        // Control: a different pin, or no pin at all, stays a named member.
        for c_pin in [Some(other), None] {
            let (_, carriers) = freeze_stories(
                Some(&manifest),
                &["b".into(), "c".into()],
                &[Some(pin.clone()), c_pin],
            );
            assert_eq!(
                carriers,
                vec![
                    you.clone(),
                    Some(
                        serde_json::json!({"stories":[{"key":"S1","alias":"b","peer":pin.clone()}]})
                    )
                ]
            );
        }
        // An unpinned holder never matches an unpinned recipient by absence.
        let (_, carriers) =
            freeze_stories(Some(&manifest), &["b".into(), "c".into()], &[None, None]);
        assert_eq!(
            carriers[1],
            Some(serde_json::json!({"stories":[{"key":"S1","alias":"b"}]}))
        );
    }

    #[test]
    fn story_metadata_is_explicit_and_omitted_for_followups() {
        let stories = serde_json::json!({ "stories": [{ "key": "S1", "you": true }] });
        for (reply_to, carrier, expected) in [
            (None, Some(&stories), Some(&stories)),
            (None, None, None),
            (Some("rr_1"), Some(&stories), None),
            (Some("rr_1"), None, None),
        ] {
            let (message, _) = build_message(
                &serde_json::json!({
                    "message": "story details",
                    "message_type": "story_assignment",
                    "x-rustain-stories": { "stories": [{ "key": "FORGED" }] },
                }),
                MessageCarriers {
                    continuation: None,
                    selected_for: None,
                    reply_to,
                    item_key: None,
                    reply_capability: None,
                    stories: carrier,
                    authorship: Authorship::Unknown,
                    send_authorization: SendAuthorization::Unknown,
                },
            );
            assert_eq!(
                message["message"]["metadata"].get(super::super::STORIES_METADATA_KEY),
                expected,
            );
        }
    }
}

#[cfg(test)]
mod selection_carrier_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn story19_23_build_message_carries_selection_only_from_parameters_on_first_sends() {
        let selection = json!({"stories":["S7-06"]});
        let carriers = MessageCarriers {
            continuation: None,
            reply_to: None,
            item_key: None,
            reply_capability: None,
            stories: None,
            selected_for: None,
            authorship: Authorship::HumanWritten,
            send_authorization: SendAuthorization::HumanDecided,
        };
        let hostile = json!({"message":"body","x-rustain-selected-for":selection,"metadata":{"x-rustain-selected-for":selection}});
        let (mut plain, _) = build_message(&json!({"message":"body"}), carriers);
        let (mut supplied, _) = build_message(&hostile, carriers);
        plain["message"]
            .as_object_mut()
            .unwrap()
            .remove("messageId");
        supplied["message"]
            .as_object_mut()
            .unwrap()
            .remove("messageId");
        assert_eq!(
            serde_json::to_vec(&plain).unwrap(),
            serde_json::to_vec(&supplied).unwrap()
        );
        assert!(
            plain["message"]["metadata"]
                .get(super::super::SELECTED_FOR_METADATA_KEY)
                .is_none()
        );
        for reply_to in [None, Some("rr-followup")] {
            let (message, _) = build_message(
                &hostile,
                MessageCarriers {
                    continuation: None,
                    selected_for: Some(&selection),
                    reply_to,
                    ..carriers
                },
            );
            assert_eq!(
                message["message"]["metadata"].get(super::super::SELECTED_FOR_METADATA_KEY),
                if reply_to.is_none() {
                    Some(&selection)
                } else {
                    None
                }
            );
        }
    }
}
