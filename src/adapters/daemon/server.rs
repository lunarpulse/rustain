//! Daemon attach server (Story 12.2b AC2/AC3/AC4/AC6).
//!
//! Replaces the 12.1a accept-stub. Owns:
//! - the **accept loop** — handshake (version check), per-connection reader/writer
//!   split, multi-attach (first writer wins; later attaches are read-only);
//! - the single **forwarder** task draining the daemon's per-activation
//!   `domain_rx`, folding the assistant response into the per-process
//!   conversation (the daemon has no `reduce()` loop) and **fanning out** each
//!   projected [`ClientEvent`] to every attached connection's bounded queue
//!   (drop a slow connection, never the turn — AC3);
//! - the **headless approval gate** (AC6): attached-writer → exact review;
//!   retain and reissue without a deadline; unattended → Safe once, otherwise deny;
//!   denied-while-unattended is a durable, visible, resumable transcript record +
//!   a waiting-count.
//!
//! The turn is driven on the daemon-owned bus (not any socket), so an in-flight
//! turn **completes and persists even if the client detaches** (AC4).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use rand_core::{OsRng, RngCore};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Notify, Semaphore, mpsc, watch};
use tokio_util::sync::CancellationToken;

#[cfg(feature = "a2a")]
use crate::adapters::a2a::driver::A2aDelegationRuntime;

#[cfg(feature = "cron")]
use crate::adapters::scheduler::cron::CronCompletion;
#[cfg(not(feature = "cron"))]
pub struct CronCompletion;
use super::protocol::{
    AttachMode, AttachSnapshot, ClientFrame, ConnectionTier, DaemonFrame, FrameError,
    MAX_FRAME_BYTES, PROTOCOL_VERSION, ProposalToken, ProtocolError, RecipientAction,
    RecipientActionOutcome, RecipientActionTarget, RecipientReplyReceipt, ToolInputWithheld,
    WithheldToolInput, read_frame, write_frame,
};
use super::runtime::DaemonCore;
use crate::adapters::rap::{
    ReplayWindow, VerifyError, verify_attach_proof, verify_envelope_reserved,
};
use crate::domain::clock::{Clock, SystemClock};
use crate::domain::events::AppEvent;
use crate::domain::models::{
    AgentId, AgentMessage, ChannelKind, ChannelTurnRequest, ChatMessage, ContentBlockType,
    Conversation, InteractionPolicySnapshot, ItemAddress, MessageRole, NodeState, PeerId,
    PeerRowControl, PeerRowOperation, PeerRowStage, PolicyPrincipal, StopReason, StreamChunk,
    ToolRisk, TurnOrigin, generate_message_id,
};
use crate::domain::ports::StoragePort;
use crate::domain::services::approval_runtime::{ApprovalRuntime, ApprovalRuntimeEvent};
use crate::infrastructure::runtime::event_bus::{RawEvent, RawEventKind};

mod peer_rows;
use peer_rows::{rap_rooted_peer, snapshot_peer_row_controls};
mod rap_inbound;
use rap_inbound::DaemonPeerConsumer;
mod team;
use team::TeamSendPayload;

/// Per-connection bounded writer queue depth. A connection that cannot keep up is
/// dropped (its queue fills) — the turn is never blocked (AC3).
const CONN_QUEUE_DEPTH: usize = 1024;
/// Maximum queued operator commands per attached writer.
const OPERATOR_QUEUE_DEPTH: usize = 64;
/// Maximum daemon-owned `/team send` actions admitted concurrently.
pub const MAX_IN_FLIGHT_TEAM_SENDS: usize = 8;

const CHANNEL_TURN_FAILED_REPLY: &str =
    "Sorry, processing failed before the agent produced a response. Please try again.";
/// Peer-authored transcript row: the provenance header wraps the raw text, a
/// real id is minted (rows persist, so an empty id breaks addressing), and
/// `synthetic: false` — peer utterances are not host-synthesized, so the `⤷`
/// glyph must not render on them.
fn peer_transcript_message(
    text: &str,
    peer: &PeerId,
    provenance: &crate::domain::models::InteractionPolicySnapshot,
) -> ChatMessage {
    ChatMessage {
        id: generate_message_id(),
        role: MessageRole::User,
        content: format!(
            "{}\n{text}\n{}\n{}",
            provenance.peer_header(peer),
            provenance.response_clause(),
            provenance.notification_clause()
        ),
        content_blocks: vec![ContentBlockType::PeerText],
        created_at: crate::domain::models::session_meta::now_unix(),
        synthetic: false,
        history_source: crate::domain::models::HistorySource::PeerDisplay,
        ..Default::default()
    }
}

/// A consolidation proposal set retained by the daemon for token-gated resolve
/// (Story 12.2d AC2/AC4). Keyed by the marker's `queued_at_unix` in
/// `AttachServer::retained_consolidations`.
struct RetainedConsolidation {
    token: super::protocol::ProposalToken,
    /// Story 12.2d AC2 / Fork-C — each proposal carries a stable `ProposalId`
    /// (minted at generation via `.enumerate()`), the forward-compat handle for
    /// the per-item-toggle fast-follow (AI-12.2d-2).
    proposals: Vec<super::protocol::ProposedFact>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TeamFramePublishResult {
    Published,
    Overflow,
    Closed,
}

fn try_publish_team_frame(
    tx: &mpsc::Sender<DaemonFrame>,
    terminal_tx: &watch::Sender<Option<ProtocolError>>,
    frame: DaemonFrame,
) -> TeamFramePublishResult {
    match tx.try_send(frame) {
        Ok(()) => TeamFramePublishResult::Published,
        Err(mpsc::error::TrySendError::Full(_)) => {
            let _ = terminal_tx.send(Some(ProtocolError::Internal(
                "team result queue overflow; connection closed".to_owned(),
            )));
            TeamFramePublishResult::Overflow
        }
        Err(mpsc::error::TrySendError::Closed(_)) => TeamFramePublishResult::Closed,
    }
}

pub(crate) fn publish_team_frame(
    tx: &mpsc::Sender<DaemonFrame>,
    terminal_tx: &watch::Sender<Option<ProtocolError>>,
    frame: DaemonFrame,
) -> bool {
    try_publish_team_frame(tx, terminal_tx, frame) == TeamFramePublishResult::Published
}

#[cfg(any(test, feature = "test-instrumentation"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TeamSendFifoEventKind {
    Enqueued,
    Dequeued,
    Discarded,
    Handled,
}

#[cfg(any(test, feature = "test-instrumentation"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamSendFifoEvent {
    pub request: String,
    pub connection_id: u64,
    pub kind: TeamSendFifoEventKind,
}

#[cfg(any(test, feature = "test-instrumentation"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TeamSendPublicationKind {
    Started,
    Settled { index: usize },
    Refused { action: RecipientAction },
    BoardResult,
    StatusResult,
    ConsentRevoked,
}

#[cfg(any(test, feature = "test-instrumentation"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamSendPublication {
    pub request: String,
    pub kind: TeamSendPublicationKind,
    pub published_to_conn: u64,
}

#[cfg(any(test, feature = "test-instrumentation"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TeamSendTerminalEventKind {
    Error(ProtocolError),
    Closed,
}

#[cfg(any(test, feature = "test-instrumentation"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamSendTerminalEvent {
    pub connection_id: u64,
    pub kind: TeamSendTerminalEventKind,
}

#[cfg(any(test, feature = "test-instrumentation"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TeamSendCompletionKind {
    Finished,
    Cancelled,
}

#[cfg(any(test, feature = "test-instrumentation"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamSendCompletion {
    pub request: String,
    pub kind: TeamSendCompletionKind,
}

/// Story 19.21c: deterministic preview completion ordering, without sleeps.
#[cfg(any(test, feature = "test-instrumentation"))]
pub struct TeamRetractPreviewGate {
    entered: Semaphore,
    release: Semaphore,
}
#[cfg(any(test, feature = "test-instrumentation"))]
impl TeamRetractPreviewGate {
    pub async fn wait_entered(&self) {
        self.entered
            .acquire()
            .await
            .expect("preview entered")
            .forget();
    }
    pub fn release(&self) {
        self.release.add_permits(1);
    }
}

#[cfg(any(test, feature = "test-instrumentation"))]
struct TeamSendTestHooksInner {
    fifo_tx: mpsc::UnboundedSender<TeamSendFifoEvent>,
    fifo_rx: Mutex<mpsc::UnboundedReceiver<TeamSendFifoEvent>>,
    publication_tx: mpsc::UnboundedSender<TeamSendPublication>,
    publication_rx: Mutex<mpsc::UnboundedReceiver<TeamSendPublication>>,
    terminal_tx: mpsc::UnboundedSender<TeamSendTerminalEvent>,
    terminal_rx: Mutex<mpsc::UnboundedReceiver<TeamSendTerminalEvent>>,
    completion_tx: mpsc::UnboundedSender<TeamSendCompletion>,
    completion_rx: Mutex<mpsc::UnboundedReceiver<TeamSendCompletion>>,
    completed_count: AtomicUsize,
    fifo_serial: Mutex<()>,
    hold_before_dequeue: Option<usize>,
    dequeue_count: AtomicUsize,
    worker_gate_count: AtomicUsize,
    worker_gate_notify: Notify,
    overflow_count: AtomicUsize,
    worker_release: Semaphore,
    hold_first_writer: bool,
    writer_claimed: AtomicBool,
    writer_release: Semaphore,
    hold_send_actions: bool,
    send_action_count: AtomicUsize,
    send_action_notify: Notify,
    send_action_release: Semaphore,
    outbound_queue_capacity: AtomicUsize,
    board_now: Option<Arc<dyn Fn() -> std::time::Instant + Send + Sync>>,
    retract_previews: Mutex<std::collections::HashMap<String, Arc<TeamRetractPreviewGate>>>,
}

/// Immutable-before-construction test controls for attached team-send lifecycle
/// observation. These hooks are absent from production builds.
#[cfg(any(test, feature = "test-instrumentation"))]
#[derive(Clone)]
pub struct TeamSendTestHooks {
    inner: Arc<TeamSendTestHooksInner>,
}

#[cfg(any(test, feature = "test-instrumentation"))]
impl TeamSendTestHooks {
    /// Create hooks before constructing the server. `hold_before_dequeue` is a
    /// 1-based TeamSend command ordinal; capacity overrides each connection's
    /// bounded output channel. A held first socket writer drains only after
    /// `release_writer()`.
    pub fn new(
        hold_before_dequeue: Option<usize>,
        outbound_queue_capacity: Option<usize>,
        hold_first_writer: bool,
    ) -> Self {
        assert!(
            outbound_queue_capacity.is_none_or(|capacity| capacity > 0),
            "test outbound queue capacity must be non-zero"
        );
        assert!(
            hold_before_dequeue.is_none_or(|ordinal| ordinal > 0),
            "held TeamSend ordinal is 1-based"
        );
        let (fifo_tx, fifo_rx) = mpsc::unbounded_channel();
        let (publication_tx, publication_rx) = mpsc::unbounded_channel();
        let (terminal_tx, terminal_rx) = mpsc::unbounded_channel();
        let (completion_tx, completion_rx) = mpsc::unbounded_channel();
        Self {
            inner: Arc::new(TeamSendTestHooksInner {
                fifo_tx,
                fifo_rx: Mutex::new(fifo_rx),
                publication_tx,
                publication_rx: Mutex::new(publication_rx),
                terminal_tx,
                terminal_rx: Mutex::new(terminal_rx),
                completion_tx,
                completion_rx: Mutex::new(completion_rx),
                completed_count: AtomicUsize::new(0),
                fifo_serial: Mutex::new(()),
                hold_before_dequeue,
                dequeue_count: AtomicUsize::new(0),
                worker_gate_count: AtomicUsize::new(0),
                worker_gate_notify: Notify::new(),
                overflow_count: AtomicUsize::new(0),
                worker_release: Semaphore::new(0),
                hold_first_writer,
                writer_claimed: AtomicBool::new(false),
                writer_release: Semaphore::new(0),
                hold_send_actions: false,
                send_action_count: AtomicUsize::new(0),
                send_action_notify: Notify::new(),
                send_action_release: Semaphore::new(0),
                outbound_queue_capacity: AtomicUsize::new(outbound_queue_capacity.unwrap_or(0)),
                board_now: None,
                retract_previews: Mutex::new(std::collections::HashMap::new()),
            }),
        }
    }

    /// Supply a monotonic clock for daemon TeamBoard reads. Configure this
    /// before cloning the hooks or constructing the server.
    pub fn with_board_now_source(
        mut self,
        source: Arc<dyn Fn() -> std::time::Instant + Send + Sync>,
    ) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("configure TeamSendTestHooks before cloning")
            .board_now = Some(source);
        self
    }

    /// Hold each admitted TeamSend task immediately before its coordinator.
    /// Use `wait_for_send_action_count` and `release_send_actions` to saturate
    /// the daemon's finite in-flight semaphore deterministically.
    pub fn with_send_action_gate(mut self) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("configure TeamSendTestHooks before cloning")
            .hold_send_actions = true;
        self
    }

    /// Receive the next FIFO observation in order.
    pub async fn next_fifo_event(&self) -> Option<TeamSendFifoEvent> {
        self.inner.fifo_rx.lock().await.recv().await
    }

    /// Receive the next targeted publication observation in order.
    pub async fn next_publication(&self) -> Option<TeamSendPublication> {
        self.inner.publication_rx.lock().await.recv().await
    }
    /// Non-blocking read of the next publication after a lifecycle fence proves
    /// no further old-request frame can be emitted.
    pub async fn try_next_publication(
        &self,
    ) -> Result<TeamSendPublication, mpsc::error::TryRecvError> {
        self.inner.publication_rx.lock().await.try_recv()
    }

    /// Receive the next explicit terminal-error/close observation in order.
    pub async fn next_terminal_event(&self) -> Option<TeamSendTerminalEvent> {
        self.inner.terminal_rx.lock().await.recv().await
    }

    /// Non-blocking read of the next terminal event; useful for proving that a
    /// silent-drop path did not publish a terminal error.
    pub async fn try_next_terminal_event(
        &self,
    ) -> Result<TeamSendTerminalEvent, mpsc::error::TryRecvError> {
        self.inner.terminal_rx.lock().await.try_recv()
    }

    /// Receive the next daemon-owned send-task completion in order.
    pub async fn next_completion(&self) -> Option<TeamSendCompletion> {
        self.inner.completion_rx.lock().await.recv().await
    }

    /// Non-blocking read of the next daemon-owned send-task completion.
    pub async fn try_next_completion(
        &self,
    ) -> Result<TeamSendCompletion, mpsc::error::TryRecvError> {
        self.inner.completion_rx.lock().await.try_recv()
    }

    /// Number of daemon-owned TeamSend tasks that have completed.
    pub fn completed_count(&self) -> usize {
        self.inner.completed_count.load(Ordering::Acquire)
    }

    /// Release the selected command's worker gate.
    pub fn release_worker(&self) {
        self.inner.worker_release.add_permits(1);
    }

    /// Wait until the selected worker command has reached its pre-dispatch gate.
    pub async fn wait_for_worker_gate_count(&self, count: usize) {
        loop {
            let notified = self.inner.worker_gate_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.inner.worker_gate_count.load(Ordering::Acquire) >= count {
                return;
            }
            notified.await;
        }
    }
    /// Number of targeted output queue overflows observed by this server.
    pub fn overflow_count(&self) -> usize {
        self.inner.overflow_count.load(Ordering::Acquire)
    }

    /// Release the first attached socket writer held by this hook.
    pub fn release_writer(&self) {
        self.inner.writer_release.add_permits(1);
    }

    /// Restore production capacity for subsequent healthy reconnections only.
    /// Existing overloaded channels retain their original fixed capacity.
    pub fn restore_default_queue_capacity(&self) {
        self.inner
            .outbound_queue_capacity
            .store(0, Ordering::Release);
    }

    /// Wait until this many admitted send tasks have reached the coordinator
    /// gate. The count is cumulative for this server.
    pub async fn wait_for_send_action_count(&self, count: usize) {
        loop {
            let notified = self.inner.send_action_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.inner.send_action_count.load(Ordering::Acquire) >= count {
                return;
            }
            notified.await;
        }
    }

    pub async fn hold_retract_preview(&self, request: &str) -> Arc<TeamRetractPreviewGate> {
        let gate = Arc::new(TeamRetractPreviewGate {
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
        });
        self.inner
            .retract_previews
            .lock()
            .await
            .insert(request.to_owned(), gate.clone());
        gate
    }
    async fn before_retract_preview(&self, request: &str) {
        let gate = self.inner.retract_previews.lock().await.remove(request);
        if let Some(gate) = gate {
            gate.entered.add_permits(1);
            gate.release
                .acquire()
                .await
                .expect("preview release")
                .forget();
        }
    }

    /// Number of send tasks that reached the optional pre-coordinator gate.
    pub fn send_action_count(&self) -> usize {
        self.inner.send_action_count.load(Ordering::Acquire)
    }

    /// Release up to `count` admitted send tasks held before their coordinator.
    pub fn release_send_actions(&self, count: usize) {
        self.inner.send_action_release.add_permits(count);
    }

    fn record_fifo(&self, request: &str, connection_id: u64, kind: TeamSendFifoEventKind) {
        let _ = self.inner.fifo_tx.send(TeamSendFifoEvent {
            request: request.to_owned(),
            connection_id,
            kind,
        });
    }

    fn record_terminal(&self, connection_id: u64, kind: TeamSendTerminalEventKind) {
        let _ = self.inner.terminal_tx.send(TeamSendTerminalEvent {
            connection_id,
            kind,
        });
    }

    fn record_completion(&self, request: String, kind: TeamSendCompletionKind) {
        self.inner.completed_count.fetch_add(1, Ordering::AcqRel);
        let _ = self
            .inner
            .completion_tx
            .send(TeamSendCompletion { request, kind });
    }

    async fn before_team_send(&self) {
        let ordinal = self.inner.dequeue_count.fetch_add(1, Ordering::AcqRel) + 1;
        if self.inner.hold_before_dequeue == Some(ordinal) {
            self.inner.worker_gate_count.fetch_add(1, Ordering::AcqRel);
            self.inner.worker_gate_notify.notify_waiters();
            if let Ok(permit) = self.inner.worker_release.acquire().await {
                permit.forget();
            }
        }
    }

    async fn before_send_action(&self) {
        if !self.inner.hold_send_actions {
            return;
        }
        self.inner.send_action_count.fetch_add(1, Ordering::AcqRel);
        self.inner.send_action_notify.notify_waiters();
        if let Ok(permit) = self.inner.send_action_release.acquire().await {
            permit.forget();
        }
    }

    async fn before_writer_drain(&self) {
        if self.inner.hold_first_writer
            && !self.inner.writer_claimed.swap(true, Ordering::AcqRel)
            && let Ok(permit) = self.inner.writer_release.acquire().await
        {
            permit.forget();
        }
    }

    fn queue_capacity(&self) -> Option<usize> {
        match self.inner.outbound_queue_capacity.load(Ordering::Acquire) {
            0 => None,
            capacity => Some(capacity),
        }
    }

    fn board_now(&self) -> Option<std::time::Instant> {
        self.inner.board_now.as_ref().map(|source| source())
    }

    fn record_publication(&self, connection_id: u64, frame: &DaemonFrame) {
        let (request, kind) = match frame {
            DaemonFrame::TeamSendStarted { request, .. } => {
                (request, TeamSendPublicationKind::Started)
            }
            DaemonFrame::TeamSendSettled { request, index, .. } => {
                (request, TeamSendPublicationKind::Settled { index: *index })
            }
            DaemonFrame::RecipientActionResult {
                action,
                target: RecipientActionTarget::Request { request },
                outcome: RecipientActionOutcome::Refused { .. },
            } => (
                request,
                TeamSendPublicationKind::Refused { action: *action },
            ),
            DaemonFrame::TeamViewDone { request, .. } => {
                (request, TeamSendPublicationKind::BoardResult)
            }
            DaemonFrame::TeamStatusResult { request, .. } => {
                (request, TeamSendPublicationKind::StatusResult)
            }
            DaemonFrame::SenderConsentRevoked { request, .. } => {
                (request, TeamSendPublicationKind::ConsentRevoked)
            }
            DaemonFrame::TeamViewChunk { .. }
            | DaemonFrame::TeamRetractPreviewed { .. }
            | DaemonFrame::TeamRetractAnswered { .. }
            | DaemonFrame::AttachChallenge { .. }
            | DaemonFrame::AttachAck { .. }
            | DaemonFrame::Event(_)
            | DaemonFrame::ToolEventWithheld { .. }
            | DaemonFrame::FrameTooLarge { .. }
            | DaemonFrame::PeerRowOperation(_)
            | DaemonFrame::History { .. }
            | DaemonFrame::ApprovalRequest { .. }
            | DaemonFrame::ApprovalInputChunk { .. }
            | DaemonFrame::ApprovalInputComplete { .. }
            | DaemonFrame::ApprovalRefused { .. }
            | DaemonFrame::ApprovalClosed { .. }
            | DaemonFrame::ConsolidationProposed { .. }
            | DaemonFrame::TeamProposal { .. }
            | DaemonFrame::TeamProposalClosed { .. }
            | DaemonFrame::PeerAccepted { .. }
            | DaemonFrame::PeerShareResult { .. }
            | DaemonFrame::RecipientActionResult { .. }
            | DaemonFrame::RecipientContextState { .. }
            | DaemonFrame::Detached
            | DaemonFrame::Error(_) => return,
        };
        let _ = self.inner.publication_tx.send(TeamSendPublication {
            request: request.clone(),
            kind,
            published_to_conn: connection_id,
        });
    }
}

fn team_send_request_key(frame: &ClientFrame) -> Option<&str> {
    match frame {
        ClientFrame::TeamSend { request, .. }
        | ClientFrame::TeamFollowup { request, .. }
        | ClientFrame::TeamRetractPreview { request, .. }
        | ClientFrame::TeamRetractDecision { request, .. } => Some(request),
        ClientFrame::TeamProposalDecision { proposal, .. } => Some(proposal),
        _ => None,
    }
}

pub(crate) fn publish_team_frame_to_connection(
    tx: &mpsc::Sender<DaemonFrame>,
    terminal_tx: &watch::Sender<Option<ProtocolError>>,
    frame: DaemonFrame,
    connection_id: u64,
    #[cfg(any(test, feature = "test-instrumentation"))] hooks: Option<&TeamSendTestHooks>,
) -> bool {
    #[cfg(any(test, feature = "test-instrumentation"))]
    let publication_frame = hooks.map(|_| frame.clone());
    #[cfg(not(any(test, feature = "test-instrumentation")))]
    let _ = connection_id;
    let result = try_publish_team_frame(tx, terminal_tx, frame);
    #[cfg(any(test, feature = "test-instrumentation"))]
    if result == TeamFramePublishResult::Overflow
        && let Some(hooks) = hooks
    {
        hooks.inner.overflow_count.fetch_add(1, Ordering::AcqRel);
    }
    #[cfg(any(test, feature = "test-instrumentation"))]
    if let (Some(hooks), Some(frame)) = (hooks, publication_frame.as_ref()) {
        hooks.record_publication(connection_id, frame);
    }
    match result {
        TeamFramePublishResult::Published => true,
        TeamFramePublishResult::Overflow | TeamFramePublishResult::Closed => false,
    }
}

/// Story 19.20c: reserve 1/128 of a frame for identifiers and the event envelope.
/// The final serialized frame is checked too; this is not an executable-input cap.
const TOOL_INPUT_FRAME_BUDGET: usize = MAX_FRAME_BYTES as usize - MAX_FRAME_BYTES as usize / 128;

#[derive(Default)]
struct JsonByteCount(usize);

impl std::io::Write for JsonByteCount {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn serialized_bytes(value: &impl serde::Serialize) -> usize {
    let mut count = JsonByteCount::default();
    // All callers are JSON-compatible domain values, not arbitrary serializers.
    serde_json::to_writer(&mut count, value).expect("attach projection serializes domain values");
    count.0
}

fn event_tool_input(event: &mut RawEvent) -> Option<&mut serde_json::Value> {
    use crate::domain::models::ToolCall;
    match &mut event.kind {
        RawEventKind::Provider(StreamChunk::ToolUse { input, .. })
        | RawEventKind::PeerProvider(StreamChunk::ToolUse { input, .. }) => Some(input),
        RawEventKind::Tool(transition) => {
            let request = match &mut transition.call {
                ToolCall::Validating { request, .. }
                | ToolCall::Scheduled { request, .. }
                | ToolCall::AwaitingApproval { request, .. }
                | ToolCall::Executing { request, .. }
                | ToolCall::Success { request, .. }
                | ToolCall::Error { request, .. }
                | ToolCall::Cancelled { request, .. } => request,
            };
            Some(&mut request.input)
        }
        _ => None,
    }
}

fn withheld_reason(tier: ConnectionTier, bytes: usize) -> Option<ToolInputWithheld> {
    if tier == ConnectionTier::Peer {
        Some(ToolInputWithheld::PeerTier)
    } else if bytes > TOOL_INPUT_FRAME_BUDGET {
        Some(ToolInputWithheld::TooLarge {
            bytes: bytes as u64,
        })
    } else {
        None
    }
}

fn transcript_parts(
    frame: &mut DaemonFrame,
) -> Option<(&mut Vec<ChatMessage>, &mut Vec<WithheldToolInput>)> {
    match frame {
        DaemonFrame::AttachAck { snapshot, .. } => {
            Some((&mut snapshot.transcript, &mut snapshot.withheld_tool_inputs))
        }
        DaemonFrame::History {
            messages,
            withheld_tool_inputs,
            ..
        } => Some((messages, withheld_tool_inputs)),
        _ => None,
    }
}

fn withhold_transcript_input(
    message: &mut ChatMessage,
    call_index: usize,
    withheld: ToolInputWithheld,
    entries: &mut Vec<WithheldToolInput>,
) {
    let call = &mut message.tool_calls[call_index];
    call.input = serde_json::Value::Null;
    entries.push(WithheldToolInput {
        message_id: message.id.clone(),
        tool_call_id: call.id.clone(),
        withheld,
    });
}

/// The attach audience boundary. Does not modify runtime events or retained history.
/// Exact-review frames are deliberately outside this projection.
fn project_attach_frame(mut frame: DaemonFrame, tier: ConnectionTier) -> DaemonFrame {
    if let DaemonFrame::Event(event) = &mut frame
        && let Some(input) = event_tool_input(event)
    {
        let bytes = serialized_bytes(input);
        if let Some(withheld) = withheld_reason(tier, bytes) {
            *input = serde_json::Value::Null;
            let DaemonFrame::Event(event) = frame else {
                unreachable!()
            };
            return DaemonFrame::ToolEventWithheld { event, withheld };
        }
    }
    let mut remaining = Vec::new();
    if let Some((messages, entries)) = transcript_parts(&mut frame) {
        for (message_index, message) in messages.iter_mut().enumerate() {
            if tier == ConnectionTier::Peer {
                message.send_authorization = crate::domain::models::SendAuthorization::Unknown;
            }
            for call_index in 0..message.tool_calls.len() {
                let call = &message.tool_calls[call_index];
                if entries
                    .iter()
                    .any(|entry| entry.message_id == message.id && entry.tool_call_id == call.id)
                {
                    continue;
                }
                let bytes = serialized_bytes(&call.input);
                if let Some(withheld) = withheld_reason(tier, bytes) {
                    withhold_transcript_input(message, call_index, withheld, entries);
                } else if !call.input.is_null() {
                    remaining.push((bytes, message_index, call_index));
                }
            }
        }
    }
    // Several individually fitting inputs can overflow a snapshot/page together.
    // Remove the largest first, retaining every other transcript field.
    if !remaining.is_empty() && serialized_bytes(&frame) > MAX_FRAME_BYTES as usize {
        remaining.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.0));
        for (bytes, message_index, call_index) in remaining {
            let (messages, entries) = transcript_parts(&mut frame).expect("transcript frame");
            withhold_transcript_input(
                &mut messages[message_index],
                call_index,
                ToolInputWithheld::TooLarge {
                    bytes: bytes as u64,
                },
                entries,
            );
            if serialized_bytes(&frame) <= MAX_FRAME_BYTES as usize {
                break;
            }
        }
    }
    frame
}

fn frame_variant_name(frame: &DaemonFrame) -> &'static str {
    match frame {
        DaemonFrame::AttachChallenge { .. } => "AttachChallenge",
        DaemonFrame::AttachAck { .. } => "AttachAck",
        DaemonFrame::Event(_) => "Event",
        DaemonFrame::ToolEventWithheld { .. } => "ToolEventWithheld",
        DaemonFrame::FrameTooLarge { .. } => "FrameTooLarge",
        DaemonFrame::PeerRowOperation(_) => "PeerRowOperation",
        DaemonFrame::History { .. } => "History",
        DaemonFrame::ApprovalRequest { .. } => "ApprovalRequest",
        DaemonFrame::ApprovalInputChunk { .. } => "ApprovalInputChunk",
        DaemonFrame::ApprovalInputComplete { .. } => "ApprovalInputComplete",
        DaemonFrame::ApprovalRefused { .. } => "ApprovalRefused",
        DaemonFrame::ApprovalClosed { .. } => "ApprovalClosed",
        DaemonFrame::ConsolidationProposed { .. } => "ConsolidationProposed",
        DaemonFrame::PeerAccepted { .. } => "PeerAccepted",
        DaemonFrame::PeerShareResult { .. } => "PeerShareResult",
        DaemonFrame::RecipientActionResult { .. } => "RecipientActionResult",
        DaemonFrame::RecipientContextState { .. } => "RecipientContextState",
        DaemonFrame::TeamProposal { .. } => "TeamProposal",
        DaemonFrame::TeamProposalClosed { .. } => "TeamProposalClosed",
        DaemonFrame::TeamSendStarted { .. } => "TeamSendStarted",
        DaemonFrame::TeamSendSettled { .. } => "TeamSendSettled",
        DaemonFrame::TeamViewChunk { .. } => "TeamViewChunk",
        DaemonFrame::TeamViewDone { .. } => "TeamViewDone",
        DaemonFrame::TeamRetractPreviewed { .. } => "TeamRetractPreviewed",
        DaemonFrame::TeamRetractAnswered { .. } => "TeamRetractAnswered",
        DaemonFrame::TeamStatusResult { .. } => "TeamStatusResult",
        DaemonFrame::SenderConsentRevoked { .. } => "SenderConsentRevoked",
        DaemonFrame::Detached => "Detached",
        DaemonFrame::Error(_) => "Error",
    }
}

/// Bound only the attach copy, preserving complete messages and all metadata.
/// An oversized envelope must fail explicitly, not masquerade as an AttachAck.
fn bound_attach_snapshot(frame: &mut DaemonFrame) -> Option<u64> {
    let original_bytes = serialized_bytes(frame);
    if original_bytes <= MAX_FRAME_BYTES as usize {
        return None;
    }
    let DaemonFrame::AttachAck { snapshot, .. } = frame else {
        return None;
    };
    let mut estimated_bytes = original_bytes;
    let mut remove = 0;
    for message in &snapshot.transcript {
        if estimated_bytes <= MAX_FRAME_BYTES as usize {
            break;
        }
        // Excluding commas and withheld entries makes the estimate conservative.
        let omitted_inputs = message
            .tool_calls
            .iter()
            .filter(|call| call.input.is_null())
            .count();
        let message_bytes = serialized_bytes(message).saturating_sub(omitted_inputs * 13);
        estimated_bytes = estimated_bytes.saturating_sub(message_bytes);
        remove += 1;
    }
    snapshot.transcript.drain(..remove);
    snapshot.withheld_tool_inputs.retain(|entry| {
        snapshot
            .transcript
            .iter()
            .any(|message| message.id == entry.message_id)
    });
    Some(original_bytes as u64)
}

async fn write_attach_frame<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &DaemonFrame,
) -> Result<(), FrameError> {
    match write_frame(writer, frame).await {
        Err(FrameError::TooLarge(bytes, _)) => {
            write_frame(
                writer,
                &DaemonFrame::FrameTooLarge {
                    frame: frame_variant_name(frame).to_owned(),
                    bytes: u64::from(bytes),
                },
            )
            .await
        }
        result => result,
    }
}

/// One attached connection's outbound handle + grant.
struct Conn {
    id: u64,
    tx: mpsc::Sender<DaemonFrame>,
    terminal_tx: watch::Sender<Option<ProtocolError>>,
    mode: AttachMode,
    tier: ConnectionTier,
    /// Exact approvals may be replayed only after this connection's AttachAck.
    approval_ready: bool,
}

/// The set of live attachments. The writer slot is held by the first `ReadWrite`
/// connection; later attaches are granted `ReadOnly` (multi-attach).
#[derive(Default)]
struct ConnRegistry {
    conns: Vec<Conn>,
}

impl ConnRegistry {
    fn has_writer(&self) -> bool {
        self.conns.iter().any(|c| c.mode == AttachMode::ReadWrite)
    }

    /// The outbound queue of the current writer, if any.
    fn writer_tx(&self) -> Option<mpsc::Sender<DaemonFrame>> {
        self.conns
            .iter()
            .find(|c| c.mode == AttachMode::ReadWrite)
            .map(|c| c.tx.clone())
    }

    fn approval_writer(&self) -> Option<&Conn> {
        self.conns.iter().find(|conn| {
            conn.mode == AttachMode::ReadWrite
                && conn.tier == ConnectionTier::TrustedLocal
                && conn.terminal_tx.borrow().is_none()
        })
    }

    /// Fan a committed peer-row operation only to trusted-local attachments.
    fn fanout_peer_row(&self, frame: &DaemonFrame) -> Vec<u64> {
        let mut dead = Vec::new();
        for conn in self
            .conns
            .iter()
            .filter(|conn| conn.tier == ConnectionTier::TrustedLocal)
        {
            match conn.tx.try_send(frame.clone()) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_))
                | Err(mpsc::error::TrySendError::Closed(_)) => dead.push(conn.id),
            }
        }
        dead
    }

    /// Project once per audience, preserving the existing slow/closed queue policy.
    fn fanout(&self, frame: &DaemonFrame) -> Vec<u64> {
        let mut dead = Vec::new();
        for tier in [ConnectionTier::TrustedLocal, ConnectionTier::Peer] {
            let mut connections = self
                .conns
                .iter()
                .filter(|conn| conn.tier == tier)
                .peekable();
            if connections.peek().is_none() {
                continue;
            }
            let projected = project_attach_frame(frame.clone(), tier);
            for conn in connections {
                match conn.tx.try_send(projected.clone()) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_))
                    | Err(mpsc::error::TrySendError::Closed(_)) => dead.push(conn.id),
                }
            }
        }
        dead
    }

    fn remove(&mut self, id: u64) {
        self.conns.retain(|c| c.id != id);
    }
}

enum PeerRowMutation {
    Upsert {
        message: ChatMessage,
        control: Option<PeerRowControl>,
    },
    ReplaceContent {
        row_id: String,
        content: String,
        authorship: crate::domain::models::Authorship,
        send_authorization: crate::domain::models::SendAuthorization,
        control: Option<PeerRowControl>,
    },
    Remove {
        row_id: String,
    },
}

struct PeerRowUndo {
    row_id: String,
    message: Option<(usize, ChatMessage)>,
    control: Option<PeerRowControl>,
}

#[derive(Default)]
struct PeerAssistantBuffer {
    text: String,
    tools: Vec<crate::domain::models::ToolCallInfo>,
}

fn rollback_peer_rows(conversation: &mut Conversation, undo: Vec<PeerRowUndo>, revision: u64) {
    for undo in undo.into_iter().rev() {
        if let Some(index) = conversation
            .messages
            .iter()
            .position(|message| message.id == undo.row_id)
        {
            conversation.messages.remove(index);
        }
        if let Some((index, message)) = undo.message {
            conversation
                .messages
                .insert(index.min(conversation.messages.len()), message);
        }
        match undo.control {
            Some(control) => {
                conversation.peer_row_controls.insert(undo.row_id, control);
            }
            None => {
                conversation.peer_row_controls.remove(&undo.row_id);
            }
        }
    }
    conversation.peer_row_revision = revision;
}

async fn commit_peer_rows(
    row_serial: &Mutex<()>,
    conversation: &Mutex<Conversation>,
    registry: &Mutex<ConnRegistry>,
    storage: &dyn StoragePort,
    mutations: Vec<PeerRowMutation>,
) -> Result<(), String> {
    let _guard = row_serial.lock().await;
    commit_peer_rows_serialized(conversation, registry, storage, mutations).await
}

async fn commit_peer_rows_serialized(
    conversation: &Mutex<Conversation>,
    registry: &Mutex<ConnRegistry>,
    storage: &dyn StoragePort,
    mutations: Vec<PeerRowMutation>,
) -> Result<(), String> {
    let mut conversation = conversation.lock().await;
    let starting_revision = conversation.peer_row_revision;
    let mut undo = Vec::with_capacity(mutations.len());
    let mut operations = Vec::with_capacity(mutations.len());

    for mutation in mutations {
        let (row_id, requires_existing) = match &mutation {
            PeerRowMutation::Upsert { message, .. } => (message.id.clone(), false),
            PeerRowMutation::Remove { row_id } | PeerRowMutation::ReplaceContent { row_id, .. } => {
                (row_id.clone(), true)
            }
        };
        let old_index = conversation
            .messages
            .iter()
            .position(|message| message.id == row_id);
        if requires_existing && old_index.is_none() {
            continue;
        }
        let Some(revision) = conversation.peer_row_revision.checked_add(1) else {
            rollback_peer_rows(&mut conversation, undo, starting_revision);
            return Err("peer row revision exhausted".to_owned());
        };
        conversation.peer_row_revision = revision;
        let old_control = conversation.peer_row_controls.remove(&row_id);
        let old_message = match mutation {
            PeerRowMutation::Upsert {
                mut message,
                control,
            } => {
                message.history_source = crate::domain::models::HistorySource::PeerDisplay;
                let public_index = old_index.unwrap_or(conversation.messages.len());
                let old_message = if let Some(index) = old_index {
                    Some((
                        index,
                        std::mem::replace(&mut conversation.messages[index], message),
                    ))
                } else {
                    conversation.messages.push(message);
                    None
                };
                if let Some(control) = &control {
                    conversation
                        .peer_row_controls
                        .insert(row_id.clone(), control.clone());
                }
                operations.push(PeerRowOperation::Upsert {
                    revision,
                    message: crate::infrastructure::runtime::event_bus::public_transcript_message(
                        &conversation.messages[public_index],
                    ),
                    control,
                });
                old_message
            }
            PeerRowMutation::ReplaceContent {
                content,
                control,
                authorship,
                send_authorization,
                ..
            } => {
                let index = old_index.expect("existing row checked before revision allocation");
                let message = &mut conversation.messages[index];
                let old_content = std::mem::take(&mut message.content);
                let mut original = message.clone();
                original.content = old_content;
                message.content = content;
                message.authorship = authorship;
                message.send_authorization = send_authorization;
                message.retracted_at_ms = None;
                message.history_source = crate::domain::models::HistorySource::PeerDisplay;
                let replacement = message.clone();
                if let Some(control) = &control {
                    conversation
                        .peer_row_controls
                        .insert(row_id.clone(), control.clone());
                }
                operations.push(PeerRowOperation::Upsert {
                    revision,
                    message: replacement,
                    control,
                });
                Some((index, original))
            }
            PeerRowMutation::Remove { row_id } => {
                let old_message =
                    old_index.map(|index| (index, conversation.messages.remove(index)));
                operations.push(PeerRowOperation::Remove { revision, row_id });
                old_message
            }
        };
        undo.push(PeerRowUndo {
            row_id,
            message: old_message,
            control: old_control,
        });
    }
    if operations.is_empty() {
        return Ok(());
    }
    if let Err(error) = storage.save_conversation(&conversation).await {
        rollback_peer_rows(&mut conversation, undo, starting_revision);
        return Err(error.to_string());
    }
    drop(conversation);

    let mut registry = registry.lock().await;
    for operation in operations {
        let frame = DaemonFrame::PeerRowOperation(Arc::new(operation));
        let dead = registry.fanout_peer_row(&frame);
        for id in dead {
            registry.remove(id);
        }
    }
    Ok(())
}
async fn remove_pending_peer_row(
    row_serial: &Mutex<()>,
    conversation: &Mutex<Conversation>,
    registry: &Mutex<ConnRegistry>,
    storage: &dyn StoragePort,
    node_id: &AgentId,
) -> Result<(), String> {
    let _guard = row_serial.lock().await;
    remove_pending_peer_row_serialized(conversation, registry, storage, node_id).await
}

async fn remove_pending_peer_row_serialized(
    conversation: &Mutex<Conversation>,
    registry: &Mutex<ConnRegistry>,
    storage: &dyn StoragePort,
    node_id: &AgentId,
) -> Result<(), String> {
    let row_id = format!("peer-response-{}", node_id.as_str());
    let pending = {
        let conversation = conversation.lock().await;
        conversation
            .peer_row_controls
            .get(&row_id)
            .is_some_and(|control| {
                control.node_id == node_id.as_str()
                    && matches!(
                        control.stage,
                        PeerRowStage::AwaitingOperator
                            | PeerRowStage::Drafting
                            | PeerRowStage::Ready
                    )
            })
    };
    if !pending {
        return Ok(());
    }
    commit_peer_rows_serialized(
        conversation,
        registry,
        storage,
        vec![PeerRowMutation::Remove { row_id }],
    )
    .await
}

async fn clear_conversation_rows(
    pending_drafts: &super::response_modes::PendingDraftController,
    row_serial: &Mutex<()>,
    conversation: &Mutex<Conversation>,
    registry: &Mutex<ConnRegistry>,
    storage: &dyn StoragePort,
) -> Result<(), String> {
    let _guard = row_serial.lock().await;
    let mut conversation = conversation.lock().await;
    let starting_revision = conversation.peer_row_revision;
    let mut revision = starting_revision;
    let row_ids: std::collections::BTreeSet<String> = conversation
        .messages
        .iter()
        .filter(|message| message.is_peer_display())
        .map(|message| message.id.clone())
        .chain(conversation.peer_row_controls.keys().cloned())
        .collect();
    let mut operations = Vec::with_capacity(row_ids.len());
    for row_id in row_ids {
        revision = revision
            .checked_add(1)
            .ok_or_else(|| "peer row revision exhausted".to_owned())?;
        operations.push(PeerRowOperation::Remove { revision, row_id });
    }

    let messages = std::mem::take(&mut conversation.messages);
    let controls = std::mem::take(&mut conversation.peer_row_controls);
    conversation.peer_row_revision = revision;
    if let Err(error) = storage.save_conversation(&conversation).await {
        conversation.messages = messages;
        conversation.peer_row_controls = controls;
        conversation.peer_row_revision = starting_revision;
        return Err(error.to_string());
    }
    drop(conversation);

    for control in controls.values() {
        pending_drafts.abandon(&control.node_id).await;
    }
    let mut registry = registry.lock().await;
    for operation in operations {
        let frame = DaemonFrame::PeerRowOperation(Arc::new(operation));
        for id in registry.fanout_peer_row(&frame) {
            registry.remove(id);
        }
    }
    Ok(())
}

fn peer_row_control(
    node_id: &AgentId,
    item_address: Option<ItemAddress>,
    provenance: &InteractionPolicySnapshot,
    fallback_label: &str,
    stage: PeerRowStage,
    node_resolvable: bool,
) -> PeerRowControl {
    PeerRowControl {
        node_id: node_id.as_str().to_owned(),
        item_address,
        message_type: provenance.message_type,
        sender_label: provenance
            .sender_label
            .clone()
            .unwrap_or_else(|| fallback_label.to_owned()),
        stage,
        node_resolvable,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PeerDraftResolutionOutcome {
    Applied {
        reply: Option<RecipientReplyReceipt>,
    },
    Idempotent,
    RefusedOneWay,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecipientAckGate {
    Allow,
    ReadOnly,
    UntrustedTier,
    RoleDenied,
}

fn recipient_ack_gate(
    mode: AttachMode,
    tier: ConnectionTier,
    acting: &AgentId,
) -> RecipientAckGate {
    if tier != ConnectionTier::TrustedLocal {
        return RecipientAckGate::UntrustedTier;
    }
    if mode != AttachMode::ReadWrite {
        return RecipientAckGate::ReadOnly;
    }
    let role = crate::domain::services::room_role::local_room_role(acting);
    if crate::domain::services::room_role::room_edit_decision(
        role,
        crate::domain::models::RoomEditKind::DurableContent,
    ) != crate::domain::models::RoomEditDecision::Allow
    {
        return RecipientAckGate::RoleDenied;
    }
    RecipientAckGate::Allow
}

fn recipient_command_gate(
    mode: AttachMode,
    tier: ConnectionTier,
    noun: &str,
    durable: bool,
) -> Result<(), ProtocolError> {
    match recipient_ack_gate(mode, tier, &AgentId::local_operator()) {
        RecipientAckGate::Allow => Ok(()),
        RecipientAckGate::ReadOnly => Err(ProtocolError::ReadOnly),
        RecipientAckGate::UntrustedTier => Err(ProtocolError::PeerVerification(format!(
            "recipient {noun} is same-host trusted-local only"
        ))),
        RecipientAckGate::RoleDenied => {
            let lifetime = if durable { "durable " } else { "" };
            Err(ProtocolError::PeerVerification(format!(
                "room role does not permit {lifetime}recipient {noun}"
            )))
        }
    }
}

/// The daemon attach server. Holds the lazily-built core, the per-process
/// conversation, the connection registry, and the daemon-owned event bus.
/// The shipped not-found sentence every recipient-item act refuses with.
const RECIPIENT_ITEM_NOT_FOUND: &str = "recipient item not found";
/// The shipped tombstone sentence every recipient-item act refuses with.
const RECIPIENT_ITEM_REMOVED: &str = "recipient item already removed";
/// The conversation-row id prefix of a peer-response card.
#[cfg(feature = "a2a")]
const PEER_RESPONSE_ROW_PREFIX: &str = "peer-response-";

pub struct AttachServer {
    core: Arc<DaemonCore>,
    conversation: Arc<Mutex<Conversation>>,
    /// Serializes row mutation, durable save, publication, and attach snapshots.
    peer_row_serial: Arc<Mutex<()>>,
    registry: Arc<Mutex<ConnRegistry>>,
    domain_tx: mpsc::UnboundedSender<AppEvent>,
    node_tree: crate::infrastructure::subagent::NodeTree,
    /// Tool actions denied while unattended, awaiting the user to resume (AC6 #5).
    blocked_waiting: Arc<AtomicUsize>,
    next_conn_id: AtomicU64,
    /// Connection id of the most recently granted ReadWrite attachment.
    last_writer_conn_id: AtomicU64,
    approval_gate: Arc<ApprovalGate>,
    turn_serial: Arc<Mutex<()>>,
    turn_complete: Arc<Notify>,
    /// A private fence for peer display persistence, separate from the public
    /// assistant-turn completion signal.
    peer_turn_closed: Arc<Notify>,
    /// Story 12.3 — current turn origin read by `commit_assistant_turn`.
    /// tokio::sync — locks=4 held.
    active_channel_origin: Arc<Mutex<ChannelKind>>,
    peer_assistant_buffers: Mutex<std::collections::HashMap<Arc<AgentId>, PeerAssistantBuffer>>,
    /// Story 12.3 — response route for the current channel-originated turn.
    /// tokio::sync — locks=4 held.
    pending_channel_response_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<String>>>>,
    /// Story 12.2d AC2/AC6 — retained consolidation proposals keyed by the marker's
    /// `queued_at_unix` (NOT `daily_log_ref`). Reused on re-attach (G7), evicted on
    /// new marker or resolve (G8). tokio::sync per locks=4 discipline (AC7).
    retained_consolidations: Arc<Mutex<std::collections::HashMap<u64, RetainedConsolidation>>>,
    /// Monotonic counter for minting `ProposalToken`s (Story 12.2d AC2).
    next_proposal_token: Arc<AtomicU64>,
    /// Story 12.2d code-review P3 — `queued_at_unix` values with an in-flight
    /// generation. A re-attach during the generation window (before the retained
    /// entry lands) checks this set so it does NOT spawn a duplicate
    /// `generate_proposals` call (G7: exactly one call per marker).
    generating_consolidations: Arc<Mutex<std::collections::HashSet<u64>>>,
    /// Server-wide peer replay window (Story 17.1a). Shared across ALL
    /// connections so an envelope accepted on one connection cannot be replayed
    /// on a reconnect. Locked only across the synchronous `verify_envelope`
    /// call — never held across an await. tokio::sync per locks=4 discipline.
    replay: Arc<Mutex<ReplayWindow>>,
    /// Wall-clock source for peer-envelope TTL checks at the verify seam
    /// (Story 17.1a). Defaults to `SystemClock` in production; tests inject a
    /// `MockClock` via [`AttachServer::with_clock`] to drive TTL deterministically.
    clock: Arc<dyn Clock>,
    /// Post-verification peer-frame delivery seam. It is configured only by
    /// the daemon composition root; test servers remain deliberately inert
    /// unless they opt in.
    peer_delivery:
        Arc<tokio::sync::RwLock<Option<Arc<crate::adapters::rap::VerifiedPeerFrameHandler>>>>,
    /// Story 18.3 (AC3) — the ONE authoritative `AgentMessageBus` slot for this
    /// daemon, supplied by the composition root rather than minted here.
    ///
    /// `with_clock_and_node_tree` used to build its own `LocalMessageBus` +
    /// `RelationshipDeliveryPolicy` privately, so the peer path was governed by a
    /// policy object no composition root held a reference to, and the only seam
    /// that could have reconciled it (`configure_peer_delivery`) had ZERO callers
    /// repo-wide. A `DeliveryPolicy` installed anywhere else would therefore have
    /// silently failed to govern the RAP peer path — the one direction a peer
    /// policy exists to govern.
    ///
    /// Retained (not just forwarded to the frame handler) so a test can prove the
    /// SAME object governs both routes.
    peer_bus: PeerBusSlot,
    /// Same policy object as the one installed in `peer_bus`, retained for the
    /// direct A2A front door's response-mode query.
    delivery_policy: Arc<dyn crate::domain::ports::DeliveryPolicy>,
    pending_drafts: Arc<super::response_modes::PendingDraftController>,
    /// Serializes card resolutions with direct replies so the node-to-turn
    /// check and append cannot race another operator connection.
    peer_reply_serial: Arc<Mutex<()>>,
    pending_consent: Option<Arc<super::consent::PendingConsentManager>>,
    pending_consent_cards:
        Arc<Mutex<std::collections::HashMap<crate::domain::models::RequestId, DaemonFrame>>>,
    pending_peer_nodes: Arc<Mutex<std::collections::HashMap<AgentId, PreparedPeerNode>>>,
    urgency_router: Option<Arc<super::urgency::UrgencyRouter>>,
    operator_turn_active: std::sync::atomic::AtomicBool,
    room_journal: Option<Arc<dyn crate::domain::ports::RoomJournal>>,
    /// Read half of the same journal — the retract path derives the disclosed
    /// `target_seq` from it instead of trusting the client's frame (AC5).
    room_journal_reader: Option<Arc<dyn crate::domain::ports::RoomJournalReader>>,
    #[cfg(feature = "a2a")]
    a2a_runtime: Option<Arc<A2aDelegationRuntime>>,
    team_peers: Vec<crate::domain::models::A2aPeerSpec>,
    team_proposals: Arc<super::team_proposal::TeamProposalRegistry>,
    team_send_slots: Arc<Semaphore>,
    team_send_shutdown: CancellationToken,
    team_send_accepting: AtomicBool,
    team_send_tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Story 19.21c: connection-owned previews shared with joined workers.
    #[cfg(feature = "a2a")]
    team_retracts: Arc<Mutex<std::collections::HashMap<u64, team::RetractSlot>>>,
    team_send_started_count: Arc<AtomicUsize>,
    team_send_settled_count: Arc<AtomicUsize>,
    team_send_saturation_refusals: Arc<AtomicUsize>,
    #[cfg(any(test, feature = "test-instrumentation"))]
    team_send_test_hooks: Option<TeamSendTestHooks>,
    /// Story 18.1b — the assistant answer produced by each inbound A2A peer
    /// node, captured at the moment its turn finished.
    ///
    /// Keyed by node id rather than read back off the shared conversation: the
    /// daemon has ONE conversation and several origins push into it, so "the
    /// last assistant message" is not reliably *this* task's answer. Capturing
    /// at completion is; guessing later is how a peer ends up reading another
    /// channel's reply.
    inbound_results:
        Arc<Mutex<std::collections::HashMap<AgentId, crate::domain::ports::StagedPeerResult>>>,
}

struct PreparedPeerNode {
    command_rx: mpsc::Receiver<crate::domain::models::Op>,
    mailbox_budget: crate::infrastructure::subagent::MailboxBudget,
}

/// Story 19.20c: every gate and its runtime are daemon-owned, including gates
/// first armed by an inbound turn. Exact transfers use nonblocking queue writes
/// inside the gate, so joining the gate also joins all input delivery.
struct ApprovalGate {
    tasks: Mutex<Vec<(Arc<ApprovalRuntime>, tokio::task::JoinHandle<()>)>>,
    shutdown: CancellationToken,
    replay: watch::Sender<u64>,
    issues: Mutex<ApprovalIssues>,
    #[cfg(any(test, feature = "test-instrumentation"))]
    active: AtomicUsize,
    #[cfg(any(test, feature = "test-instrumentation"))]
    replay_hold: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    #[cfg(any(test, feature = "test-instrumentation"))]
    replay_entered: Semaphore,
    #[cfg(any(test, feature = "test-instrumentation"))]
    replay_completed: Semaphore,
    #[cfg(any(test, feature = "test-instrumentation"))]
    transfer_attempts: Semaphore,
    #[cfg(any(test, feature = "test-instrumentation"))]
    exit_hold: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    #[cfg(any(test, feature = "test-instrumentation"))]
    exit_entered: Semaphore,
}

#[derive(Default)]
struct ApprovalIssues {
    /// None means retained; Some names the connection that received the card.
    issued: std::collections::HashMap<crate::domain::models::RequestId, Option<u64>>,
    deciding: std::collections::HashSet<crate::domain::models::RequestId>,
    /// Sender-consent replies retain their legacy idempotent/no-reply behavior,
    /// even after the card retires. Keep only identity, never its input or card.
    consent_ids: std::collections::HashSet<crate::domain::models::RequestId>,
}

impl Default for ApprovalGate {
    fn default() -> Self {
        Self {
            tasks: Mutex::new(Vec::new()),
            shutdown: CancellationToken::new(),
            replay: watch::channel(0).0,
            issues: Mutex::new(ApprovalIssues::default()),
            #[cfg(any(test, feature = "test-instrumentation"))]
            active: AtomicUsize::new(0),
            #[cfg(any(test, feature = "test-instrumentation"))]
            replay_hold: Mutex::new(None),
            #[cfg(any(test, feature = "test-instrumentation"))]
            replay_entered: Semaphore::new(0),
            #[cfg(any(test, feature = "test-instrumentation"))]
            replay_completed: Semaphore::new(0),
            #[cfg(any(test, feature = "test-instrumentation"))]
            transfer_attempts: Semaphore::new(0),
            #[cfg(any(test, feature = "test-instrumentation"))]
            exit_hold: Mutex::new(None),
            #[cfg(any(test, feature = "test-instrumentation"))]
            exit_entered: Semaphore::new(0),
        }
    }
}

#[cfg(any(test, feature = "test-instrumentation"))]
struct ApprovalGateRunGuard<'a>(&'a AtomicUsize);

#[cfg(any(test, feature = "test-instrumentation"))]
impl Drop for ApprovalGateRunGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(any(test, feature = "test-instrumentation"))]
struct ApprovalTransferAttempt<'a>(&'a Semaphore);

#[cfg(any(test, feature = "test-instrumentation"))]
impl Drop for ApprovalTransferAttempt<'_> {
    fn drop(&mut self) {
        self.0.add_permits(1);
    }
}

impl ApprovalGate {
    async fn shutdown(&self, registry: &Mutex<ConnRegistry>) {
        let tasks = {
            let mut tasks = self.tasks.lock().await;
            self.shutdown.cancel();
            std::mem::take(&mut *tasks)
        };
        for (runtime, _) in &tasks {
            runtime
                .cancel_all(
                    crate::domain::services::approval_runtime::CancelReason::RuntimeShutdown,
                )
                .await;
        }
        for (_, task) in tasks {
            let _ = task.await;
        }
        let ids: Vec<_> = self.issues.lock().await.issued.keys().cloned().collect();
        for id in ids {
            close_tool_approval(self, registry, &id).await;
        }
        self.issues.lock().await.consent_ids.clear();
    }

    async fn runtimes(&self) -> Vec<Arc<ApprovalRuntime>> {
        self.tasks
            .lock()
            .await
            .iter()
            .map(|(runtime, _)| runtime.clone())
            .collect()
    }

    async fn pending_tool_ids(
        &self,
    ) -> std::collections::HashSet<crate::domain::models::RequestId> {
        let mut ids = std::collections::HashSet::new();
        for runtime in self.runtimes().await {
            ids.extend(
                runtime
                    .pending_tool_requests(
                        crate::domain::services::approval_runtime::ReviewScope::AllTools,
                    )
                    .await
                    .into_iter()
                    .map(|request| request.id),
            );
        }
        ids
    }
}

/// Subscribe and publish the approval gate as one ordered operation.
///
/// Install the receiver and owned handle under one per-runtime registration
/// lock. Concurrent first callers cannot miss a request or spawn duplicate
/// subscribers; shutdown closes this same registration boundary.
async fn ensure_approval_gate_once(
    approval: Arc<ApprovalRuntime>,
    registry: Arc<Mutex<ConnRegistry>>,
    blocked: Arc<AtomicUsize>,
    conversation: Arc<Mutex<Conversation>>,
    storage: Arc<dyn crate::domain::ports::StoragePort>,
    domain_tx: mpsc::UnboundedSender<AppEvent>,
    pending_consent_cards: Arc<
        Mutex<std::collections::HashMap<crate::domain::models::RequestId, DaemonFrame>>,
    >,
    gate: Arc<ApprovalGate>,
) {
    let mut tasks = gate.tasks.lock().await;
    if gate.shutdown.is_cancelled() {
        // Shutdown owns registered runtimes only. A late caller must close its
        // runtime itself, outside the task-registry lock.
        drop(tasks);
        approval
            .cancel_all(crate::domain::services::approval_runtime::CancelReason::RuntimeShutdown)
            .await;
        return;
    }
    if tasks
        .iter()
        .any(|(runtime, _)| Arc::ptr_eq(runtime, &approval))
    {
        return;
    }
    let events = approval.subscribe();
    let replay = gate.replay.subscribe();
    let task = tokio::spawn(run_approval_gate(
        events,
        approval.clone(),
        registry,
        blocked,
        conversation,
        storage,
        domain_tx,
        pending_consent_cards,
        gate.clone(),
        replay,
    ));
    tasks.push((approval, task));
}

/// The daemon's single `AgentMessageBus` slot: one atomically swappable pointer
/// shared by every route that delivers into this daemon's `NodeTree`.
///
/// Story 18.3 (AC3). Structurally identical to `AgentCore.agent_message_bus`
/// (`Arc<ArcSwap<Arc<dyn AgentMessageBus>>>`) so a composition root can hand the
/// same shape to either consumer, and so installing a `DeliveryPolicy` is one
/// store into one slot rather than a per-adapter mint.
pub type PeerBusSlot = Arc<arc_swap::ArcSwap<Arc<dyn crate::domain::ports::AgentMessageBus>>>;

/// Build the default peer bus slot: a `LocalMessageBus` over `node_tree` with
/// the stock `RelationshipDeliveryPolicy`.
///
/// This is the ONE `LocalMessageBus` construction site outside a composition
/// root. It exists for the convenience constructors that non-daemon callers and
/// tests use; the daemon composition root builds and owns its own slot so the
/// policy it installs is the policy the peer path honours.
pub fn default_peer_bus_slot(node_tree: &crate::infrastructure::subagent::NodeTree) -> PeerBusSlot {
    peer_bus_slot_with_policy(
        node_tree,
        Arc::new(crate::domain::ports::RelationshipDeliveryPolicy),
    )
}

pub fn peer_bus_slot_with_policy(
    node_tree: &crate::infrastructure::subagent::NodeTree,
    policy: Arc<dyn crate::domain::ports::DeliveryPolicy>,
) -> PeerBusSlot {
    let bus = Arc::new(
        crate::infrastructure::agent_message_bus::LocalMessageBus::new(node_tree.clone(), policy),
    ) as Arc<dyn crate::domain::ports::AgentMessageBus>;
    Arc::new(arc_swap::ArcSwap::from_pointee(bus))
}

fn inbound_peer_refuse_reason(
    disposition: crate::domain::models::DeliveryDisposition,
    terminal: bool,
) -> crate::domain::models::RefuseReason {
    if terminal {
        crate::domain::models::RefuseReason::TerminalState
    } else if crate::domain::models::may_consent_refuse(disposition) {
        crate::domain::models::RefuseReason::Policy
    } else {
        crate::domain::models::RefuseReason::Unavailable
    }
}

impl AttachServer {
    pub fn new(
        core: Arc<DaemonCore>,
        conversation: Arc<Mutex<Conversation>>,
        domain_tx: mpsc::UnboundedSender<AppEvent>,
    ) -> Arc<Self> {
        Self::with_clock(
            core,
            conversation,
            domain_tx,
            Arc::new(SystemClock::default()),
        )
    }

    /// Like [`AttachServer::new`] but with an injectable wall-clock source. Used
    /// by tests to drive peer-envelope TTL past expiry deterministically; the
    /// replay window is always fresh and server-shared regardless of clock.
    pub fn with_clock(
        core: Arc<DaemonCore>,
        conversation: Arc<Mutex<Conversation>>,
        domain_tx: mpsc::UnboundedSender<AppEvent>,
        clock: Arc<dyn Clock>,
    ) -> Arc<Self> {
        let node_tree = crate::infrastructure::subagent::NodeTree::with_event_tx(
            domain_tx.clone(),
            Arc::new(|| chrono::Utc::now().timestamp_millis()),
        );
        let delivery_policy: Arc<dyn crate::domain::ports::DeliveryPolicy> =
            Arc::new(crate::domain::ports::RelationshipDeliveryPolicy);
        let peer_bus = peer_bus_slot_with_policy(&node_tree, delivery_policy.clone());
        Self::with_clock_and_node_tree(
            core,
            conversation,
            domain_tx,
            clock,
            node_tree,
            peer_bus,
            delivery_policy,
            None,
            None,
            None,
            None,
            Arc::new(super::team_proposal::TeamProposalRegistry::new()),
        )
    }

    pub fn new_with_node_tree(
        core: Arc<DaemonCore>,
        conversation: Arc<Mutex<Conversation>>,
        domain_tx: mpsc::UnboundedSender<AppEvent>,
        node_tree: crate::infrastructure::subagent::NodeTree,
    ) -> Arc<Self> {
        let delivery_policy: Arc<dyn crate::domain::ports::DeliveryPolicy> =
            Arc::new(crate::domain::ports::RelationshipDeliveryPolicy);
        let peer_bus = peer_bus_slot_with_policy(&node_tree, delivery_policy.clone());
        Self::with_clock_and_node_tree(
            core,
            conversation,
            domain_tx,
            Arc::new(SystemClock::default()),
            node_tree,
            peer_bus,
            delivery_policy,
            None,
            None,
            None,
            None,
            Arc::new(super::team_proposal::TeamProposalRegistry::new()),
        )
    }

    /// The production entry point (Story 18.3, AC3): the composition root owns
    /// the bus slot and hands it in, so the `DeliveryPolicy` it installed is the
    /// one the RAP peer path actually honours.
    pub fn new_with_node_tree_and_bus(
        core: Arc<DaemonCore>,
        conversation: Arc<Mutex<Conversation>>,
        domain_tx: mpsc::UnboundedSender<AppEvent>,
        node_tree: crate::infrastructure::subagent::NodeTree,
        peer_bus: PeerBusSlot,
    ) -> Arc<Self> {
        Self::new_with_node_tree_bus_and_policy(
            core,
            conversation,
            domain_tx,
            node_tree,
            peer_bus,
            Arc::new(crate::domain::ports::RelationshipDeliveryPolicy),
        )
    }

    pub fn new_with_node_tree_bus_and_policy(
        core: Arc<DaemonCore>,
        conversation: Arc<Mutex<Conversation>>,
        domain_tx: mpsc::UnboundedSender<AppEvent>,
        node_tree: crate::infrastructure::subagent::NodeTree,
        peer_bus: PeerBusSlot,
        delivery_policy: Arc<dyn crate::domain::ports::DeliveryPolicy>,
    ) -> Arc<Self> {
        Self::with_clock_and_node_tree(
            core,
            conversation,
            domain_tx,
            Arc::new(SystemClock::default()),
            node_tree,
            peer_bus,
            delivery_policy,
            None,
            None,
            None,
            None,
            Arc::new(super::team_proposal::TeamProposalRegistry::new()),
        )
    }

    pub fn new_with_node_tree_bus_policy_and_journal(
        core: Arc<DaemonCore>,
        conversation: Arc<Mutex<Conversation>>,
        domain_tx: mpsc::UnboundedSender<AppEvent>,
        node_tree: crate::infrastructure::subagent::NodeTree,
        peer_bus: PeerBusSlot,
        delivery_policy: Arc<dyn crate::domain::ports::DeliveryPolicy>,
        room_journal: Arc<dyn crate::domain::ports::RoomJournal>,
        room_journal_reader: Arc<dyn crate::domain::ports::RoomJournalReader>,
        consent_projection: Option<Arc<crate::adapters::policy::JournalConsentProjection>>,
    ) -> Arc<Self> {
        Self::new_with_node_tree_bus_policy_journal_and_urgency(
            core,
            conversation,
            domain_tx,
            node_tree,
            peer_bus,
            delivery_policy,
            room_journal,
            room_journal_reader,
            consent_projection,
            None,
        )
    }

    pub(crate) fn new_with_node_tree_bus_policy_journal_and_urgency(
        core: Arc<DaemonCore>,
        conversation: Arc<Mutex<Conversation>>,
        domain_tx: mpsc::UnboundedSender<AppEvent>,
        node_tree: crate::infrastructure::subagent::NodeTree,
        peer_bus: PeerBusSlot,
        delivery_policy: Arc<dyn crate::domain::ports::DeliveryPolicy>,
        room_journal: Arc<dyn crate::domain::ports::RoomJournal>,
        room_journal_reader: Arc<dyn crate::domain::ports::RoomJournalReader>,
        consent_projection: Option<Arc<crate::adapters::policy::JournalConsentProjection>>,
        urgency_router: Option<Arc<super::urgency::UrgencyRouter>>,
    ) -> Arc<Self> {
        Self::with_clock_and_node_tree(
            core,
            conversation,
            domain_tx,
            Arc::new(SystemClock::default()),
            node_tree,
            peer_bus,
            delivery_policy,
            Some(room_journal),
            Some(room_journal_reader),
            consent_projection,
            urgency_router,
            Arc::new(super::team_proposal::TeamProposalRegistry::new()),
        )
    }
    /// Journaled construction path for attached team commands. The runtime and
    /// roster are installed before the server is returned to any caller.
    pub(crate) fn new_with_node_tree_bus_policy_journal_urgency_and_team(
        core: Arc<DaemonCore>,
        conversation: Arc<Mutex<Conversation>>,
        domain_tx: mpsc::UnboundedSender<AppEvent>,
        node_tree: crate::infrastructure::subagent::NodeTree,
        peer_bus: PeerBusSlot,
        delivery_policy: Arc<dyn crate::domain::ports::DeliveryPolicy>,
        room_journal: Arc<dyn crate::domain::ports::RoomJournal>,
        room_journal_reader: Arc<dyn crate::domain::ports::RoomJournalReader>,
        consent_projection: Option<Arc<crate::adapters::policy::JournalConsentProjection>>,
        urgency_router: Option<Arc<super::urgency::UrgencyRouter>>,
        #[cfg(feature = "a2a")] runtime: Option<Arc<A2aDelegationRuntime>>,
        peers: Vec<crate::domain::models::A2aPeerSpec>,
        team_proposals: Arc<super::team_proposal::TeamProposalRegistry>,
        #[cfg(any(test, feature = "test-instrumentation"))] test_hooks: Option<TeamSendTestHooks>,
    ) -> Arc<Self> {
        let mut server = Self::with_clock_and_node_tree(
            core,
            conversation,
            domain_tx,
            Arc::new(SystemClock::default()),
            node_tree,
            peer_bus,
            delivery_policy,
            Some(room_journal),
            Some(room_journal_reader),
            consent_projection,
            urgency_router,
            team_proposals,
        );
        let server_mut = Arc::get_mut(&mut server).expect("new AttachServer has a unique Arc");
        #[cfg(feature = "a2a")]
        {
            server_mut.a2a_runtime = runtime;
        }
        server_mut.team_peers = peers;
        #[cfg(any(test, feature = "test-instrumentation"))]
        {
            server_mut.team_send_test_hooks = test_hooks;
        }
        server
    }
    /// Public journaled constructor for external conformance harnesses that
    /// provide the daemon's already-composed A2A runtime and peer roster.
    pub fn new_with_node_tree_bus_policy_journal_and_team(
        core: Arc<DaemonCore>,
        conversation: Arc<Mutex<Conversation>>,
        domain_tx: mpsc::UnboundedSender<AppEvent>,
        node_tree: crate::infrastructure::subagent::NodeTree,
        peer_bus: PeerBusSlot,
        delivery_policy: Arc<dyn crate::domain::ports::DeliveryPolicy>,
        room_journal: Arc<dyn crate::domain::ports::RoomJournal>,
        room_journal_reader: Arc<dyn crate::domain::ports::RoomJournalReader>,
        consent_projection: Option<Arc<crate::adapters::policy::JournalConsentProjection>>,
        #[cfg(feature = "a2a")] runtime: Option<Arc<A2aDelegationRuntime>>,
        peers: Vec<crate::domain::models::A2aPeerSpec>,
        team_proposals: Arc<super::team_proposal::TeamProposalRegistry>,
        #[cfg(any(test, feature = "test-instrumentation"))] test_hooks: Option<TeamSendTestHooks>,
    ) -> Arc<Self> {
        Self::new_with_node_tree_bus_policy_journal_urgency_and_team(
            core,
            conversation,
            domain_tx,
            node_tree,
            peer_bus,
            delivery_policy,
            room_journal,
            room_journal_reader,
            consent_projection,
            None,
            #[cfg(feature = "a2a")]
            runtime,
            peers,
            team_proposals,
            #[cfg(any(test, feature = "test-instrumentation"))]
            test_hooks,
        )
    }

    fn with_clock_and_node_tree(
        core: Arc<DaemonCore>,
        conversation: Arc<Mutex<Conversation>>,
        domain_tx: mpsc::UnboundedSender<AppEvent>,
        clock: Arc<dyn Clock>,
        node_tree: crate::infrastructure::subagent::NodeTree,
        peer_bus: PeerBusSlot,
        delivery_policy: Arc<dyn crate::domain::ports::DeliveryPolicy>,
        room_journal: Option<Arc<dyn crate::domain::ports::RoomJournal>>,
        room_journal_reader: Option<Arc<dyn crate::domain::ports::RoomJournalReader>>,
        consent_projection: Option<Arc<crate::adapters::policy::JournalConsentProjection>>,
        urgency_router: Option<Arc<super::urgency::UrgencyRouter>>,
        team_proposals: Arc<super::team_proposal::TeamProposalRegistry>,
    ) -> Arc<Self> {
        let pending_consent =
            room_journal
                .as_ref()
                .zip(consent_projection)
                .map(|(journal, projection)| {
                    Arc::new(super::consent::PendingConsentManager::new(
                        projection,
                        Arc::clone(journal),
                        Arc::clone(&clock),
                        room_journal_reader.clone(),
                    ))
                });
        Arc::new_cyclic(|_weak| Self {
            core,
            conversation,
            registry: Arc::new(Mutex::new(ConnRegistry::default())),
            domain_tx,
            node_tree,
            blocked_waiting: Arc::new(AtomicUsize::new(0)),
            next_conn_id: AtomicU64::new(1),
            last_writer_conn_id: AtomicU64::new(0),
            approval_gate: Arc::new(ApprovalGate::default()),
            turn_serial: Arc::new(Mutex::new(())),
            turn_complete: Arc::new(Notify::new()),
            peer_turn_closed: Arc::new(Notify::new()),
            retained_consolidations: Arc::new(Mutex::new(std::collections::HashMap::new())),
            active_channel_origin: Arc::new(Mutex::new(ChannelKind::Terminal)),
            peer_assistant_buffers: Mutex::new(std::collections::HashMap::new()),
            pending_channel_response_tx: Arc::new(Mutex::new(None)),
            next_proposal_token: Arc::new(AtomicU64::new(1)),
            generating_consolidations: Arc::new(Mutex::new(std::collections::HashSet::new())),
            replay: Arc::new(Mutex::new(ReplayWindow::default())),
            clock,
            peer_delivery: Arc::new(tokio::sync::RwLock::new(None)),
            peer_bus,
            delivery_policy,
            pending_drafts: Arc::new(super::response_modes::PendingDraftController::default()),
            peer_row_serial: Arc::new(Mutex::new(())),
            peer_reply_serial: Arc::new(Mutex::new(())),
            pending_consent,
            pending_consent_cards: Arc::new(Mutex::new(std::collections::HashMap::new())),
            pending_peer_nodes: Arc::new(Mutex::new(std::collections::HashMap::new())),
            urgency_router,
            operator_turn_active: std::sync::atomic::AtomicBool::new(false),
            room_journal,
            room_journal_reader,
            #[cfg(feature = "a2a")]
            a2a_runtime: None,
            team_peers: Vec::new(),
            team_proposals,
            team_send_slots: Arc::new(Semaphore::new(MAX_IN_FLIGHT_TEAM_SENDS)),
            team_send_shutdown: CancellationToken::new(),
            team_send_tasks: Mutex::new(Vec::new()),
            #[cfg(feature = "a2a")]
            team_retracts: Arc::new(Mutex::new(std::collections::HashMap::new())),
            team_send_accepting: AtomicBool::new(true),
            team_send_started_count: Arc::new(AtomicUsize::new(0)),
            team_send_settled_count: Arc::new(AtomicUsize::new(0)),
            team_send_saturation_refusals: Arc::new(AtomicUsize::new(0)),
            #[cfg(any(test, feature = "test-instrumentation"))]
            team_send_test_hooks: None,
            inbound_results: Arc::new(Mutex::new(std::collections::HashMap::new())),
        })
    }

    /// Install the resolved effective policy so the consent gate honours TOML
    /// overrides as implied consent (F2).
    pub async fn configure_consent_policy(
        &self,
        policy: std::sync::Arc<crate::domain::models::EffectivePolicy>,
    ) {
        if let Some(manager) = &self.pending_consent {
            manager.set_effective_policy(policy).await;
        }
    }
    /// Deterministic seam for attached-send lifecycle conformance.
    pub fn team_send_observations(&self) -> (usize, usize, usize, usize) {
        (
            MAX_IN_FLIGHT_TEAM_SENDS - self.team_send_slots.available_permits(),
            self.team_send_started_count.load(Ordering::Acquire),
            self.team_send_settled_count.load(Ordering::Acquire),
            self.team_send_saturation_refusals.load(Ordering::Acquire),
        )
    }

    pub fn node_tree(&self) -> crate::infrastructure::subagent::NodeTree {
        self.node_tree.clone()
    }

    /// The authoritative bus slot this server delivers verified peer frames
    /// through — the same object the composition root installed (AC3).
    pub fn peer_bus(&self) -> PeerBusSlot {
        self.peer_bus.clone()
    }

    /// Install the mandatory transparency recorder and enable peer delivery.
    ///
    /// Before this call verified frames are rejected at the daemon boundary;
    /// there is no recorder-free live path. The rebuilt handler receives the
    /// same authoritative bus slot owned by this server.
    pub async fn configure_peer_recorder(
        self: &Arc<Self>,
        recorder: Arc<dyn crate::domain::ports::PeerInteractionRecorder>,
        topics: Arc<crate::adapters::rap::PeerTopicStore>,
    ) {
        let handler = Arc::new(
            crate::adapters::rap::VerifiedPeerFrameHandler::new(
                self.node_tree.clone(),
                self.peer_bus.clone(),
                self.domain_tx.clone(),
                Arc::new(DaemonPeerConsumer::new(Arc::downgrade(self))),
                recorder,
            )
            // Story 18.4a — the **same** `Arc` the `"composite"` context adapter
            // reads. ⛔ Constructing a second store here would give this host an
            // agent that reads a different log than the one its transport
            // writes, which is the failure this argument exists to prevent.
            .with_topics(topics),
        );
        *self.peer_delivery.write().await = Some(handler);
    }

    /// Clone the configured peer front door for sibling network listeners.
    pub async fn verified_peer_handler(
        &self,
    ) -> Option<Arc<crate::adapters::rap::VerifiedPeerFrameHandler>> {
        self.peer_delivery.read().await.clone()
    }

    /// Current count of connected channels for honest `status` reporting (AC4).
    pub fn channel_count(&self) -> usize {
        // best-effort, non-blocking read
        self.registry.try_lock().map(|r| r.conns.len()).unwrap_or(0)
    }

    async fn item_context_for_node(
        &self,
        node_id: &AgentId,
    ) -> Option<(ItemAddress, Option<String>)> {
        #[cfg(feature = "a2a")]
        {
            let reader = self.room_journal_reader.as_ref()?;
            let entries = match reader.load_entries().await {
                Ok(entries) => entries,
                Err(error) => {
                    tracing::warn!(%error, node = %node_id, "loading journal for peer-row target failed");
                    return None;
                }
            };
            let projection =
                crate::adapters::policy::JournalRecipientItemProjection::from_entries(&entries);
            crate::adapters::a2a::exec::recipient_item_for_node(&projection, node_id)
                .map(|item| (item.address, item.alias))
        }
        #[cfg(not(feature = "a2a"))]
        {
            let _ = node_id;
            None
        }
    }
    pub async fn run(
        self: Arc<Self>,
        listener: UnixListener,
        mut domain_rx: mpsc::UnboundedReceiver<AppEvent>,
        channel_rx: Option<mpsc::UnboundedReceiver<ChannelTurnRequest>>,
        cron_completion_rx: Option<mpsc::UnboundedReceiver<CronCompletion>>,
        shutdown: CancellationToken,
    ) {
        // Story 19.20 review: settlement is best-effort. A failed commit rolls
        // the rows back (still unresolvable, as before 19.20) and the next
        // start retries; it must never leave the daemon without its socket.
        if let Err(error) = self.settle_interrupted_drafts().await {
            tracing::warn!(
                %error,
                "failed to settle interrupted peer drafts; serving them unsettled"
            );
        }
        // The single forwarder task: fold → fan out (drains the one mpsc
        // `domain_rx`; connections do NOT each subscribe). Channel turns enter
        // this same forwarder so their response callback is resolved by the same
        // assistant commit path as socket turns.
        let fwd = self.clone();
        let fwd_shutdown = shutdown.clone();
        let forwarder = tokio::spawn(async move {
            let mut assistant_buf = String::new();
            let mut channel_rx = channel_rx;
            let mut cron_completion_rx = cron_completion_rx;
            loop {
                tokio::select! {
                    _ = fwd_shutdown.cancelled() => break,
                    maybe = domain_rx.recv() => {
                        let Some(event) = maybe else { break };
                        fwd.handle_bus_event(&event, &mut assistant_buf).await;
                    }
                    maybe = async {
                        match &mut channel_rx {
                            Some(rx) => rx.recv().await,
                            None => std::future::pending::<Option<ChannelTurnRequest>>().await,
                        }
                    } => {
                        let Some(req) = maybe else {
                            channel_rx = None;
                            continue;
                        };
                        let srv = fwd.clone();
                        tokio::spawn(async move { srv.drive_channel_turn(req).await; });
                    }
                    maybe = async {
                        match &mut cron_completion_rx {
                            Some(rx) => rx.recv().await,
                            None => std::future::pending::<Option<CronCompletion>>().await,
                        }
                    } => {
                        let Some(completion) = maybe else {
                            cron_completion_rx = None;
                            continue;
                        };
                        fwd.commit_cron_completion(completion).await;
                    }
                }
            }
        });

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                accepted = listener.accept() => match accepted {
                    Ok((stream, _addr)) => {
                        let srv = self.clone();
                        tokio::spawn(async move { srv.handle_connection(stream).await; });
                    }
                    Err(e) => tracing::warn!(error = %e, "daemon attach accept failed"),
                },
            }
        }

        let tasks = {
            let mut tasks = self.team_send_tasks.lock().await;
            self.team_send_accepting.store(false, Ordering::Release);
            self.team_send_shutdown.cancel();
            std::mem::take(&mut *tasks)
        };
        for task in tasks {
            let _ = task.await;
        }
        self.team_proposals.shutdown().await;
        self.approval_gate.shutdown(&self.registry).await;
        forwarder.abort();
    }
    async fn surface_queued_interactions(
        &self,
        queued: Vec<super::urgency::SurfaceInteraction>,
    ) -> anyhow::Result<()> {
        if queued.is_empty() {
            return Ok(());
        }
        let mutations = queued
            .into_iter()
            .map(|interaction| PeerRowMutation::Upsert {
                message: peer_transcript_message(
                    &interaction.text,
                    &interaction.peer,
                    &interaction.provenance,
                ),
                control: None,
            })
            .collect();
        commit_peer_rows(
            &self.peer_row_serial,
            &self.conversation,
            &self.registry,
            self.core.storage.as_ref(),
            mutations,
        )
        .await
        .map_err(anyhow::Error::msg)?;
        Ok(())
    }

    pub(crate) async fn surface_digest_batch(
        &self,
        batch: super::urgency::DigestBatch,
    ) -> anyhow::Result<()> {
        let mut content = batch.content();
        for item in &batch.items {
            content.push_str(&format!(
                "\n  {}\n  {}",
                item.provenance.response_clause(),
                item.provenance.notification_clause()
            ));
        }
        let message = ChatMessage {
            id: generate_message_id(),
            role: MessageRole::System,
            content,
            created_at: crate::domain::models::session_meta::now_unix(),
            synthetic: true,
            ..Default::default()
        };
        commit_peer_rows(
            &self.peer_row_serial,
            &self.conversation,
            &self.registry,
            self.core.storage.as_ref(),
            vec![PeerRowMutation::Upsert {
                message,
                control: None,
            }],
        )
        .await
        .map_err(anyhow::Error::msg)?;
        Ok(())
    }

    async fn collect_peer_chunk(&self, node: &Arc<AgentId>, chunk: &StreamChunk) {
        use crate::domain::models::{ToolCallInfo, ToolResultInfo};
        match chunk {
            StreamChunk::TurnComplete { stop_reason } if *stop_reason != StopReason::ToolUse => {
                self.finish_peer_buffer(node, stop_reason).await;
                return;
            }
            StreamChunk::Error { content } => {
                self.finish_peer_buffer(node, &StopReason::Cancelled).await;
                if !content.is_empty() {
                    let notice = ChatMessage {
                        id: generate_message_id(),
                        role: MessageRole::System,
                        content: format!("Peer turn failed: {content}"),
                        created_at: crate::domain::models::session_meta::now_unix(),
                        synthetic: true,
                        ..Default::default()
                    };
                    if let Err(error) = commit_peer_rows(
                        &self.peer_row_serial,
                        &self.conversation,
                        &self.registry,
                        self.core.storage.as_ref(),
                        vec![PeerRowMutation::Upsert {
                            message: notice,
                            control: None,
                        }],
                    )
                    .await
                    {
                        tracing::warn!(%error, "persisting peer stream error notice failed");
                    }
                }
                return;
            }
            _ => {}
        }
        let mut buffers = self.peer_assistant_buffers.lock().await;
        match chunk {
            StreamChunk::Text { content, .. } => {
                buffers
                    .entry(node.clone())
                    .or_default()
                    .text
                    .push_str(content);
            }
            StreamChunk::ToolUse { id, name, input } => {
                buffers
                    .entry(node.clone())
                    .or_default()
                    .tools
                    .push(ToolCallInfo {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                        result: None,
                        started_at_ms: None,
                        completed_at_ms: None,
                        status: None,
                    });
            }
            StreamChunk::ToolResult {
                id,
                content,
                is_error,
                diff,
                operator_private,
            } => {
                let buffer = buffers.entry(node.clone()).or_default();
                if let Some(tool) = buffer.tools.iter_mut().find(|tool| tool.id == *id) {
                    tool.result = Some(ToolResultInfo {
                        content: content.clone(),
                        is_error: *is_error,
                        diff: diff.clone(),
                    });
                } else if !*operator_private {
                    // Retain unexpected result evidence without inventing a call.
                    buffer.text.push_str(content);
                }
            }
            _ => {}
        }
    }

    async fn finish_peer_buffer(&self, node: &Arc<AgentId>, stop_reason: &StopReason) {
        let Some(mut buffer) = self.peer_assistant_buffers.lock().await.remove(node) else {
            return;
        };
        if *stop_reason == StopReason::Cancelled {
            // A cancelled peer turn is not an answer; tool evidence remains visible.
            buffer.text.clear();
        }
        if buffer.text.is_empty() && buffer.tools.is_empty() {
            return;
        }
        let message = ChatMessage {
            id: generate_message_id(),
            role: if *stop_reason == StopReason::Cancelled {
                MessageRole::System
            } else {
                MessageRole::Assistant
            },
            content: buffer.text,
            tool_calls: buffer.tools,
            stop_reason: Some(stop_reason.clone()),
            authorship: crate::domain::models::Authorship::AgentComposed,
            send_authorization: crate::domain::models::SendAuthorization::Unknown,
            history_source: crate::domain::models::HistorySource::PeerDisplay,
            created_at: crate::domain::models::session_meta::now_unix(),
            ..Default::default()
        };
        if let Err(error) = commit_peer_rows(
            &self.peer_row_serial,
            &self.conversation,
            &self.registry,
            self.core.storage.as_ref(),
            vec![PeerRowMutation::Upsert {
                message,
                control: None,
            }],
        )
        .await
        {
            tracing::warn!(%error, "persisting peer turn display failed");
        }
    }

    /// Retain exact proposal evidence without folding its private arguments or
    /// result into the public prose transcript. Snapshots project these tools
    /// out; the stored conversation and model continuation remain complete.
    async fn retain_operator_private_chunk(&self, chunk: &StreamChunk) {
        use crate::domain::models::{ToolCallInfo, ToolResultInfo};
        let mut conversation = self.conversation.lock().await;
        match chunk {
            StreamChunk::ToolUse { id, name, input } => {
                conversation.messages.push(ChatMessage {
                    id: generate_message_id(),
                    role: MessageRole::Assistant,
                    tool_calls: vec![ToolCallInfo {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                        result: None,
                        started_at_ms: None,
                        completed_at_ms: None,
                        status: None,
                    }],
                    created_at: crate::domain::models::session_meta::now_unix(),
                    ..Default::default()
                });
            }
            StreamChunk::ToolResult {
                id,
                content,
                is_error,
                diff,
                ..
            } => {
                if let Some(tool) = conversation
                    .messages
                    .iter_mut()
                    .rev()
                    .flat_map(|message| message.tool_calls.iter_mut())
                    .find(|tool| {
                        tool.id == *id
                            && tool.name
                                == crate::domain::models::team_proposal::TEAM_PROPOSAL_TOOL_NAME
                    })
                {
                    tool.result = Some(ToolResultInfo {
                        content: content.clone(),
                        is_error: *is_error,
                        diff: diff.clone(),
                    });
                }
            }
            _ => return,
        }
        conversation.updated_at = crate::domain::models::session_meta::now_unix();
        if let Err(error) = self.core.storage.save_conversation(&conversation).await {
            tracing::warn!(%error, "persisting operator-private tool evidence failed");
        }
    }

    /// Fold an assistant turn into the conversation (persist on completion) and
    /// fan the projected event out to all attachments.
    async fn handle_bus_event(&self, event: &AppEvent, assistant_buf: &mut String) {
        match event {
            AppEvent::PeerProviderChunk { node, chunk, .. } => {
                self.collect_peer_chunk(node, chunk).await;
            }
            AppEvent::PeerTurnClosed { node, .. } => {
                self.finish_peer_buffer(node, &StopReason::Cancelled).await;
                self.peer_turn_closed.notify_waiters();
            }
            AppEvent::PeerSystemNotice { node, level, .. }
                if *level == crate::domain::models::NoticeLevel::Error =>
            {
                self.finish_peer_buffer(node, &StopReason::Cancelled).await;
            }
            _ => {}
        }
        if let AppEvent::ProviderChunk { chunk, .. } = event {
            if chunk.is_operator_private() {
                self.retain_operator_private_chunk(chunk).await;
                self.operator_turn_active.store(true, Ordering::Release);
                return;
            }
            match chunk {
                StreamChunk::Text { content, .. } => {
                    self.operator_turn_active.store(true, Ordering::Release);
                    assistant_buf.push_str(content);
                }
                StreamChunk::TurnComplete { stop_reason } => {
                    self.operator_turn_active.store(false, Ordering::Release);
                    if *stop_reason == StopReason::Cancelled {
                        // A cancelled turn has no committed assistant answer. In
                        // particular, an aborted inbound turn can have already
                        // streamed text, which must not become the prefix of the
                        // next turn's answer.
                        assistant_buf.clear();
                    } else {
                        self.commit_assistant_turn(assistant_buf, stop_reason).await;
                        assistant_buf.clear();
                        self.turn_complete.notify_waiters();
                    }
                    if let Some(router) = &self.urgency_router {
                        let queued = router.idle_queue_snapshot().await;
                        let count = queued.len();
                        match self.surface_queued_interactions(queued).await {
                            Ok(()) => router.drain_idle_queue(count).await,
                            Err(error) => tracing::error!(
                                %error,
                                "failed to surface queued peer interactions; will retry at next idle"
                            ),
                        }
                    }
                }
                StreamChunk::ToolUse { id, name, .. } => {
                    assistant_buf.push_str(&format!("\n[tool use: {name} (id: {id})]\n"));
                    self.operator_turn_active.store(true, Ordering::Release);
                }
                StreamChunk::ToolResult { content, .. } => {
                    assistant_buf.push_str(&format!("[tool result: {content}]\n"));
                    self.operator_turn_active.store(true, Ordering::Release);
                }
                _ => {}
            }
        }
        // 18.9b-a review patch (owner ruling 2026-09-06) — daemon-side
        // self-heal mirroring the TUI handler (tui/handlers/a2a_catalog.rs):
        // a peer whose AgentCard lands AFTER first activity would otherwise
        // stay invisible for the process lifetime, because the one-shot eager
        // registration in `set_a2a_provider` ran before the card was cached
        // and nothing daemon-side repopulated on the catalog signal.
        #[cfg(feature = "a2a")]
        if let AppEvent::A2aCatalogChanged {
            peer_id,
            skill_count,
        } = event
        {
            if let Some(runtime) = self.core.built_runtime() {
                if let Some(composite) = runtime.tools.as_any().downcast_ref::<
                    crate::adapters::composite_toolset_adapter::CompositeToolsetAdapter,
                >() {
                    if let Err(error) = composite.populate_registry().await {
                        tracing::debug!(
                            %error,
                            "populate_registry failed on A2aCatalogChanged (daemon)"
                        );
                    }
                } else {
                    tracing::debug!(
                        peer_id = %peer_id,
                        skill_count,
                        "A2aCatalogChanged: daemon toolset is not the composite; a2a catalogue unchanged"
                    );
                }
            } else {
                tracing::debug!(
                    peer_id = %peer_id,
                    skill_count,
                    "A2aCatalogChanged before the daemon runtime is built; catalogue unchanged"
                );
            }
        }
        // Project → fan out (reusing the single `from_app_event` mapping).
        if let Some(raw) = RawEvent::from_app_event(event) {
            self.fanout(DaemonFrame::Event(raw)).await;
        }
    }

    /// Append the accumulated assistant message (origin-tagged) and persist (AC4).
    async fn commit_assistant_turn(&self, text: &str, stop_reason: &StopReason) {
        if text.is_empty() {
            return;
        }
        let origin = *self.active_channel_origin.lock().await;
        let mut conv = self.conversation.lock().await;
        conv.messages.push(ChatMessage {
            id: generate_message_id(),
            role: MessageRole::Assistant,
            content: text.to_string(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: crate::domain::models::session_meta::now_unix(),
            token_count: None,
            stop_reason: Some(stop_reason.clone()),
            synthetic: false,
            images: vec![],
            origin,
            authorship: crate::domain::models::Authorship::HumanWritten,
            send_authorization: crate::domain::models::SendAuthorization::Unknown,
            retracted_at_ms: None,
            history_source: Default::default(),
        });
        let now = crate::domain::models::session_meta::now_unix();
        conv.updated_at = now;
        conv.last_response_at = Some(now);
        if let Err(e) = self.core.storage.save_conversation(&conv).await {
            tracing::warn!(error = %e, "daemon: persisting conversation after turn failed");
        }
        drop(conv);
        if let Some(tx) = self.pending_channel_response_tx.lock().await.take() {
            let _ = tx.send(text.to_string());
        }
    }

    #[cfg(feature = "cron")]
    async fn commit_cron_completion(&self, completion: CronCompletion) {
        let text = format!("[cron: {}] {}", completion.job_name, completion.result_text);
        let mut conv = self.conversation.lock().await;
        conv.messages.push(ChatMessage {
            id: generate_message_id(),
            role: MessageRole::Assistant,
            content: text,
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: crate::domain::models::session_meta::now_unix(),
            token_count: None,
            stop_reason: Some(StopReason::EndTurn),
            synthetic: false,
            images: vec![],
            origin: ChannelKind::Cron,
            authorship: crate::domain::models::Authorship::HumanWritten,
            send_authorization: crate::domain::models::SendAuthorization::Unknown,
            retracted_at_ms: None,
            history_source: Default::default(),
        });
        let now = crate::domain::models::session_meta::now_unix();
        conv.updated_at = now;
        conv.last_response_at = Some(now);
        if let Err(e) = self.core.storage.save_conversation(&conv).await {
            tracing::warn!(error = %e, "daemon: persisting cron completion failed");
        }
    }

    #[cfg(not(feature = "cron"))]
    async fn commit_cron_completion(&self, _completion: CronCompletion) {}

    async fn fanout(&self, frame: DaemonFrame) {
        let dead = {
            let reg = self.registry.lock().await;
            reg.fanout(&frame)
        };
        if !dead.is_empty() {
            let mut reg = self.registry.lock().await;
            for id in dead {
                tracing::info!(conn = id, "daemon: dropping slow/closed attach connection");
                reg.remove(id);
                #[cfg(feature = "a2a")]
                self.team_retracts.lock().await.remove(&id);
            }
        }
    }

    /// Handle one connection: handshake → register → reader loop.
    async fn handle_connection(self: Arc<Self>, stream: UnixStream) {
        let (mut read_half, mut write_half) = stream.into_split();

        // ── Server-first challenge handshake (Story 17.1a) ──
        // Mint a fresh, one-use challenge nonce and send it. The client must
        // answer with an Ed25519 proof bound to this exact nonce before the
        // connection is trusted — for BOTH TrustedLocal and Peer tiers.
        let mut challenge = [0u8; 32];
        OsRng.fill_bytes(&mut challenge);
        let nonce = challenge.to_vec();
        if write_frame(
            &mut write_half,
            &DaemonFrame::AttachChallenge {
                nonce: nonce.clone(),
            },
        )
        .await
        .is_err()
        {
            return;
        }

        // First frame MUST be the proof-bearing Attach.
        let attach = match read_frame::<_, ClientFrame>(&mut read_half).await {
            Ok(Some(ClientFrame::Attach {
                protocol_version,
                read_only_ok,
                tier,
                challenge_nonce,
                identity,
                proof,
            })) => {
                // Version negotiation (AC2): reject a mismatch with a clear Error
                // BEFORE any proof work or side effect.
                if protocol_version != PROTOCOL_VERSION {
                    let _ = write_frame(
                        &mut write_half,
                        &DaemonFrame::Error(ProtocolError::VersionMismatch {
                            daemon: PROTOCOL_VERSION,
                            client: protocol_version,
                        }),
                    )
                    .await;
                    return;
                }
                // Bind the proof to the exact challenge we issued for THIS
                // connection. A proof captured elsewhere (a different nonce)
                // cannot satisfy it — the core challenge-response replay defense.
                if challenge_nonce != nonce {
                    let _ = write_frame(
                        &mut write_half,
                        &DaemonFrame::Error(ProtocolError::AttachProof(
                            "challenge nonce does not match the one issued".into(),
                        )),
                    )
                    .await;
                    return;
                }
                // Verify possession: identity↔key binding, tier tag, read-only
                // flag, and the signature over the domain-separated transcript.
                // The claimed tier's tag is used, so a proof minted for one tier
                // fails verification for another. All checks precede registry
                // mutation, the attach snapshot, the writer grant, app events,
                // and dispatch — zero side effect on failure.
                if let Err(e) = verify_attach_proof(
                    &identity,
                    &proof,
                    &nonce,
                    PROTOCOL_VERSION,
                    tier.proof_tag(),
                    read_only_ok,
                ) {
                    let _ = write_frame(
                        &mut write_half,
                        &DaemonFrame::Error(ProtocolError::AttachProof(e.to_string())),
                    )
                    .await;
                    return;
                }
                (read_only_ok, tier)
            }
            Ok(Some(_)) => {
                let _ = write_frame(
                    &mut write_half,
                    &DaemonFrame::Error(ProtocolError::Malformed(
                        "first frame must be Attach".into(),
                    )),
                )
                .await;
                return;
            }
            Ok(None) => return, // clean disconnect before handshake
            Err(e) => {
                tracing::warn!(error = %e, "daemon: attach handshake read failed");
                return;
            }
        };
        let (read_only_ok, tier) = attach;

        // The row serial lock covers grant, snapshot and registration. A peer-row
        // commit is therefore either in the snapshot or queued after it.
        let row_guard = self.peer_row_serial.lock().await;
        let conn_id = self.next_conn_id.fetch_add(1, Ordering::SeqCst);
        #[cfg(any(test, feature = "test-instrumentation"))]
        let outbound_queue_capacity = self
            .team_send_test_hooks
            .as_ref()
            .and_then(TeamSendTestHooks::queue_capacity)
            .unwrap_or(CONN_QUEUE_DEPTH);
        #[cfg(not(any(test, feature = "test-instrumentation")))]
        let outbound_queue_capacity = CONN_QUEUE_DEPTH;
        let (tx, mut rx) = mpsc::channel::<DaemonFrame>(outbound_queue_capacity);
        let (terminal_tx, mut terminal_rx) = watch::channel(None);
        let granted_mode = {
            let mut reg = self.registry.lock().await;
            let granted = if tier == ConnectionTier::Peer || read_only_ok || reg.has_writer() {
                AttachMode::ReadOnly
            } else {
                AttachMode::ReadWrite
            };
            if granted == AttachMode::ReadWrite {
                self.last_writer_conn_id.store(conn_id, Ordering::SeqCst);
                #[cfg(feature = "a2a")]
                self.team_retracts.lock().await.clear();
            }
            reg.conns.push(Conn {
                id: conn_id,
                tx: tx.clone(),
                terminal_tx: terminal_tx.clone(),
                mode: granted,
                tier,
                approval_ready: false,
            });
            granted
        };

        // Snapshot for immediate render (AC2).
        let pending_consent_count = self.pending_consent_cards.lock().await.len();
        let snapshot = {
            let conv = self.conversation.lock().await;
            let trusted_local = tier == ConnectionTier::TrustedLocal;
            AttachSnapshot {
                conversation_id: conv.id.clone(),
                transcript: conv
                    .messages
                    .iter()
                    .map(crate::infrastructure::runtime::event_bus::public_transcript_message)
                    .collect(),
                permission_mode: self.core.security.current_mode(),
                channels: vec![ChannelKind::Terminal],
                blocked_actions_waiting: self.blocked_waiting.load(Ordering::SeqCst),
                pending_consent_cards: pending_consent_count,
                retained_approvals: self.retained_approval_count().await,
                peer_row_controls: if trusted_local {
                    snapshot_peer_row_controls(&conv, &self.pending_drafts).await
                } else {
                    std::collections::BTreeMap::new()
                },
                peer_row_revision: if trusted_local {
                    conv.peer_row_revision
                } else {
                    0
                },
                recipient_context_ids: if trusted_local {
                    self.recipient_context_inventory().await.0
                } else {
                    Vec::new()
                },
                withheld_tool_inputs: Vec::new(),
            }
        };
        drop(row_guard);

        let mut acknowledgement = project_attach_frame(
            DaemonFrame::AttachAck {
                granted_mode,
                snapshot,
            },
            tier,
        );
        let omitted_bytes = bound_attach_snapshot(&mut acknowledgement);
        // Never substitute a notice for the mandatory first AttachAck. Metadata
        // that cannot fit is not safe to truncate (IDs and controls are exact).
        if serialized_bytes(&acknowledgement) > MAX_FRAME_BYTES as usize {
            let _ = write_frame(
                &mut write_half,
                &DaemonFrame::Error(ProtocolError::Internal(
                    "attach metadata exceeds the frame limit".to_owned(),
                )),
            )
            .await;
            self.registry.lock().await.remove(conn_id);
            return;
        }
        if write_frame(&mut write_half, &acknowledgement)
            .await
            .is_err()
        {
            self.registry.lock().await.remove(conn_id);
            return;
        }
        if let Some(bytes) = omitted_bytes {
            if write_frame(
                &mut write_half,
                &DaemonFrame::FrameTooLarge {
                    frame: "AttachAck".to_owned(),
                    bytes,
                },
            )
            .await
            .is_err()
            {
                self.registry.lock().await.remove(conn_id);
                return;
            }
        }

        // Reset blocked-waiting after the writer sees the attach snapshot count.
        if granted_mode == AttachMode::ReadWrite {
            let old = self.blocked_waiting.swap(0, Ordering::SeqCst);
            if old > 0 {
                tracing::info!(
                    count = old,
                    "daemon: cleared blocked-waiting counter on writer attach"
                );
            }
        }
        let mut writer_terminal_rx = terminal_rx.clone();
        #[cfg(any(test, feature = "test-instrumentation"))]
        let writer_hooks = self.team_send_test_hooks.clone();
        let writer = tokio::spawn(async move {
            #[cfg(any(test, feature = "test-instrumentation"))]
            if let Some(hooks) = writer_hooks.as_ref() {
                hooks.before_writer_drain().await;
            }
            let mut terminal_error_published = false;
            loop {
                tokio::select! {
                    biased;
                    changed = writer_terminal_rx.changed() => {
                        if changed.is_ok() {
                            let error = { writer_terminal_rx.borrow().clone() };
                            if let Some(error) = error {
                                if write_frame(&mut write_half, &DaemonFrame::Error(error.clone()))
                                    .await
                                    .is_ok()
                                {
                                    #[cfg(any(test, feature = "test-instrumentation"))]
                                    if let Some(hooks) = writer_hooks.as_ref() {
                                        hooks.record_terminal(
                                            conn_id,
                                            TeamSendTerminalEventKind::Error(error),
                                        );
                                    }
                                    terminal_error_published = true;
                                }
                            }
                        }
                        break;
                    }
                    frame = rx.recv() => {
                        let Some(frame) = frame else { break };
                        if write_attach_frame(&mut write_half, &frame).await.is_err() {
                            break;
                        }
                    }
                }
            }
            terminal_error_published
        });

        // Story 12.2c AC7 — drain the 12.1c durable boundary queues to the freshly
        // attached WRITER and emit them onto the normal event stream (no new frame).
        // The purge notice is daemon-owned state, so the DAEMON drains it (a
        // read-only client reading/clearing it would violate the ownership boundary).
        if granted_mode == AttachMode::ReadWrite {
            self.emit_session_queue_notices(conn_id).await;
            self.emit_pending_consent_cards(conn_id).await;
            self.registry
                .lock()
                .await
                .conns
                .iter_mut()
                .filter(|conn| conn.id == conn_id)
                .for_each(|conn| conn.approval_ready = true);
            self.approval_gate
                .replay
                .send_modify(|generation| *generation += 1);
        }

        // Turns, History fences and context commands share a bounded writer-side FIFO.
        // Approval replies and independent recipient actions remain reader-side.
        let operator_turns =
            if granted_mode == AttachMode::ReadWrite && tier == ConnectionTier::TrustedLocal {
                // Retain the deciding endpoint with the daemon-owned worker:
                // EOF removes its registry entry before queued sends may run.
                let send_connection = (tx, terminal_tx.clone());
                let (tx, mut rx) = mpsc::channel::<ClientFrame>(OPERATOR_QUEUE_DEPTH);
                let server = self.clone();
                #[cfg(any(test, feature = "test-instrumentation"))]
                let worker_hooks = server.team_send_test_hooks.clone();
                let worker = tokio::spawn(async move {
                    while let Some(frame) = rx.recv().await {
                        #[cfg(any(test, feature = "test-instrumentation"))]
                        let team_request = team_send_request_key(&frame).map(str::to_owned);
                        #[cfg(any(test, feature = "test-instrumentation"))]
                        if team_request.is_some()
                            && let Some(hooks) = worker_hooks.as_ref()
                        {
                            hooks.before_team_send().await;
                        }
                        // A detached writer's queued work still runs while no other writer has
                        // attached (AC4: the daemon owns what was submitted). Once a later
                        // writer has been granted, the stale commands are discarded so they can
                        // never act on the replacement writer's conversation.
                        if server.last_writer_conn_id.load(Ordering::SeqCst) != conn_id {
                            #[cfg(any(test, feature = "test-instrumentation"))]
                            {
                                if let (Some(request), Some(hooks)) =
                                    (team_request.as_deref(), worker_hooks.as_ref())
                                {
                                    let _order = hooks.inner.fifo_serial.lock().await;
                                    hooks.record_fifo(
                                        request,
                                        conn_id,
                                        TeamSendFifoEventKind::Discarded,
                                    );
                                }
                                while let Some(queued) = rx.recv().await {
                                    if let (Some(request), Some(hooks)) =
                                        (team_send_request_key(&queued), worker_hooks.as_ref())
                                    {
                                        let _order = hooks.inner.fifo_serial.lock().await;
                                        hooks.record_fifo(
                                            request,
                                            conn_id,
                                            TeamSendFifoEventKind::Discarded,
                                        );
                                    }
                                }
                            }
                            break;
                        }
                        #[cfg(any(test, feature = "test-instrumentation"))]
                        if let (Some(request), Some(hooks)) =
                            (team_request.as_deref(), worker_hooks.as_ref())
                        {
                            let _order = hooks.inner.fifo_serial.lock().await;
                            hooks.record_fifo(request, conn_id, TeamSendFifoEventKind::Dequeued);
                        }
                        match frame {
                            ClientFrame::TeamSend {
                                request,
                                recipients,
                                text,
                                message_type,
                                subject,
                            } => {
                                server
                                    .handle_team_send(
                                        conn_id,
                                        tier,
                                        granted_mode,
                                        request,
                                        Some(TeamSendPayload::Human {
                                            recipients,
                                            text,
                                            message_type,
                                            subject,
                                            reply_to: None,
                                        }),
                                        Some(send_connection.clone()),
                                    )
                                    .await;
                            }
                            ClientFrame::TeamFollowup {
                                request,
                                peer,
                                reply_to,
                                text,
                            } => {
                                server
                                    .handle_team_send(
                                        conn_id,
                                        tier,
                                        granted_mode,
                                        request,
                                        Some(TeamSendPayload::Human {
                                            recipients: vec![peer],
                                            text,
                                            message_type: None,
                                            subject: None,
                                            reply_to: Some(reply_to),
                                        }),
                                        Some(send_connection.clone()),
                                    )
                                    .await;
                            }
                            ClientFrame::TeamProposalDecision {
                                proposal,
                                decision: crate::domain::models::TeamProposalDecision::Send,
                            } => {
                                server
                                    .handle_team_send(
                                        conn_id,
                                        tier,
                                        granted_mode,
                                        proposal,
                                        None,
                                        Some(send_connection.clone()),
                                    )
                                    .await;
                            }
                            frame => {
                                server
                                    .handle_client_frame_tiered(frame, granted_mode, tier, conn_id)
                                    .await;
                            }
                        }
                        #[cfg(any(test, feature = "test-instrumentation"))]
                        if let (Some(request), Some(hooks)) =
                            (team_request.as_deref(), worker_hooks.as_ref())
                        {
                            let _order = hooks.inner.fifo_serial.lock().await;
                            hooks.record_fifo(request, conn_id, TeamSendFifoEventKind::Handled);
                        }
                    }
                    // Story 19.21c D3/OR-2: deregistration invalidates the retract
                    // target only after the answers this writer already queued ran.
                    #[cfg(feature = "a2a")]
                    server.team_retracts.lock().await.remove(&conn_id);
                });
                Some((tx, worker))
            } else {
                None
            };

        // Reader loop. Peer envelopes are verified against the SERVER-WIDE
        // replay window held on `self` (shared across connections).
        let mut terminal_shutdown = false;
        loop {
            let next_frame = tokio::select! {
                biased;
                _ = terminal_rx.changed() => {
                    terminal_shutdown = terminal_rx.borrow().is_some();
                    break;
                }
                frame = read_frame::<_, ClientFrame>(&mut read_half) => frame,
            };
            match next_frame {
                Ok(Some(frame)) => {
                    if let Some((turns, _worker)) = &operator_turns
                        && matches!(
                            &frame,
                            ClientFrame::UserMessage { .. }
                                | ClientFrame::HistoryRequest { .. }
                                | ClientFrame::InspectRecipientContext
                                | ClientFrame::SetRecipientContext { .. }
                                | ClientFrame::TeamSend { .. }
                                | ClientFrame::TeamFollowup { .. }
                                | ClientFrame::TeamRetractPreview { .. }
                                | ClientFrame::TeamRetractDecision { .. }
                                | ClientFrame::EnableTeamProposals
                                | ClientFrame::TeamProposalDecision { .. }
                                | ClientFrame::RevokeSenderConsent { .. }
                        )
                    {
                        #[cfg(any(test, feature = "test-instrumentation"))]
                        let team_send_request = team_send_request_key(&frame).map(str::to_owned);
                        #[cfg(any(test, feature = "test-instrumentation"))]
                        let hooks = self.team_send_test_hooks.clone();
                        #[cfg(any(test, feature = "test-instrumentation"))]
                        let _fifo_order = match hooks.as_ref() {
                            Some(hooks) => Some(hooks.inner.fifo_serial.lock().await),
                            None => None,
                        };
                        match turns.try_send(frame) {
                            Ok(()) => {
                                #[cfg(any(test, feature = "test-instrumentation"))]
                                if let (Some(request), Some(hooks)) =
                                    (team_send_request, hooks.as_ref())
                                {
                                    hooks.record_fifo(
                                        &request,
                                        conn_id,
                                        TeamSendFifoEventKind::Enqueued,
                                    );
                                }
                            }
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                terminal_shutdown = true;
                                let _ = terminal_tx.send(Some(ProtocolError::Internal(
                                    "operator command queue is full; connection closed".to_owned(),
                                )));
                                break;
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => break,
                        }
                        continue;
                    }
                    if self
                        .handle_client_frame_tiered(frame, granted_mode, tier, conn_id)
                        .await
                    {
                        break; // Detach
                    }
                }
                Ok(None) => break, // peer closed
                Err(e) => {
                    tracing::warn!(error = %e, conn = conn_id, "daemon: attach read error");
                    break;
                }
            }
        }

        // Cleanup — the turn (if any) continues daemon-side (AC4).
        {
            let mut reg = self.registry.lock().await;
            reg.remove(conn_id);
        }
        self.team_proposals.detach_writer(conn_id).await;
        drop(operator_turns);
        let terminal_error_published = if terminal_shutdown {
            writer.await.unwrap_or(false)
        } else {
            writer.abort();
            let _ = writer.await;
            false
        };
        if terminal_error_published {
            drop(read_half);
            #[cfg(any(test, feature = "test-instrumentation"))]
            if let Some(hooks) = self.team_send_test_hooks.as_ref() {
                hooks.record_terminal(conn_id, TeamSendTerminalEventKind::Closed);
            }
        }
    }

    /// Resolve one peer-response card. A fresh resolution is Applied even when
    /// the deliberate choice records no reply turn; a settled retry is
    /// Idempotent and can never masquerade as that successful no-turn result.
    async fn resolve_peer_draft(
        &self,
        node_id: &AgentId,
        resolution: super::response_modes::DraftResolution,
    ) -> Result<PeerDraftResolutionOutcome, String> {
        let _guard = self.peer_reply_serial.lock().await;
        self.resolve_peer_draft_locked(node_id, resolution, None)
            .await
    }

    async fn resolve_peer_draft_locked(
        &self,
        node_id: &AgentId,
        resolution: super::response_modes::DraftResolution,
        preloaded: Option<&crate::adapters::policy::JournalRecipientItemProjection>,
    ) -> Result<PeerDraftResolutionOutcome, String> {
        let _row_guard = self.peer_row_serial.lock().await;
        if rap_rooted_peer(node_id).is_some()
            && matches!(
                self.pending_drafts.phase(node_id.as_str()).await,
                None | Some(super::response_modes::DraftPhase::Ready)
            )
        {
            let legacy = {
                let conversation = self.conversation.lock().await;
                let row_id = format!("peer-response-{node_id}");
                conversation
                    .messages
                    .iter()
                    .find(|row| row.id == row_id)
                    .zip(conversation.peer_row_controls.get(&row_id))
                    .filter(|(_, control)| {
                        control.node_id == node_id.as_str()
                            && matches!(
                                control.stage,
                                PeerRowStage::AwaitingOperator | PeerRowStage::Ready
                            )
                    })
                    .map(|(row, control)| (row.clone(), control.clone()))
            };
            if let Some((mut row, mut control)) = legacy {
                let mode = if control.stage == PeerRowStage::AwaitingOperator {
                    crate::domain::models::ResponseMode::NotifyAndWait
                } else {
                    crate::domain::models::ResponseMode::NotifyAndDraft
                };
                row.content = super::response_modes::rap_one_way_notice(mode);
                control.stage = PeerRowStage::Settled;
                control.node_resolvable = false;
                commit_peer_rows_serialized(
                    &self.conversation,
                    &self.registry,
                    self.core.storage.as_ref(),
                    vec![PeerRowMutation::Upsert {
                        message: row,
                        control: Some(control),
                    }],
                )
                .await?;
                self.pending_drafts.abandon(node_id.as_str()).await;
                return Ok(PeerDraftResolutionOutcome::RefusedOneWay);
            }
        }
        match self.pending_drafts.phase(node_id.as_str()).await {
            Some(super::response_modes::DraftPhase::Settled) => {
                return Ok(PeerDraftResolutionOutcome::Idempotent);
            }
            Some(super::response_modes::DraftPhase::Ready) => {}
            Some(super::response_modes::DraftPhase::Drafting) => {
                return Err("peer draft is still being composed".to_owned());
            }
            None => {
                // Rebuild controller state only from a persisted pending row.
                // Settled and previously answered rows are visible no-ops.
                let persisted = {
                    let conversation = self.conversation.lock().await;
                    let row_id = format!("peer-response-{}", node_id.as_str());
                    let message = conversation
                        .messages
                        .iter()
                        .find(|message| message.id == row_id);
                    let control = conversation
                        .peer_row_controls
                        .get(&row_id)
                        .filter(|control| control.node_id == node_id.as_str());
                    if control.is_some_and(|control| {
                        control.stage == PeerRowStage::Settled || control.item_address.is_some()
                    }) {
                        return Ok(PeerDraftResolutionOutcome::Idempotent);
                    }
                    message
                        .zip(control)
                        .map(|(message, control)| (message.content.clone(), control.stage))
                }
                .ok_or_else(|| "peer response row was not found".to_owned())?;
                let generated = match persisted {
                    (content, PeerRowStage::AwaitingOperator)
                        if content == super::response_modes::AWAITING_RESPONSE_PLACEHOLDER =>
                    {
                        String::new()
                    }
                    (content, PeerRowStage::Ready) => content
                        .strip_prefix(super::response_modes::DRAFT_APPROVAL_PREFIX)
                        .map(str::to_owned)
                        .ok_or_else(|| "peer response is already settled".to_owned())?,
                    (_, PeerRowStage::Drafting) => {
                        return Err("peer draft is still being composed".to_owned());
                    }
                    (_, PeerRowStage::Settled) => {
                        return Ok(PeerDraftResolutionOutcome::Idempotent);
                    }
                    (_, PeerRowStage::AwaitingOperator) => {
                        return Err("peer response is already settled".to_owned());
                    }
                };
                if !self.pending_drafts.begin(node_id.as_str()).await
                    || !self
                        .pending_drafts
                        .complete(node_id.as_str(), generated)
                        .await
                {
                    return Err("peer response could not be reconstructed".to_owned());
                }
            }
        }
        let original = match self.pending_drafts.state(node_id.as_str()).await {
            Some(super::response_modes::DraftState::Ready { content }) => content,
            _ => return Err("peer draft is not ready for resolution".to_owned()),
        };
        let state = self
            .pending_drafts
            .resolve(node_id.as_str(), resolution)
            .await
            .ok_or_else(|| "peer draft resolution was rejected".to_owned())?;
        let (content, authorship, send_authorization, sent) = match state {
            super::response_modes::DraftState::Sent {
                content,
                authorship,
                send_authorization,
            } => (content, authorship, send_authorization, true),
            super::response_modes::DraftState::Rejected => (
                String::new(),
                crate::domain::models::Authorship::HumanWritten,
                crate::domain::models::SendAuthorization::Unknown,
                false,
            ),
            _ => return Err("peer draft did not reach a terminal resolution".to_owned()),
        };
        let journal = self
            .room_journal
            .as_ref()
            .ok_or_else(|| "room journal is unavailable".to_owned())?;
        // The reply turn precedes `PeerDraftResolved`, allowing retries after
        // a partial journal failure to reuse the one real via_node turn.
        let recorded_turn = match self
            .reply_turn_for_resolution(
                node_id,
                &content,
                authorship,
                send_authorization,
                sent,
                preloaded,
            )
            .await
        {
            Ok(turn) => turn,
            Err(error) => {
                let restored = self
                    .pending_drafts
                    .restore_ready(node_id.as_str(), original)
                    .await;
                if !restored {
                    tracing::error!(node = %node_id, "failed to roll back draft after a refused reply turn");
                }
                return Err(error);
            }
        };
        if let Err(error) = journal
            .record_event(crate::domain::models::RoomEvent::PeerDraftResolved {
                node: node_id.clone(),
                agent_composed: authorship == crate::domain::models::Authorship::AgentComposed,
                sent,
            })
            .await
        {
            let restored = self
                .pending_drafts
                .restore_ready(node_id.as_str(), original)
                .await;
            if !restored {
                tracing::error!(node = %node_id, "failed to roll back draft after journal failure");
            }
            return Err(error.to_string());
        }

        let row_id = format!("peer-response-{}", node_id.as_str());
        let (mut row, mut control) = {
            let conversation = self.conversation.lock().await;
            let Some(row) = conversation
                .messages
                .iter()
                .find(|message| message.id == row_id)
                .cloned()
            else {
                let restored = self
                    .pending_drafts
                    .restore_ready(node_id.as_str(), original)
                    .await;
                if !restored {
                    tracing::error!(node = %node_id, "failed to roll back draft after missing row");
                }
                return Err("peer draft row was not found".to_owned());
            };
            (row, conversation.peer_row_controls.get(&row_id).cloned())
        };
        if sent {
            row.send_authorization = send_authorization;
            row.content = content.clone();
            row.authorship = authorship;
            row.synthetic = false;
        } else {
            row.content = format!("[draft rejected]\n{}", row.content);
            row.synthetic = true;
        }
        if let Some(control) = &mut control {
            control.stage = PeerRowStage::Settled;
            control.node_resolvable = false;
        }
        if let Err(error) = commit_peer_rows_serialized(
            &self.conversation,
            &self.registry,
            self.core.storage.as_ref(),
            vec![PeerRowMutation::Upsert {
                message: row,
                control,
            }],
        )
        .await
        {
            // Preserve the retryable live-controller behavior when the local
            // card update itself cannot be persisted.
            let restored = self
                .pending_drafts
                .restore_ready(node_id.as_str(), original)
                .await;
            if !restored {
                tracing::error!(node = %node_id, "failed to roll back draft after persistence failure");
            }
            return Err(error);
        }

        self.node_tree.set_state(node_id, NodeState::Running).await;
        if sent {
            self.inbound_results.lock().await.insert(
                node_id.clone(),
                crate::domain::ports::StagedPeerResult {
                    text: content,
                    authorship,
                    send_authorization,
                },
            );
            self.node_tree
                .set_state(node_id, NodeState::Completed)
                .await;
        } else {
            self.node_tree
                .set_state(node_id, NodeState::Cancelled)
                .await;
        }
        self.node_tree.deregister(node_id).await;
        Ok(PeerDraftResolutionOutcome::Applied {
            reply: recorded_turn,
        })
    }

    /// Story 19.18 `AC3(a)`/`Q1` — the reply-turn half of a card resolution.
    ///
    /// Reads the journal once and joins the node to its item through the
    /// fold (`P4`). **One turn per node, enforced by the journal**: when the
    /// fold already holds a turn with this `via_node` (a retry after a
    /// partial failure, or a card answered by `/team reply` and then
    /// resolved again), nothing is appended — the same content replays the
    /// remaining steps, and a different text or a reject is refused (`M37`).
    /// A reject with no turn, and a node with no item, record nothing.
    #[cfg(feature = "a2a")]
    async fn reply_turn_for_resolution(
        &self,
        node_id: &AgentId,
        content: &str,
        authorship: crate::domain::models::Authorship,
        send_authorization: crate::domain::models::SendAuthorization,
        sent: bool,
        preloaded: Option<&crate::adapters::policy::JournalRecipientItemProjection>,
    ) -> Result<Option<RecipientReplyReceipt>, String> {
        use crate::domain::models::{
            RoomEvent, TurnAddress, claim_reply_turn_id, fresh_reply_turn_candidate,
        };

        let reader = self
            .room_journal_reader
            .as_ref()
            .ok_or_else(|| "room journal reader is unavailable".to_owned())?;
        let journal = self
            .room_journal
            .as_ref()
            .ok_or_else(|| "room journal is unavailable".to_owned())?;
        let loaded;
        let projection = if let Some(projection) = preloaded {
            projection
        } else {
            let entries = reader
                .load_entries()
                .await
                .map_err(|error| error.to_string())?;
            loaded =
                crate::adapters::policy::JournalRecipientItemProjection::from_entries(&entries);
            &loaded
        };
        if let Some(existing) = projection.reply_for_node(node_id) {
            if sent && existing.content == content {
                return Ok(Some(RecipientReplyReceipt {
                    item_id: existing.answers.as_str().to_owned(),
                    turn_id: existing.address.turn().as_str().to_owned(),
                    kind: existing.kind,
                }));
            }
            return Err(
                "this card was already answered with a recorded reply; only that same answer can \
                 be resent"
                    .to_owned(),
            );
        }
        if !sent {
            return Ok(None);
        }
        if content.len() > crate::domain::models::MAX_REPLY_BYTES {
            return Err(format!(
                "reply is longer than {} bytes — shorten it",
                crate::domain::models::MAX_REPLY_BYTES
            ));
        }
        let Some(item) = crate::adapters::a2a::exec::recipient_item_for_node(projection, node_id)
        else {
            tracing::info!(node = %node_id, "peer response resolved for a node with no recipient item; no reply turn recorded");
            return Ok(None);
        };
        let turn = claim_reply_turn_id(fresh_reply_turn_candidate(), |id| {
            projection.holds_reply_turn_id(id)
        })
        .map_err(|error| error.to_string())?;
        journal
            .record_event(RoomEvent::RecipientItemReplied {
                address: TurnAddress::answering(&item.address, turn.clone()),
                answers: item.address.item().clone(),
                content: content.to_owned(),
                kind: crate::domain::models::ReplyKind::Answer,
                authorship,
                via_node: Some(node_id.clone()),
                send_authorization,
            })
            .await
            .map_err(|error| error.to_string())?;
        Ok(Some(RecipientReplyReceipt {
            item_id: item.address.item().as_str().to_owned(),
            turn_id: turn.as_str().to_owned(),
            kind: crate::domain::models::ReplyKind::Answer,
        }))
    }

    /// Without the `a2a` feature no inbound A2A node exists, so no card
    /// resolution can answer a recipient item.
    #[cfg(not(feature = "a2a"))]
    async fn reply_turn_for_resolution(
        &self,
        _node_id: &AgentId,
        _content: &str,
        _authorship: crate::domain::models::Authorship,
        _send_authorization: crate::domain::models::SendAuthorization,
        _sent: bool,
        _preloaded: Option<&crate::adapters::policy::JournalRecipientItemProjection>,
    ) -> Result<Option<RecipientReplyReceipt>, String> {
        Ok(None)
    }

    /// Execute one operator share act against the running daemon (Story 18.4a;
    /// code-review D3 + D1).
    ///
    /// The daemon is the right home for this: it owns the one `PeerTopicStore`
    /// the context provider reads and the one transport the host identity
    /// listens on. Every refusal is produced before anything is dialed, in the
    /// same order the CLI used to check — alias, artifact, summary, listener —
    /// and each names what was **not** done.
    async fn execute_peer_share(
        &self,
        alias: &str,
        artifact_id: &str,
        topic: &str,
        summary: Option<String>,
    ) -> super::protocol::PeerShareOutcome {
        use crate::adapters::cli::peer::share::{
            ShareRefusal, derived_summary, share_refusal_text, validate_summary,
        };

        let refuse = |refusal: ShareRefusal| super::protocol::PeerShareOutcome::Refused {
            reason: share_refusal_text(alias, &refusal),
        };

        let workspace = self.core.workspace.clone();
        let config_path = crate::infrastructure::paths::workspace_p2p_config_path(&workspace);
        let (peer_id, _address, _relay) =
            match crate::infrastructure::runtime::peer_bridge::resolve_ping_target(
                &workspace,
                &config_path,
                alias,
            ) {
                Ok(target) => target,
                Err(crate::adapters::cli::peer::ping::PingRefusal::UnknownAlias) => {
                    return refuse(ShareRefusal::UnknownAlias);
                }
                Err(crate::adapters::cli::peer::ping::PingRefusal::Unpinned) => {
                    return refuse(ShareRefusal::Unpinned);
                }
                Err(crate::adapters::cli::peer::ping::PingRefusal::NoReach) => {
                    return refuse(ShareRefusal::NoReach);
                }
                Err(crate::adapters::cli::peer::ping::PingRefusal::RelayNotConfigured) => {
                    return refuse(ShareRefusal::RelayNotConfigured);
                }
                Err(other) => {
                    return refuse(ShareRefusal::LocalFault {
                        reason: crate::adapters::cli::peer::rows::ping_refusal_text(alias, &other),
                    });
                }
            };

        let artifact = match crate::infrastructure::runtime::peer_bridge::resolve_shared_artifact(
            &workspace,
            artifact_id,
        )
        .await
        {
            Ok(Some(artifact)) => artifact,
            Ok(None) => {
                return refuse(ShareRefusal::UnknownArtifact {
                    artifact: artifact_id.to_owned(),
                });
            }
            Err(error) => {
                return refuse(ShareRefusal::LocalFault {
                    reason: format!("the room journal could not be read ({error})"),
                });
            }
        };

        let summary_text = summary.unwrap_or_else(|| derived_summary(&artifact));
        let summary = match validate_summary(&summary_text) {
            Ok(summary) => summary,
            Err(refusal) => return refuse(refusal),
        };

        let handler = self.peer_delivery.read().await.clone();
        let Some(handler) = handler else {
            return refuse(ShareRefusal::LocalFault {
                reason: "the peer delivery front door is not configured on this daemon".to_owned(),
            });
        };
        match handler
            .share_handle(
                &peer_id,
                &artifact,
                &crate::domain::models::CorrelationId::new(topic),
                summary,
            )
            .await
        {
            Ok(()) => super::protocol::PeerShareOutcome::Advertised,
            Err(error) => super::protocol::PeerShareOutcome::Refused {
                reason: format!("Nothing was shared with '{alias}': {error}"),
            },
        }
    }

    async fn retract_auto_response(
        &self,
        message_id: &str,
        target_seq: Option<u64>,
    ) -> Result<(), String> {
        let journal = self
            .room_journal
            .as_ref()
            .ok_or_else(|| "room journal is unavailable".to_owned())?;
        let reader = self
            .room_journal_reader
            .as_ref()
            .ok_or_else(|| "room journal reader is unavailable".to_owned())?;
        let _row_guard = self.peer_row_serial.lock().await;
        let (mut message, mut control) = {
            let conversation = self.conversation.lock().await;
            let message = conversation
                .messages
                .iter()
                .find(|message| message.id == message_id)
                .cloned()
                .ok_or_else(|| "auto response row was not found".to_owned())?;
            (
                message,
                conversation.peer_row_controls.get(message_id).cloned(),
            )
        };
        // AC5 — the daemon owns the message↔seq binding. The retract target is
        // the journal seq of this response's `PeerDisclosure`, derived from the
        // journal itself (operator-paced path, never per-message); the frame's
        // `target_seq` is validated against it, never trusted.
        let node = message_id
            .strip_prefix("peer-response-")
            .and_then(|suffix| AgentId::parse(suffix).ok())
            .ok_or_else(|| "auto response row does not name a peer response".to_owned())?;
        let entries = reader
            .load_entries()
            .await
            .map_err(|error| error.to_string())?;
        let derived_seq = entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.record {
                crate::domain::models::JournalRecord::Room(
                    crate::domain::models::RoomEvent::PeerDisclosure {
                        node: disclosed, ..
                    },
                ) if *disclosed == node => Some(entry.seq),
                _ => None,
            })
            .ok_or_else(|| "no disclosure is journaled for this auto response".to_owned())?;
        if let Some(supplied) = target_seq
            && supplied != derived_seq
        {
            return Err(format!(
                "retraction target mismatch: the row's disclosure is journaled at seq {derived_seq}, not {supplied}"
            ));
        }

        let plan = super::response_modes::plan_auto_response_retraction(
            &message,
            derived_seq,
            true,
            self.clock.wall_now_ms(),
        )
        .map_err(|error| format!("retraction refused: {error:?}"))?;
        message = plan.message;

        journal
            .record_event(plan.event)
            .await
            .map_err(|error| error.to_string())?;
        if let Some(control) = &mut control {
            control.stage = PeerRowStage::Settled;
            control.node_resolvable = false;
        }
        commit_peer_rows_serialized(
            &self.conversation,
            &self.registry,
            self.core.storage.as_ref(),
            vec![PeerRowMutation::Upsert { message, control }],
        )
        .await
    }

    /// Gate, load, fold, find, and check the legality table for ONE act on one
    /// recipient-owned item.
    ///
    /// The legality helper for recipient-item edits. The shared lookup owns the
    /// trusted-local gate and journal projection; the caller chooses how an
    /// idempotent result or refusal is presented on its wire action.
    async fn resolve_item_for_edit(
        &self,
        mode: AttachMode,
        tier: ConnectionTier,
        act: crate::adapters::policy::recipient_item::RecipientItemAct,
        item_id: &str,
    ) -> Result<
        Option<(
            crate::domain::models::RecipientItemView,
            Arc<dyn crate::domain::ports::RoomJournal>,
        )>,
        ProtocolError,
    > {
        use crate::adapters::policy::recipient_item::{ItemActOutcome, next_item_state};

        let (item, _projection, journal) = self
            .find_recipient_item(mode, tier, act.noun(), item_id)
            .await?;
        match next_item_state(&item.state, act) {
            ItemActOutcome::Applied(_) => Ok(Some((item, journal))),
            ItemActOutcome::Idempotent => Ok(None),
            ItemActOutcome::Refused(_) => {
                Err(ProtocolError::Malformed(RECIPIENT_ITEM_REMOVED.to_owned()))
            }
        }
    }

    /// The gate + load + find prefix every recipient-item act shares —
    /// acknowledge, remove (through [`Self::resolve_item_for_edit`]) and Story
    /// 19.18's reply. One gate, one journal load, one lookup by id.
    async fn find_recipient_item(
        &self,
        mode: AttachMode,
        tier: ConnectionTier,
        noun: &str,
        item_id: &str,
    ) -> Result<
        (
            crate::domain::models::RecipientItemView,
            crate::adapters::policy::JournalRecipientItemProjection,
            Arc<dyn crate::domain::ports::RoomJournal>,
        ),
        ProtocolError,
    > {
        recipient_command_gate(mode, tier, noun, true)?;
        let reader = self
            .room_journal_reader
            .as_ref()
            .ok_or_else(|| ProtocolError::Internal("room journal reader is unavailable".into()))?;
        let journal = self
            .room_journal
            .as_ref()
            .ok_or_else(|| ProtocolError::Internal("room journal is unavailable".into()))?;
        let entries = reader
            .load_entries()
            .await
            .map_err(|error| ProtocolError::Internal(error.to_string()))?;
        let projection =
            crate::adapters::policy::JournalRecipientItemProjection::from_entries(&entries);
        let item = projection
            .find_by_id(item_id)
            .ok_or_else(|| ProtocolError::Malformed(RECIPIENT_ITEM_NOT_FOUND.to_owned()))?;
        Ok((item, projection, Arc::clone(journal)))
    }

    async fn recipient_context_inventory(&self) -> (Vec<String>, String) {
        use super::recipient_context::RecipientContextInclusion;
        let entries = match self.core.built_runtime() {
            Some(runtime) => runtime.recipient_context.inventory().await,
            None => Vec::new(),
        };
        if entries.is_empty() {
            return (
                Vec::new(),
                "no recipient items selected as peer context for this daemon conversation"
                    .to_owned(),
            );
        }
        let mut ids = Vec::with_capacity(entries.len());
        let mut text = String::from("recipient item context · this daemon conversation\n");
        for entry in entries {
            let id = entry.address.item().as_str();
            ids.push(id.to_owned());
            text.push_str(&format!(
                "  item {id} · {} · peer-tainted\n",
                crate::domain::services::transparency::item_principal_label(
                    entry.address.principal()
                ),
            ));
            let state = match entry.inclusion {
                RecipientContextInclusion::Selected => "selected · not used yet",
                RecipientContextInclusion::Full => "included",
                RecipientContextInclusion::Partial => "included partially · budget-truncated",
                RecipientContextInclusion::Skipped => "not included · budget-truncated",
                RecipientContextInclusion::Unavailable => "unavailable",
            };
            if let Some(turn) = entry.last_use_turn_id {
                text.push_str(&format!(
                    "      last turn {turn} · {} estimated tokens · {state}\n",
                    entry.estimated_tokens,
                ));
            } else {
                text.push_str(&format!("      {state}\n"));
            }
        }
        (ids, text)
    }

    async fn broadcast_recipient_context(&self) {
        let (selected_item_ids, inventory) = self.recipient_context_inventory().await;
        let frame = DaemonFrame::RecipientContextState {
            selected_item_ids,
            inventory,
            show_inventory: false,
        };
        let mut registry = self.registry.lock().await;
        let dead = registry.fanout_peer_row(&frame);
        for id in dead {
            registry.remove(id);
        }
    }

    async fn set_recipient_context(
        &self,
        conn_id: u64,
        mode: AttachMode,
        tier: ConnectionTier,
        item_id: String,
        selected: bool,
    ) {
        let action = if selected {
            RecipientAction::ContextSelect
        } else {
            RecipientAction::ContextRemove
        };
        let target = RecipientActionTarget::Item {
            item_id: item_id.clone(),
        };
        let noun = if selected {
            "context selection"
        } else {
            "context removal"
        };
        let outcome = match recipient_command_gate(mode, tier, noun, false) {
            Err(error) => RecipientActionOutcome::Refused { error },
            Ok(()) => {
                #[cfg(not(feature = "a2a"))]
                {
                    RecipientActionOutcome::Refused {
                        error: ProtocolError::Malformed(
                            "`/team context` needs the `a2a` feature; this build was compiled without it.".to_owned(),
                        ),
                    }
                }
                #[cfg(feature = "a2a")]
                {
                    let _action_guard = self.peer_reply_serial.lock().await;
                    let _row_guard = self.peer_row_serial.lock().await;
                    let result = if selected {
                        match self.core.ensure_runtime().await {
                            Ok(runtime) => {
                                runtime
                                    .recipient_context
                                    .select(&self.core.workspace, &item_id)
                                    .await
                            }
                            Err(error) => Err(anyhow::anyhow!(error.to_string())),
                        }
                    } else {
                        // Off neither initializes a provider nor resolves content.
                        Ok(match self.core.built_runtime() {
                            Some(runtime) => runtime.recipient_context.deselect(&item_id).await,
                            None => false,
                        })
                    };
                    match result {
                        Ok(changed) => {
                            self.broadcast_recipient_context().await;
                            RecipientActionOutcome::ContextSelection { selected, changed }
                        }
                        Err(error) => RecipientActionOutcome::Refused {
                            error: ProtocolError::Malformed(error.to_string()),
                        },
                    }
                }
            }
        };
        self.send_recipient_action_result(conn_id, action, target, outcome)
            .await;
    }

    /// Story 19.18 `AC2` — the `/team reply` arm: record one reply turn
    /// answering a recipient item on THIS host. ⛔ No egress, no push, and no
    /// write on the answered item (`A2`, `A4`).
    ///
    /// Order: shared gate + load + find → legality (a tombstone is refused;
    /// a retracted item is answerable) → size → author-time scrub (`P13`) →
    /// (d′) a persisted pending card for this item is resolved as `WriteOwn`
    /// with the same text (one act, one turn, the card clears) → otherwise one
    /// direct `RecipientItemReplied`. The confirmation frame is sent only
    /// after the turn is durable (`M25`); every refusal writes zero rows.
    async fn reply_to_recipient_item(
        &self,
        conn_id: u64,
        mode: AttachMode,
        tier: ConnectionTier,
        item_id: &str,
        text: String,
    ) {
        let target = RecipientActionTarget::Item {
            item_id: item_id.to_owned(),
        };
        #[cfg(not(feature = "a2a"))]
        {
            let _ = (mode, tier, &text);
            self.send_recipient_action_result(
                conn_id,
                RecipientAction::Reply,
                target,
                RecipientActionOutcome::Refused {
                    error: ProtocolError::Malformed(
                        "`/team reply` needs the `a2a` feature; this build was compiled without it."
                            .to_owned(),
                    ),
                },
            )
            .await;
        }
        #[cfg(feature = "a2a")]
        {
            use crate::domain::models::{
                Authorship, MAX_REPLY_BYTES, RecipientItemState, ReplyKind, RoomEvent, TurnAddress,
                claim_reply_turn_id, fresh_reply_turn_candidate,
            };

            let _reply_guard = self.peer_reply_serial.lock().await;
            let (item, projection, journal) =
                match self.find_recipient_item(mode, tier, "reply", item_id).await {
                    Ok(found) => found,
                    Err(error) => {
                        drop(_reply_guard);
                        self.send_recipient_action_result(
                            conn_id,
                            RecipientAction::Reply,
                            target,
                            RecipientActionOutcome::Refused { error },
                        )
                        .await;
                        return;
                    }
                };
            let refuse = |error| RecipientActionOutcome::Refused { error };
            if matches!(item.state, RecipientItemState::Removed { .. }) {
                drop(_reply_guard);
                self.send_recipient_action_result(
                    conn_id,
                    RecipientAction::Reply,
                    target,
                    refuse(ProtocolError::Malformed(RECIPIENT_ITEM_REMOVED.to_owned())),
                )
                .await;
                return;
            }
            if text.trim().is_empty() {
                drop(_reply_guard);
                self.send_recipient_action_result(
                    conn_id,
                    RecipientAction::Reply,
                    target,
                    refuse(ProtocolError::Malformed("reply text is empty".to_owned())),
                )
                .await;
                return;
            }
            if text.len() > MAX_REPLY_BYTES {
                drop(_reply_guard);
                self.send_recipient_action_result(
                    conn_id,
                    RecipientAction::Reply,
                    target,
                    refuse(ProtocolError::Malformed(format!(
                        "reply is longer than {MAX_REPLY_BYTES} bytes — shorten it"
                    ))),
                )
                .await;
                return;
            }
            let fragments =
                crate::domain::ports::InboundPeerRuntime::disclosure_forbidden_fragments(self)
                    .await;
            if crate::adapters::a2a::projection::withholds(&text, &self.core.workspace, &fragments)
            {
                drop(_reply_guard);
                self.send_recipient_action_result(
                    conn_id,
                    RecipientAction::Reply,
                    target,
                    refuse(ProtocolError::Malformed(
                        "this reply names a host path, which peers are never shown — rephrase it"
                            .to_owned(),
                    )),
                )
                .await;
                return;
            }
            if let Some(node) = self.pending_card_for_item(&projection, &item).await {
                let outcome = match self
                    .resolve_peer_draft_locked(
                        &node,
                        super::response_modes::DraftResolution::WriteOwn(text),
                        Some(&projection),
                    )
                    .await
                {
                    Ok(PeerDraftResolutionOutcome::Applied { reply: Some(reply) }) => {
                        RecipientActionOutcome::Applied { reply: Some(reply) }
                    }
                    Ok(PeerDraftResolutionOutcome::Applied { reply: None }) => {
                        refuse(ProtocolError::Internal(
                            "recipient reply completed without a durable receipt".to_owned(),
                        ))
                    }
                    Ok(PeerDraftResolutionOutcome::Idempotent) => {
                        RecipientActionOutcome::Idempotent
                    }
                    Ok(PeerDraftResolutionOutcome::RefusedOneWay) => {
                        refuse(ProtocolError::PeerVerification(
                            super::response_modes::RAP_ONE_WAY_REFUSAL.to_owned(),
                        ))
                    }
                    Err(error) => refuse(ProtocolError::Internal(error)),
                };
                drop(_reply_guard);
                self.send_recipient_action_result(conn_id, RecipientAction::Reply, target, outcome)
                    .await;
                return;
            }
            let turn = match claim_reply_turn_id(fresh_reply_turn_candidate(), |id| {
                projection.holds_reply_turn_id(id)
            }) {
                Ok(turn) => turn,
                Err(error) => {
                    drop(_reply_guard);
                    self.send_recipient_action_result(
                        conn_id,
                        RecipientAction::Reply,
                        target,
                        refuse(ProtocolError::Internal(error.to_string())),
                    )
                    .await;
                    return;
                }
            };
            let turn_id = turn.as_str().to_owned();
            let orphan_rows = self
                .orphan_response_mutations(&projection, &item, &text)
                .await;
            if let Err(error) = journal
                .record_event(RoomEvent::RecipientItemReplied {
                    address: TurnAddress::answering(&item.address, turn),
                    answers: item.address.item().clone(),
                    content: text,
                    kind: ReplyKind::Answer,
                    authorship: Authorship::HumanWritten,
                    send_authorization: crate::domain::models::SendAuthorization::HumanDecided,
                    via_node: None,
                })
                .await
            {
                drop(_reply_guard);
                self.send_recipient_action_result(
                    conn_id,
                    RecipientAction::Reply,
                    target,
                    refuse(ProtocolError::Internal(error.to_string())),
                )
                .await;
                return;
            }
            if let Err(error) = commit_peer_rows(
                &self.peer_row_serial,
                &self.conversation,
                &self.registry,
                self.core.storage.as_ref(),
                orphan_rows,
            )
            .await
            {
                tracing::error!(error = %error, "durable recipient reply could not update its orphan card");
            }
            drop(_reply_guard);
            self.send_recipient_action_result(
                conn_id,
                RecipientAction::Reply,
                target,
                RecipientActionOutcome::Applied {
                    reply: Some(RecipientReplyReceipt {
                        item_id: item_id.to_owned(),
                        turn_id,
                        kind: ReplyKind::Answer,
                    }),
                },
            )
            .await;
        }
    }

    /// Record an independent local defer turn without touching the pending
    /// peer-response card or its controller. The normal item gate and journal
    /// projection remain the sole authority for addressing and state.
    async fn defer_recipient_item(
        &self,
        conn_id: u64,
        mode: AttachMode,
        tier: ConnectionTier,
        item_id: &str,
    ) {
        let target = RecipientActionTarget::Item {
            item_id: item_id.to_owned(),
        };
        #[cfg(not(feature = "a2a"))]
        {
            let _ = (mode, tier);
            self.send_recipient_action_result(
                conn_id,
                RecipientAction::Defer,
                target,
                RecipientActionOutcome::Refused {
                    error: ProtocolError::Malformed(
                        "`/team defer` needs the `a2a` feature; this build was compiled without it."
                            .to_owned(),
                    ),
                },
            )
            .await;
        }
        #[cfg(feature = "a2a")]
        {
            use crate::domain::models::{RecipientItemState, ReplyKind};

            let _reply_guard = self.peer_reply_serial.lock().await;
            let (item, projection, journal) =
                match self.find_recipient_item(mode, tier, "defer", item_id).await {
                    Ok(found) => found,
                    Err(error) => {
                        drop(_reply_guard);
                        self.send_recipient_action_result(
                            conn_id,
                            RecipientAction::Defer,
                            target,
                            RecipientActionOutcome::Refused { error },
                        )
                        .await;
                        return;
                    }
                };
            let refuse = |error| RecipientActionOutcome::Refused { error };
            if matches!(item.state, RecipientItemState::Removed { .. }) {
                drop(_reply_guard);
                self.send_recipient_action_result(
                    conn_id,
                    RecipientAction::Defer,
                    target,
                    refuse(ProtocolError::Malformed(RECIPIENT_ITEM_REMOVED.to_owned())),
                )
                .await;
                return;
            }
            let turn = match crate::domain::models::claim_reply_turn_id(
                crate::domain::models::fresh_reply_turn_candidate(),
                |id| projection.holds_reply_turn_id(id),
            ) {
                Ok(turn) => turn,
                Err(error) => {
                    drop(_reply_guard);
                    self.send_recipient_action_result(
                        conn_id,
                        RecipientAction::Defer,
                        target,
                        refuse(ProtocolError::Internal(error.to_string())),
                    )
                    .await;
                    return;
                }
            };
            let turn_id = turn.as_str().to_owned();
            if let Err(error) = journal
                .record_event(crate::domain::models::RoomEvent::RecipientItemReplied {
                    address: crate::domain::models::TurnAddress::answering(&item.address, turn),
                    answers: item.address.item().clone(),
                    content: "Busy — defer 30m".to_owned(),
                    kind: ReplyKind::Defer { minutes: 30 },
                    authorship: crate::domain::models::Authorship::HumanWritten,
                    send_authorization: crate::domain::models::SendAuthorization::HumanDecided,
                    via_node: None,
                })
                .await
            {
                drop(_reply_guard);
                self.send_recipient_action_result(
                    conn_id,
                    RecipientAction::Defer,
                    target,
                    refuse(ProtocolError::Internal(error.to_string())),
                )
                .await;
                return;
            }
            drop(_reply_guard);
            self.send_recipient_action_result(
                conn_id,
                RecipientAction::Defer,
                target,
                RecipientActionOutcome::Applied {
                    reply: Some(RecipientReplyReceipt {
                        item_id: item_id.to_owned(),
                        turn_id,
                        kind: ReplyKind::Defer { minutes: 30 },
                    }),
                },
            )
            .await;
        }
    }

    /// A card resolves only while its process-owned controller is live.
    /// A restart orphan's item remains directly answerable; its display can
    /// be replaced without reviving or resolving the failed A2A task.
    #[cfg(feature = "a2a")]
    async fn pending_card_for_item(
        &self,
        projection: &crate::adapters::policy::JournalRecipientItemProjection,
        item: &crate::domain::models::RecipientItemView,
    ) -> Option<AgentId> {
        let nodes: Vec<(String, Option<PeerRowStage>)> = {
            let conversation = self.conversation.lock().await;
            conversation
                .messages
                .iter()
                .filter_map(|message| {
                    let node = message.id.strip_prefix(PEER_RESPONSE_ROW_PREFIX)?;
                    let stage = match conversation.peer_row_controls.get(&message.id) {
                        Some(control) if node == control.node_id.as_str() => Some(control.stage),
                        Some(_) => return None,
                        None => None,
                    };
                    Some((node.to_owned(), stage))
                })
                .collect()
        };
        for (node, stage) in nodes {
            let Ok(node) = AgentId::parse(&node) else {
                continue;
            };
            let phase = self.pending_drafts.phase(node.as_str()).await;
            let eligible = match (stage, phase) {
                (Some(PeerRowStage::Settled), _) => false,
                (_, Some(super::response_modes::DraftPhase::Drafting)) => true,
                (
                    Some(PeerRowStage::AwaitingOperator | PeerRowStage::Ready) | None,
                    Some(super::response_modes::DraftPhase::Ready),
                ) => true,
                _ => false,
            };
            if !eligible {
                // A drafting placeholder left by a crashed process is not
                // resolvable; its item remains directly answerable.
                continue;
            }
            if crate::adapters::a2a::exec::recipient_item_for_node(projection, &node)
                .is_some_and(|answered| answered.address == item.address)
            {
                return Some(node);
            }
        }
        None
    }

    #[cfg(feature = "a2a")]
    async fn orphan_response_mutations(
        &self,
        projection: &crate::adapters::policy::JournalRecipientItemProjection,
        item: &crate::domain::models::RecipientItemView,
        content: &str,
    ) -> Vec<PeerRowMutation> {
        let conversation = self.conversation.lock().await;
        let mut mutations = Vec::new();
        for message in &conversation.messages {
            let Some(control) = conversation.peer_row_controls.get(&message.id) else {
                continue;
            };
            if control.stage == PeerRowStage::Settled
                || control.item_address.as_ref() != Some(&item.address)
                || self.pending_drafts.phase(&control.node_id).await.is_some()
            {
                continue;
            }
            let Ok(node) = AgentId::parse(&control.node_id) else {
                continue;
            };
            if !crate::adapters::a2a::exec::recipient_item_for_node(projection, &node)
                .is_some_and(|joined| joined.address == item.address)
            {
                continue;
            }
            let mut settled = control.clone();
            settled.stage = PeerRowStage::Settled;
            settled.node_resolvable = false;
            mutations.push(PeerRowMutation::ReplaceContent {
                row_id: message.id.clone(),
                content: content.to_owned(),
                authorship: crate::domain::models::Authorship::HumanWritten,
                send_authorization: crate::domain::models::SendAuthorization::HumanDecided,
                control: Some(settled),
            });
        }
        mutations
    }

    /// The authenticated grant and isolated lifecycle tests enter this same
    /// targeted publication seam. It owns no timer or automatic decision.
    async fn grant_team_proposal_writer(&self, conn_id: u64) {
        if let Some((tx, terminal)) = self.team_connection(conn_id).await {
            self.team_proposals
                .grant_writer(
                    conn_id,
                    tx,
                    terminal,
                    #[cfg(any(test, feature = "test-instrumentation"))]
                    self.team_send_test_hooks.clone(),
                )
                .await;
        }
    }

    /// Returns `true` if the connection should detach.
    async fn handle_client_frame(
        &self,
        frame: ClientFrame,
        mode: AttachMode,
        conn_id: u64,
    ) -> bool {
        self.handle_client_frame_tiered(frame, mode, ConnectionTier::TrustedLocal, conn_id)
            .await
    }

    async fn handle_client_frame_tiered(
        &self,
        frame: ClientFrame,
        mode: AttachMode,
        tier: ConnectionTier,
        conn_id: u64,
    ) -> bool {
        match frame {
            ClientFrame::Detach => {
                self.send_to(conn_id, DaemonFrame::Detached).await;
                return true;
            }
            ClientFrame::UserMessage { text, .. } => {
                if mode != AttachMode::ReadWrite {
                    self.send_to(conn_id, DaemonFrame::Error(ProtocolError::ReadOnly))
                        .await;
                    return false;
                }
                self.drive_user_turn(text).await;
            }
            ClientFrame::ApprovalResponse {
                request_id,
                outcome,
            } => {
                let is_consent = self
                    .pending_consent_cards
                    .lock()
                    .await
                    .contains_key(&request_id);
                let is_consent = is_consent
                    || self
                        .approval_gate
                        .issues
                        .lock()
                        .await
                        .consent_ids
                        .contains(&request_id);
                if !is_consent {
                    self.resolve_tool_approval(request_id, outcome, mode, tier, conn_id)
                        .await;
                    return false;
                }
                if mode != AttachMode::ReadWrite {
                    self.send_to(conn_id, DaemonFrame::Error(ProtocolError::ReadOnly))
                        .await;
                    return false;
                }
                let consent_request = self
                    .pending_consent_cards
                    .lock()
                    .await
                    .remove(&request_id)
                    .is_some();
                let consent_always = consent_request
                    && matches!(
                        &outcome,
                        crate::domain::models::ApprovalOutcome::AlwaysTool { .. }
                            | crate::domain::models::ApprovalOutcome::AlwaysServer { .. }
                            | crate::domain::models::ApprovalOutcome::AlwaysAndSave { .. }
                    );
                if let Ok(runtime) = self.core.ensure_runtime().await {
                    let outcome = if consent_always {
                        let sender = runtime
                            .approval
                            .pending_source(&request_id)
                            .await
                            .and_then(|source| source.policy_principal().cloned());
                        let granted = match (&self.pending_consent, sender.as_ref()) {
                            (Some(manager), Some(sender)) => {
                                manager.grant_always(request_id.clone(), sender).await
                            }
                            _ => false,
                        };
                        if granted {
                            crate::domain::models::ApprovalOutcome::Once
                        } else {
                            crate::domain::models::ApprovalOutcome::Reject {
                                feedback: Some(
                                    "sender consent grant was not durably recorded".to_owned(),
                                ),
                            }
                        }
                    } else {
                        outcome
                    };
                    runtime
                        .approval
                        .resolve(
                            &request_id,
                            outcome,
                            crate::domain::models::PermissionResolution::Operator,
                        )
                        .await;
                }
            }
            ClientFrame::InputResponse { node, responses } => {
                if mode != AttachMode::ReadWrite {
                    self.send_to(conn_id, DaemonFrame::Error(ProtocolError::ReadOnly))
                        .await;
                    return false;
                }
                #[cfg(feature = "mcp")]
                if let Ok(rt) = self.core.ensure_runtime().await {
                    // Decode the raw `inputResponses` map into the driver's type
                    // and route to the parked node. The driver validates against
                    // `requestedSchema` (D4) and correlates by key (R-6) before
                    // forwarding `tasks/update`; a refused answer leaves the node
                    // `Waiting` (observable, not an error).
                    match serde_json::from_value::<
                        std::collections::BTreeMap<
                            String,
                            crate::adapters::mcp::tasks::InputResponse,
                        >,
                    >(responses)
                    {
                        Ok(parsed) => {
                            let node_id = match crate::domain::models::AgentId::parse(&node) {
                                Ok(node_id) => node_id,
                                Err(error) => {
                                    self.send_to(
                                        conn_id,
                                        DaemonFrame::Error(ProtocolError::Malformed(format!(
                                            "invalid InputResponse node: {error}"
                                        ))),
                                    )
                                    .await;
                                    return false;
                                }
                            };
                            let answer = crate::adapters::mcp::task_driver::InputAnswer {
                                responses: parsed,
                            };
                            let mut routed = false;
                            for runtime in &rt.mcp_task_runtimes {
                                if runtime
                                    .submit_answer(&node_id, answer.clone())
                                    .await
                                    .is_ok()
                                {
                                    routed = true;
                                    break;
                                }
                            }
                            if !routed {
                                tracing::warn!(%node_id, "InputResponse: node is not in a live Waiting MCP-task epoch");
                            }
                        }
                        Err(error) => {
                            tracing::warn!(%error, "InputResponse: malformed inputResponses map");
                        }
                    }
                }
            }
            ClientFrame::HistoryRequest {
                before_index,
                count,
            } => {
                let (messages, has_more) = {
                    let conv = self.conversation.lock().await;
                    let count = count.max(1);
                    let end = before_index.unwrap_or(conv.messages.len());
                    let end = end.min(conv.messages.len());
                    let start = end.saturating_sub(count);
                    (
                        conv.messages[start..end].iter()
                            .map(crate::infrastructure::runtime::event_bus::public_transcript_message)
                            .collect(),
                        start > 0,
                    )
                };
                self.send_to(
                    conn_id,
                    project_attach_frame(
                        DaemonFrame::History {
                            messages,
                            has_more,
                            withheld_tool_inputs: Vec::new(),
                        },
                        tier,
                    ),
                )
                .await;
            }
            ClientFrame::PeerEnvelope(envelope) => {
                if tier != ConnectionTier::Peer {
                    self.send_to(
                        conn_id,
                        DaemonFrame::Error(ProtocolError::PeerVerification(
                            "PeerEnvelope requires peer connection tier".into(),
                        )),
                    )
                    .await;
                    return false;
                }
                // Cryptographic verification reserves, but does not commit,
                // this peer's replay/feed position. The reservation rejects a
                // concurrent duplicate while semantic delivery runs without a
                // replay lock. Successful local ingest commits; every failure
                // rolls back and remains retriable.
                let reservation = {
                    let mut replay = self.replay.lock().await;
                    verify_envelope_reserved(&envelope, self.clock.wall_now_ms(), &mut replay)
                };
                match reservation {
                    Ok(reservation) => {
                        let Some(handler) = self.peer_delivery.read().await.clone() else {
                            self.replay.lock().await.rollback(&reservation);
                            self.send_to(
                                conn_id,
                                DaemonFrame::Error(ProtocolError::PeerVerification(
                                    "peer delivery is not configured".into(),
                                )),
                            )
                            .await;
                            return false;
                        };
                        let peer_id = envelope.signer.peer_id.clone();
                        match handler
                            .handle_verified_peer_frame(*envelope.clone(), peer_id)
                            .await
                        {
                            Ok(()) => {
                                if !self.replay.lock().await.commit(reservation) {
                                    self.send_to(
                                        conn_id,
                                        DaemonFrame::Error(ProtocolError::PeerVerification(
                                            "peer replay reservation was lost".into(),
                                        )),
                                    )
                                    .await;
                                    return false;
                                }
                                self.send_to(
                                    conn_id,
                                    DaemonFrame::PeerAccepted {
                                        sequence: envelope.header.sequence,
                                    },
                                )
                                .await;
                            }
                            Err(error) => {
                                self.replay.lock().await.rollback(&reservation);
                                self.send_to(
                                    conn_id,
                                    DaemonFrame::Error(ProtocolError::PeerVerification(
                                        error.to_string(),
                                    )),
                                )
                                .await;
                            }
                        }
                    }
                    Err(VerifyError::FeedForkOrGap { .. }) => {
                        self.send_to(conn_id, DaemonFrame::Error(ProtocolError::FeedForkOrGap))
                            .await;
                    }
                    Err(e) => {
                        self.send_to(
                            conn_id,
                            DaemonFrame::Error(ProtocolError::PeerVerification(e.to_string())),
                        )
                        .await;
                    }
                }
            }
            ClientFrame::ResolvePeerDraft { node, action } => {
                let target = RecipientActionTarget::Node {
                    node_id: node.clone(),
                };
                let outcome = if mode != AttachMode::ReadWrite {
                    RecipientActionOutcome::Refused {
                        error: ProtocolError::ReadOnly,
                    }
                } else if tier != ConnectionTier::TrustedLocal {
                    RecipientActionOutcome::Refused {
                        error: ProtocolError::PeerVerification(
                            "draft resolution is same-host trusted-local only".to_owned(),
                        ),
                    }
                } else {
                    match AgentId::parse(&node) {
                        Err(error) => RecipientActionOutcome::Refused {
                            error: ProtocolError::Malformed(error.to_string()),
                        },
                        Ok(node_id) => {
                            let resolution = match action {
                                super::protocol::PeerDraftAction::Approve => {
                                    super::response_modes::DraftResolution::Approve
                                }
                                super::protocol::PeerDraftAction::Edit { content } => {
                                    super::response_modes::DraftResolution::Edit(content)
                                }
                                super::protocol::PeerDraftAction::Reject => {
                                    super::response_modes::DraftResolution::Reject
                                }
                                super::protocol::PeerDraftAction::WriteOwn { content } => {
                                    super::response_modes::DraftResolution::WriteOwn(content)
                                }
                            };
                            match self.resolve_peer_draft(&node_id, resolution).await {
                                Ok(PeerDraftResolutionOutcome::Applied { reply }) => {
                                    RecipientActionOutcome::Applied { reply }
                                }
                                Ok(PeerDraftResolutionOutcome::Idempotent) => {
                                    RecipientActionOutcome::Idempotent
                                }
                                Ok(PeerDraftResolutionOutcome::RefusedOneWay) => {
                                    RecipientActionOutcome::Refused {
                                        error: ProtocolError::PeerVerification(
                                            super::response_modes::RAP_ONE_WAY_REFUSAL.to_owned(),
                                        ),
                                    }
                                }
                                Err(error) => RecipientActionOutcome::Refused {
                                    error: ProtocolError::Internal(error),
                                },
                            }
                        }
                    }
                };
                self.send_recipient_action_result(
                    conn_id,
                    RecipientAction::Response,
                    target,
                    outcome,
                )
                .await;
            }
            ClientFrame::RetractAutoResponse {
                message_id,
                target_seq,
            } => {
                if mode != AttachMode::ReadWrite {
                    self.send_to(conn_id, DaemonFrame::Error(ProtocolError::ReadOnly))
                        .await;
                    return false;
                }
                if tier != ConnectionTier::TrustedLocal {
                    self.send_to(
                        conn_id,
                        DaemonFrame::Error(ProtocolError::PeerVerification(
                            "retraction is same-host trusted-local only".to_owned(),
                        )),
                    )
                    .await;
                    return false;
                }
                if let Err(error) = self.retract_auto_response(&message_id, target_seq).await {
                    self.send_to(conn_id, DaemonFrame::Error(ProtocolError::Internal(error)))
                        .await;
                }
            }
            ClientFrame::AcknowledgeRecipientItem { item_id } => {
                let _action_guard = self.peer_reply_serial.lock().await;
                let target = RecipientActionTarget::Item {
                    item_id: item_id.clone(),
                };
                let outcome = match self
                    .find_recipient_item(mode, tier, "acknowledgement", &item_id)
                    .await
                {
                    Err(error) => RecipientActionOutcome::Refused { error },
                    Ok((item, _, journal)) => {
                        match crate::adapters::policy::recipient_item::next_item_state(
                            &item.state,
                            crate::adapters::policy::recipient_item::RecipientItemAct::Acknowledge,
                        ) {
                            crate::adapters::policy::recipient_item::ItemActOutcome::Applied(_) => {
                                match journal
                                    .record_event(
                                        crate::domain::models::RoomEvent::RecipientItemAcknowledged {
                                            address: item.address,
                                            // Keep the actual arrival attribution; the unresolved
                                            // pseudonym path deliberately has no roster alias.
                                            alias: item.alias.clone(),
                                        },
                                    )
                                    .await
                                {
                                    Ok(()) => RecipientActionOutcome::Applied { reply: None },
                                    Err(error) => RecipientActionOutcome::Refused {
                                        error: ProtocolError::Internal(error.to_string()),
                                    },
                                }
                            }
                            crate::adapters::policy::recipient_item::ItemActOutcome::Idempotent => {
                                RecipientActionOutcome::Idempotent
                            }
                            crate::adapters::policy::recipient_item::ItemActOutcome::Refused(_) => {
                                RecipientActionOutcome::Refused {
                                    error: ProtocolError::Malformed(
                                        RECIPIENT_ITEM_REMOVED.to_owned(),
                                    ),
                                }
                            }
                        }
                    }
                };
                drop(_action_guard);
                self.send_recipient_action_result(
                    conn_id,
                    RecipientAction::Acknowledge,
                    target,
                    outcome,
                )
                .await;
            }
            ClientFrame::RemoveRecipientItem { item_id } => {
                let _action_guard = self.peer_reply_serial.lock().await;
                match self
                    .resolve_item_for_edit(
                        mode,
                        tier,
                        crate::adapters::policy::recipient_item::RecipientItemAct::Remove,
                        &item_id,
                    )
                    .await
                {
                    Ok(Some((item, journal))) => {
                        // The removal record carries only the address.
                        if let Err(error) = journal
                            .record_event(crate::domain::models::RoomEvent::RecipientItemRemoved {
                                address: item.address,
                            })
                            .await
                        {
                            self.send_to(
                                conn_id,
                                DaemonFrame::Error(ProtocolError::Internal(error.to_string())),
                            )
                            .await;
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        self.send_to(conn_id, DaemonFrame::Error(error)).await;
                    }
                }
            }
            ClientFrame::ReplyToRecipientItem { item_id, text } => {
                self.reply_to_recipient_item(conn_id, mode, tier, &item_id, text)
                    .await;
            }
            ClientFrame::DeferRecipientItem { item_id } => {
                self.defer_recipient_item(conn_id, mode, tier, &item_id)
                    .await;
            }
            ClientFrame::InspectRecipientContext => {
                if tier != ConnectionTier::TrustedLocal {
                    self.send_to(
                        conn_id,
                        DaemonFrame::Error(ProtocolError::PeerVerification(
                            "recipient context inventory is same-host trusted-local only"
                                .to_owned(),
                        )),
                    )
                    .await;
                } else {
                    let (selected_item_ids, inventory) = self.recipient_context_inventory().await;
                    self.send_to(
                        conn_id,
                        DaemonFrame::RecipientContextState {
                            selected_item_ids,
                            inventory,
                            show_inventory: true,
                        },
                    )
                    .await;
                }
            }
            ClientFrame::SetRecipientContext { item_id, selected } => {
                self.set_recipient_context(conn_id, mode, tier, item_id, selected)
                    .await;
            }
            ClientFrame::PeerShare {
                alias,
                artifact,
                topic,
                summary,
            } => {
                // Story 18.4a (code-review D3) — an operator act, so the same
                // gates as every other mutating trusted-local frame.
                if mode != AttachMode::ReadWrite {
                    self.send_to(conn_id, DaemonFrame::Error(ProtocolError::ReadOnly))
                        .await;
                    return false;
                }
                if tier != ConnectionTier::TrustedLocal {
                    self.send_to(
                        conn_id,
                        DaemonFrame::Error(ProtocolError::PeerVerification(
                            "topic share is same-host trusted-local only".to_owned(),
                        )),
                    )
                    .await;
                    return false;
                }
                let outcome = self
                    .execute_peer_share(&alias, &artifact, &topic, summary)
                    .await;
                self.send_to(conn_id, DaemonFrame::PeerShareResult { outcome })
                    .await;
            }
            ClientFrame::Attach { .. } => {
                // Re-Attach mid-session is a protocol error.
                self.send_to(
                    conn_id,
                    DaemonFrame::Error(ProtocolError::Malformed("duplicate Attach".into())),
                )
                .await;
            }
            // Story 12.2d AC4/AC5/AC6/AC7 — daemon-authoritative, token-gated resolve.
            ClientFrame::ConsolidationResolve { token, accept } => {
                // AC5 — reject from read-only (mutation frame).
                if mode != AttachMode::ReadWrite {
                    self.send_to(conn_id, DaemonFrame::Error(ProtocolError::ReadOnly))
                        .await;
                    return false;
                }

                // Look up retained entry by token (NOT by queued_at_unix — the
                // resolve carries the token the client was actually shown; confused-deputy guard).
                let resolved = {
                    let mut retained = self.retained_consolidations.lock().await;
                    let mut found: Option<(u64, RetainedConsolidation)> = None;
                    for (&key, entry) in retained.iter() {
                        if entry.token == token {
                            found = Some((
                                key,
                                RetainedConsolidation {
                                    token: entry.token,
                                    proposals: entry.proposals.clone(),
                                },
                            ));
                            break;
                        }
                    }
                    if let Some((key, _)) = &found {
                        retained.remove(key);
                    }
                    found
                };

                let Some((key, entry)) = resolved else {
                    // No matching token — stale/reconnect/superseded. Reject, write nothing.
                    self.send_to(
                        conn_id,
                        DaemonFrame::Error(ProtocolError::Internal(
                            "stale or unknown consolidation token".into(),
                        )),
                    )
                    .await;
                    return false;
                };

                if accept {
                    // Accept: re-scan each fact for secrets (defense-in-depth, G10),
                    // write via memory port, THEN clear marker (G9 ordering).
                    let mut promoted = 0usize;
                    let mut secret_skipped = 0usize;
                    let mut write_errors = 0usize;
                    for pf in &entry.proposals {
                        let fact = &pf.fact;
                        let blob = format!(
                            "{}\n{}\n{}",
                            fact.category,
                            fact.fact,
                            fact.detail.as_deref().unwrap_or("")
                        );
                        if crate::domain::services::secret_scan::scan_for_secrets(&blob).is_some() {
                            tracing::warn!(
                                category = %fact.category,
                                "consolidation resolve: dropping secret-bearing fact"
                            );
                            secret_skipped += 1;
                            continue;
                        }
                        if let Err(e) = self.core.memory.remember_fact(fact.clone()).await {
                            tracing::warn!(error = %e, "daemon: consolidation remember_fact failed");
                            write_errors += 1;
                            continue;
                        }
                        promoted += 1;
                    }
                    // AC7 — await writes before clearing marker (G9).
                    // flush() is a no-op for most adapters; calls it for durability if needed.
                    if let Err(e) = self.core.memory.flush().await {
                        tracing::warn!(error = %e, "daemon: consolidation flush failed");
                    }
                    // P1+P2 (code review): clear ONLY the marker this resolve was generated
                    // from (`key`), and ONLY when no transient write error occurred. A
                    // write-error path leaves the marker intact so the next writer-attach
                    // re-proposes (never lose) — the inverse of the silent-loss G9 guards.
                    if write_errors == 0 {
                        if let Err(e) = super::session_queue::clear_consolidation_due_if(
                            &self.core.workspace,
                            key,
                        ) {
                            tracing::warn!(error = %e, "daemon: consolidation marker clear failed after apply");
                        }
                    } else {
                        tracing::warn!(
                            write_errors,
                            "daemon: consolidation had write errors — preserving marker for retry"
                        );
                    }
                    let msg = if write_errors > 0 {
                        format!(
                            "Promoted {promoted} facts ({secret_skipped} skipped, {write_errors} failed — will retry on next attach)"
                        )
                    } else if secret_skipped > 0 {
                        format!("Promoted {promoted} facts ({secret_skipped} skipped)")
                    } else {
                        format!("Promoted {promoted} facts")
                    };
                    self.send_to(
                        conn_id,
                        DaemonFrame::Event(RawEvent {
                            conversation_id: None,
                            timestamp_ms: chrono::Utc::now().timestamp_millis(),
                            kind: RawEventKind::SystemNotice {
                                level: crate::domain::models::NoticeLevel::Info,
                                message: msg,
                            },
                        }),
                    )
                    .await;
                } else {
                    // Decline: clear marker (DQ4 — user resolved), no writes. P1 — clear
                    // ONLY the resolved marker, never a newer one written meanwhile.
                    if let Err(e) =
                        super::session_queue::clear_consolidation_due_if(&self.core.workspace, key)
                    {
                        tracing::warn!(error = %e, "daemon: consolidation marker clear failed after decline");
                    }
                    self.send_to(
                        conn_id,
                        DaemonFrame::Event(RawEvent {
                            conversation_id: None,
                            timestamp_ms: chrono::Utc::now().timestamp_millis(),
                            kind: RawEventKind::SystemNotice {
                                level: crate::domain::models::NoticeLevel::Info,
                                message: "Consolidation declined".into(),
                            },
                        }),
                    )
                    .await;
                }
            }
            ClientFrame::TeamSend {
                request,
                recipients,
                text,
                message_type,
                subject,
            } => {
                self.handle_team_send(
                    conn_id,
                    tier,
                    mode,
                    request,
                    Some(TeamSendPayload::Human {
                        recipients,
                        text,
                        message_type,
                        subject,
                        reply_to: None,
                    }),
                    None,
                )
                .await;
            }
            ClientFrame::TeamFollowup {
                request,
                peer,
                reply_to,
                text,
            } => {
                self.handle_team_send(
                    conn_id,
                    tier,
                    mode,
                    request,
                    Some(TeamSendPayload::Human {
                        recipients: vec![peer],
                        text,
                        message_type: None,
                        subject: None,
                        reply_to: Some(reply_to),
                    }),
                    None,
                )
                .await;
            }
            ClientFrame::EnableTeamProposals => {
                if let Some(error) = Self::team_request_error(tier, mode, true) {
                    self.send_to(conn_id, DaemonFrame::Error(error)).await;
                    return false;
                }
                self.grant_team_proposal_writer(conn_id).await;
            }
            ClientFrame::TeamProposalDecision { proposal, decision } => {
                if let Some(error) = Self::team_request_error(tier, mode, true) {
                    self.send_to(conn_id, DaemonFrame::Error(error)).await;
                    return false;
                }
                match decision {
                    crate::domain::models::TeamProposalDecision::Send => {
                        self.handle_team_send(conn_id, tier, mode, proposal, None, None)
                            .await;
                    }
                    crate::domain::models::TeamProposalDecision::Dismiss => {
                        let outcome = if self.team_proposals.take(&proposal).await.is_some() {
                            super::protocol::TeamProposalCloseOutcome::Dismissed
                        } else {
                            super::protocol::TeamProposalCloseOutcome::Refused {
                                reason: "That proposal is no longer waiting. Nothing was sent."
                                    .to_owned(),
                            }
                        };
                        self.close_team_proposal(conn_id, proposal, outcome).await;
                    }
                }
            }
            ClientFrame::TeamBoard { request, selector } => {
                self.handle_team_board(conn_id, tier, mode, request, selector)
                    .await;
            }
            ClientFrame::TeamThread {
                request,
                peer,
                item_id,
            } => {
                self.handle_team_thread(conn_id, tier, mode, request, peer, item_id)
                    .await;
            }
            ClientFrame::TeamRetractPreview {
                request,
                peer,
                item_id,
            } => {
                self.handle_team_retract_preview(conn_id, tier, mode, request, peer, item_id)
                    .await;
            }
            ClientFrame::TeamRetractDecision { request, confirm } => {
                self.handle_team_retract_decision(conn_id, tier, mode, request, confirm)
                    .await;
            }
            ClientFrame::TeamStatus { request } => {
                self.handle_team_status(conn_id, tier, mode, request).await;
            }
            ClientFrame::RevokeSenderConsent { request, alias } => {
                self.handle_team_revoke(conn_id, tier, mode, request, alias)
                    .await;
            }
        }
        false
    }

    /// Build the runtime (first activity), spawn the approval gate once, drive
    /// the turn. The turn emits to the daemon bus; the forwarder fans out + folds.
    async fn drive_user_turn(&self, text: String) {
        if text.trim() == "/clear" {
            let _turn_guard = self.turn_serial.lock().await;
            let _action_guard = self.peer_reply_serial.lock().await;
            if let Some(handler) = self.peer_delivery.read().await.clone() {
                handler.clear_all_taint().await;
            }
            if let Err(error) = clear_conversation_rows(
                &self.pending_drafts,
                &self.peer_row_serial,
                &self.conversation,
                &self.registry,
                self.core.storage.as_ref(),
            )
            .await
            {
                tracing::warn!(%error, "clearing conversation rows failed");
            } else {
                let conversation_id = self.conversation.lock().await.id.clone();
                self.team_proposals
                    .withdraw_conversation(&conversation_id)
                    .await;
                let _row_guard = self.peer_row_serial.lock().await;
                if let Some(runtime) = self.core.built_runtime() {
                    runtime.recipient_context.clear().await;
                }
                self.broadcast_recipient_context().await;
            }
            return;
        }
        let _ = self
            .drive_user_turn_inner(text, ChannelKind::Terminal, None, TurnOrigin::Interactive)
            .await;
    }

    async fn drive_channel_turn(&self, req: ChannelTurnRequest) {
        let _ = self
            .drive_user_turn_inner(
                req.text,
                req.origin,
                Some(req.response_tx),
                TurnOrigin::Channel,
            )
            .await;
    }

    async fn drive_user_turn_inner(
        &self,
        text: String,
        origin: ChannelKind,
        response_tx: Option<tokio::sync::oneshot::Sender<String>>,
        turn_origin: TurnOrigin,
    ) -> Result<(), String> {
        let _turn_guard = self.turn_serial.lock().await;
        *self.active_channel_origin.lock().await = origin;
        *self.pending_channel_response_tx.lock().await = response_tx;
        self.operator_turn_active.store(true, Ordering::Release);

        let rt = match self.core.ensure_runtime().await {
            Ok(rt) => rt,
            Err(e) => {
                tracing::error!(error = %e, "daemon: building turn runtime failed");
                self.resolve_pending_channel_response(CHANNEL_TURN_FAILED_REPLY)
                    .await;
                self.operator_turn_active.store(false, Ordering::Release);
                *self.active_channel_origin.lock().await = ChannelKind::Terminal;
                return Err(e.to_string());
            }
        };
        self.ensure_approval_gate(rt.approval.clone()).await;

        let turn_complete = self.turn_complete.notified();
        let handle = {
            let mut conv = self.conversation.lock().await;
            rt.drive_turn(
                text,
                origin,
                &mut conv,
                &self.domain_tx,
                turn_origin,
                CancellationToken::new(),
            )
            .await
        };

        if let Err(e) = handle.await {
            tracing::warn!(error = ?e, "daemon turn task failed");
            self.resolve_pending_channel_response(CHANNEL_TURN_FAILED_REPLY)
                .await;
            self.operator_turn_active.store(false, Ordering::Release);
            *self.active_channel_origin.lock().await = ChannelKind::Terminal;
            return Err(e.to_string());
        }
        let folded = tokio::time::timeout(std::time::Duration::from_secs(5), turn_complete).await;
        if folded.is_err() {
            tracing::warn!(
                "daemon turn completed but assistant commit was not observed before timeout"
            );
        }
        self.resolve_pending_channel_response(CHANNEL_TURN_FAILED_REPLY)
            .await;
        self.operator_turn_active.store(false, Ordering::Release);
        *self.active_channel_origin.lock().await = ChannelKind::Terminal;
        Ok(())
    }

    async fn resolve_pending_channel_response(&self, fallback: &str) {
        if let Some(tx) = self.pending_channel_response_tx.lock().await.take() {
            let _ = tx.send(fallback.to_string());
        }
    }

    /// Spawn the headless approval gate exactly once (AC6).
    ///
    /// The subscription is taken **here**, synchronously, not inside the spawned
    /// task: `broadcast` only delivers to receivers that already exist, so a
    /// `request()` issued between the spawn and the task's first poll would be
    /// broadcast into the void. Story 18.1b made that reachable — an inbound
    /// A2A admission
    /// approval is raised immediately after the gate is armed, with no turn in
    /// between to absorb the gap.
    async fn ensure_approval_gate(&self, approval: Arc<ApprovalRuntime>) {
        ensure_approval_gate_once(
            approval,
            self.registry.clone(),
            self.blocked_waiting.clone(),
            self.conversation.clone(),
            self.core.storage.clone(),
            self.domain_tx.clone(),
            self.pending_consent_cards.clone(),
            self.approval_gate.clone(),
        )
        .await;
    }

    /// Admission is independent of the operator FIFO: the waiting turn owns
    /// turn_serial. Claim the issue before resolving so duplicate socket frames
    /// are refused even while durable Always persistence is in flight.
    async fn resolve_tool_approval(
        &self,
        request_id: crate::domain::models::RequestId,
        outcome: crate::domain::models::ApprovalOutcome,
        mode: AttachMode,
        tier: ConnectionTier,
        conn_id: u64,
    ) {
        use super::protocol::ApprovalRefusal;
        use crate::domain::services::approval_runtime::ReviewScope;
        // Decisions never create a provider/runtime. Reject the connection
        // boundary before even looking up pending work.
        let authorized = mode == AttachMode::ReadWrite
            && tier == ConnectionTier::TrustedLocal
            && self
                .registry
                .lock()
                .await
                .approval_writer()
                .is_some_and(|conn| conn.id == conn_id);
        let runtime = if authorized {
            self.core.built_runtime()
        } else {
            None
        };
        let pending = match &runtime {
            Some(runtime) => runtime
                .approval
                .pending_review(&request_id, ReviewScope::AllTools)
                .await
                .is_some(),
            None => false,
        };
        let refusal = {
            let mut issues = self.approval_gate.issues.lock().await;
            let registry = self.registry.lock().await;
            if mode != AttachMode::ReadWrite
                || tier != ConnectionTier::TrustedLocal
                || !registry
                    .approval_writer()
                    .is_some_and(|conn| conn.id == conn_id)
            {
                Some(ApprovalRefusal::WriterOnly)
            } else if self.approval_gate.shutdown.is_cancelled()
                || !pending
                || issues.deciding.contains(&request_id)
            {
                Some(ApprovalRefusal::NoLongerPending)
            } else if issues.issued.get(&request_id) != Some(&Some(conn_id)) {
                Some(ApprovalRefusal::IssuedElsewhere)
            } else {
                issues.deciding.insert(request_id.clone());
                None
            }
        };
        if let Some(reason) = refusal {
            let mut registry = self.registry.lock().await;
            if let Some(conn) = registry.conns.iter().find(|conn| conn.id == conn_id) {
                if try_publish_team_frame(
                    &conn.tx,
                    &conn.terminal_tx,
                    DaemonFrame::ApprovalRefused { request_id, reason },
                ) != TeamFramePublishResult::Published
                {
                    registry.remove(conn_id);
                }
            }
        } else if let Some(runtime) = runtime {
            runtime
                .approval
                .resolve(
                    &request_id,
                    outcome,
                    crate::domain::models::PermissionResolution::Operator,
                )
                .await;
        }
    }

    async fn retained_approval_count(&self) -> usize {
        let pending = self.approval_gate.pending_tool_ids().await;
        let issues = self.approval_gate.issues.lock().await;
        let registry = self.registry.lock().await;
        pending
            .iter()
            .filter(|id| {
                !issues.deciding.contains(*id)
                    && !issues.issued.get(*id).copied().flatten().is_some_and(|id| {
                        registry.approval_writer().is_some_and(|conn| conn.id == id)
                    })
            })
            .count()
    }

    async fn emit_pending_consent_cards(&self, conn_id: u64) {
        let cards: Vec<_> = self
            .pending_consent_cards
            .lock()
            .await
            .values()
            .cloned()
            .collect();
        for card in cards {
            let request_id = match &card {
                DaemonFrame::ApprovalRequest { request_id, .. } => request_id.clone(),
                _ => continue,
            };
            if !self.send_to(conn_id, card).await {
                self.pending_consent_cards.lock().await.remove(&request_id);
                if let Ok(runtime) = self.core.ensure_runtime().await {
                    runtime
                        .approval
                        .resolve(
                            &request_id,
                            crate::domain::models::ApprovalOutcome::Reject {
                                feedback: Some(
                                    "sender consent failed: attached writer queue is full"
                                        .to_owned(),
                                ),
                            },
                            crate::domain::models::PermissionResolution::Policy,
                        )
                        .await;
                }
                self.blocked_waiting.fetch_add(1, Ordering::SeqCst);
                record_blocked_action(
                    &self.conversation,
                    &self.core.storage,
                    "a2a/sender-consent",
                    crate::domain::models::HistorySource::PeerDisplay,
                )
                .await;
                crate::domain::ports::EventEmitter::emit(
                    &crate::infrastructure::runtime::event_bus::ChannelEmitter::new(
                        self.domain_tx.clone(),
                    ),
                    AppEvent::SystemNotice {
                        conversation_id: None,
                        level: crate::domain::models::NoticeLevel::Warning,
                        message:
                            "Sender consent card admission failed: the attached writer queue is full."
                                .to_owned(),
                    },
                );
            }
        }
    }

    /// Story 12.2c AC7 + 12.2d AC1/AC2/AC6 — surface the 12.1c boundary queues to
    /// a freshly-attached writer.
    ///
    /// - **Purge notice:** emitted then cleared (once-only delivery, 12.2c).
    /// - **Consolidation-due marker:** the rich card path (12.2d). Checks retained
    ///   proposals for reuse (G7), generates if needed, sends
    ///   `DaemonFrame::ConsolidationProposed` to writer only.
    async fn emit_session_queue_notices(&self, conn_id: u64) {
        let workspace = &self.core.workspace;
        let now = chrono::Utc::now().timestamp_millis();

        // ── Purge notice (12.2c AC7 — unchanged) ──
        if let Some(notice) = super::session_queue::read_purge_notice(workspace) {
            let enqueued = self
                .send_to(
                    conn_id,
                    DaemonFrame::Event(RawEvent {
                        conversation_id: None,
                        timestamp_ms: now,
                        kind: RawEventKind::SystemNotice {
                            level: crate::domain::models::NoticeLevel::Info,
                            message: notice.message(),
                        },
                    }),
                )
                .await;
            if enqueued {
                if let Err(e) = super::session_queue::clear_purge_notice(workspace) {
                    tracing::warn!(error = %e, "daemon: purge notice was emitted but could not be cleared");
                }
            } else {
                tracing::warn!(
                    "daemon: purge notice left queued because attach notice delivery failed"
                );
            }
        }

        // ── Consolidation card (12.2d AC1/AC2/AC6) ──
        // Replaces the 12.2c one-liner pointer. This is the ONE touch of the
        // 12.2c-owned function.
        if let Some(marker) = super::session_queue::read_consolidation_due(workspace) {
            let queued_at = marker.queued_at_unix;

            // G7 — check for a retained entry keyed by queued_at_unix (reuse, no respend).
            // P7 (code review): clone the payload and DROP the guard before the socket
            // write, so the retained-map lock is never held across `.await` (project
            // async-lock policy — release guards before await points).
            let reuse = {
                let retained = self.retained_consolidations.lock().await;
                retained
                    .get(&queued_at)
                    .map(|existing| (existing.token, existing.proposals.clone()))
            };
            if let Some((token, proposals)) = reuse {
                // Reuse: re-emit the same token+proposals, zero LLM call.
                self.send_to(
                    conn_id,
                    DaemonFrame::ConsolidationProposed { token, proposals },
                )
                .await;
                return;
            }

            // G8 — a new marker (different queued_at_unix) ⇒ the generation task evicts
            // any superseded entry (`map.retain(|k,_| *k == queued_at)`) before inserting.

            // AC1 step 3 — check for empty recent entries.
            let entries = match self.core.memory.recent(30).await {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(error = %e, "daemon: failed to read recent entries for consolidation");
                    return;
                }
            };
            if entries.is_empty() {
                // Nothing to consolidate → clear marker, emit info, no card.
                if let Err(e) = super::session_queue::clear_consolidation_due(workspace) {
                    tracing::warn!(error = %e, "daemon: could not clear consolidation marker on empty recent");
                }
                self.send_to(
                    conn_id,
                    DaemonFrame::Event(RawEvent {
                        conversation_id: None,
                        timestamp_ms: now,
                        kind: RawEventKind::SystemNotice {
                            level: crate::domain::models::NoticeLevel::Info,
                            message: "Nothing to consolidate yet".into(),
                        },
                    }),
                )
                .await;
                return;
            }

            // AC1 step 2+4 — ensure runtime built, emit "Reviewing…" notice, spawn generation.
            let rt = match self.core.ensure_runtime().await {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::warn!(error = %e, "daemon: could not build runtime for consolidation generation");
                    // Don't clear marker — retry next attach (AC6: timeout/provider-error).
                    return;
                }
            };

            // P3 (code review) — reserve this marker's generation slot. If a generation
            // is already in flight for the same `queued_at_unix` (a re-attach during the
            // window before the retained entry lands), do NOT spawn a duplicate
            // `generate_proposals` call (G7: exactly one call per marker). The in-flight
            // task delivers the card; this re-attach returns silently (no orphan notice).
            {
                let mut inflight = self.generating_consolidations.lock().await;
                if !inflight.insert(queued_at) {
                    return;
                }
            }

            // Emit "Reviewing recent activity…" notice (matches event_loop.rs:2116-2120).
            self.send_to(
                conn_id,
                DaemonFrame::Event(RawEvent {
                    conversation_id: None,
                    timestamp_ms: now,
                    kind: RawEventKind::SystemNotice {
                        level: crate::domain::models::NoticeLevel::Info,
                        message: "Reviewing recent activity for durable facts…".into(),
                    },
                }),
            )
            .await;

            // Resolve model via the same pattern as DaemonTurnRuntime::drive_turn.
            let model = {
                let config = rt.app_config.load_full();
                use crate::domain::services::model_router::{
                    ModelResolutionRequest, resolve_effective_model,
                };
                let req = ModelResolutionRequest {
                    explicit_override: None,
                    tier_hint: None,
                    step_kind: None,
                    retry_count: 0,
                    input_tokens: 0,
                    fallback_model: config.model.clone(),
                };
                resolve_effective_model(&req, &config.router).model.clone()
            };

            // Spawn generation AFTER AttachAck (NFR49 safe — structurally off handshake path).
            let prompt_body =
                crate::domain::services::consolidation::build_proposal_prompt(&entries);
            let provider = rt.provider.clone();
            let retained = self.retained_consolidations.clone();
            let token_counter = self.next_proposal_token.clone();
            let generating = self.generating_consolidations.clone();
            let send_tx = {
                let reg = self.registry.lock().await;
                reg.conns
                    .iter()
                    .find(|c| c.id == conn_id)
                    .map(|c| c.tx.clone())
            };
            let workspace = workspace.to_path_buf();

            tokio::spawn(async move {
                use crate::domain::services::consolidation::CONSOLIDATION_TIMEOUT;

                let result = tokio::time::timeout(
                    CONSOLIDATION_TIMEOUT,
                    crate::domain::services::consolidation::generate_proposals(
                        &*provider,
                        &model,
                        &prompt_body,
                    ),
                )
                .await;

                match result {
                    Ok(Ok(facts)) if !facts.is_empty() => {
                        // Mint token + per-item ProposalId (Fork-C): the id is the stable
                        // handle the per-item-toggle fast-follow (AI-12.2d-2) filters on.
                        let token_val = token_counter.fetch_add(1, Ordering::SeqCst);
                        let token = super::protocol::ProposalToken(token_val);
                        let proposals: Vec<super::protocol::ProposedFact> = facts
                            .into_iter()
                            .enumerate()
                            .map(|(i, fact)| super::protocol::ProposedFact {
                                id: super::protocol::ProposalId(i as u32),
                                fact,
                            })
                            .collect();
                        let frame = DaemonFrame::ConsolidationProposed {
                            token,
                            proposals: proposals.clone(),
                        };
                        // Store in retained map. G8 — evict any superseded entry (a stale
                        // `queued_at_unix` from an earlier boundary) before inserting, so the
                        // map stays bounded and no stale token survives (review P4).
                        {
                            let mut map = retained.lock().await;
                            map.retain(|k, _| *k == queued_at);
                            map.insert(queued_at, RetainedConsolidation { token, proposals });
                        }
                        // Send to writer only.
                        if let Some(tx) = send_tx {
                            // bounded mpsc::Sender::send is async — must await or the
                            // frame future is dropped and the card never reaches the writer.
                            let _ = tx.send(frame).await;
                        }
                    }
                    Ok(Ok(_)) => {
                        // Empty proposals → clear marker, emit info (no card).
                        if let Err(e) = super::session_queue::clear_consolidation_due(&workspace) {
                            tracing::warn!(error = %e, "daemon: could not clear consolidation marker on empty proposals");
                        }
                        // Best-effort info notice (connection may be gone).
                        if let Some(tx) = send_tx {
                            let _ = tx
                                .send(DaemonFrame::Event(RawEvent {
                                    conversation_id: None,
                                    timestamp_ms: chrono::Utc::now().timestamp_millis(),
                                    kind: RawEventKind::SystemNotice {
                                        level: crate::domain::models::NoticeLevel::Info,
                                        message: "Nothing worth promoting from recent activity"
                                            .into(),
                                    },
                                }))
                                .await;
                        }
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "daemon: consolidation generation failed");
                        // Don't clear marker — retry next attach (AC6: transient).
                    }
                    Err(_) => {
                        tracing::warn!("daemon: consolidation generation timed out");
                        // Don't clear marker — retry next attach (AC6: transient).
                    }
                }
                // P3 — release the in-flight slot on every completion path so the next
                // writer-attach can reuse (G7 hit) or regenerate (transient failure).
                generating.lock().await.remove(&queued_at);
            });
        }
    }

    async fn send_recipient_action_result(
        &self,
        conn_id: u64,
        action: RecipientAction,
        target: RecipientActionTarget,
        outcome: RecipientActionOutcome,
    ) {
        let tx = {
            let registry = self.registry.lock().await;
            registry
                .conns
                .iter()
                .find(|connection| connection.id == conn_id)
                .map(|connection| connection.tx.clone())
        };
        if let Some(tx) = tx {
            let _ = tx
                .send(DaemonFrame::RecipientActionResult {
                    action,
                    target,
                    outcome,
                })
                .await;
        }
    }

    async fn send_to(&self, conn_id: u64, frame: DaemonFrame) -> bool {
        let tx = {
            let reg = self.registry.lock().await;
            reg.conns
                .iter()
                .find(|c| c.id == conn_id)
                .map(|c| c.tx.clone())
        };
        if let Some(tx) = tx {
            tx.try_send(frame).is_ok()
        } else {
            false
        }
    }
}

/// Issue from the private pending snapshot. Only nonblocking queue writes run
/// inside this gate, so delivery cannot outlive its gate or follow its close.
async fn issue_tool_approval(
    approval: &ApprovalRuntime,
    registry: &Mutex<ConnRegistry>,
    gate: &ApprovalGate,
    id: &crate::domain::models::RequestId,
) -> Option<crate::domain::services::approval_runtime::PendingReview> {
    use crate::domain::services::approval_runtime::ReviewScope;
    #[cfg(any(test, feature = "test-instrumentation"))]
    let _attempt = ApprovalTransferAttempt(&gate.transfer_attempts);
    let review = approval.pending_review(id, ReviewScope::AllTools).await?;
    let writer_id = {
        let mut issues = gate.issues.lock().await;
        let registry = registry.lock().await;
        if gate.shutdown.is_cancelled() || issues.deciding.contains(id) {
            return None;
        }
        let prior = issues.issued.get(id).copied();
        let Some(writer) = registry.approval_writer() else {
            issues.issued.insert(id.clone(), None);
            return prior.is_none().then_some(review);
        };
        if prior == Some(Some(writer.id)) {
            return None;
        }
        issues.issued.insert(id.clone(), None);
        if !writer.approval_ready {
            return None;
        }
        writer.id
    };
    let serialized =
        serde_json::to_string(&*review.input).expect("a pending JSON value always serializes");
    let chunks = super::protocol::exact_input_chunks(&serialized);
    let frame = DaemonFrame::ApprovalRequest {
        request_id: id.clone(),
        tool: review.tool.clone(),
        source: review.source.clone(),
        risk: review.risk,
        summary: super::protocol::bounded_wire_summary(
            crate::domain::services::approval_presentation::summary(
                &review.tool,
                &review.input,
                true,
            ),
        ),
        input_bytes: Some(serialized.len() as u64),
    };
    let mut issues = gate.issues.lock().await;
    let mut registry = registry.lock().await;
    if gate.shutdown.is_cancelled() || issues.deciding.contains(id) {
        return None;
    }
    let writer = registry
        .approval_writer()
        .filter(|writer| writer.id == writer_id && writer.approval_ready)?;
    issues.issued.insert(id.clone(), Some(writer_id));
    let tx = writer.tx.clone();
    let terminal_tx = writer.terminal_tx.clone();
    let frames =
        std::iter::once(frame)
            .chain(chunks.iter().enumerate().map(|(sequence, data)| {
                DaemonFrame::ApprovalInputChunk {
                    request_id: id.clone(),
                    sequence: sequence as u32,
                    data: (*data).to_owned(),
                }
            }))
            .chain(std::iter::once(DaemonFrame::ApprovalInputComplete {
                request_id: id.clone(),
                chunks: chunks.len() as u32,
            }));
    for frame in frames {
        if gate.shutdown.is_cancelled() {
            break;
        }
        if try_publish_team_frame(&tx, &terminal_tx, frame) != TeamFramePublishResult::Published {
            // Proposal rule, not consent's reject-and-record rule. Removal is
            // immediate so another attach can claim the writer slot.
            registry.remove(writer_id);
            issues.issued.insert(id.clone(), None);
            break;
        }
    }
    None
}

async fn apply_unattended_tool_policy(
    approval: &ApprovalRuntime,
    review: crate::domain::services::approval_runtime::PendingReview,
    blocked: &AtomicUsize,
    conversation: &Arc<Mutex<Conversation>>,
    storage: &Arc<dyn crate::domain::ports::StoragePort>,
) {
    if review.risk == ToolRisk::Safe {
        approval
            .resolve(
                &review.id,
                crate::domain::models::ApprovalOutcome::Once,
                crate::domain::models::PermissionResolution::Policy,
            )
            .await;
    } else {
        approval
            .resolve(
                &review.id,
                crate::domain::models::ApprovalOutcome::Reject {
                    feedback: Some(
                        "denied: a side-effecting tool needs approval but no client is attached"
                            .into(),
                    ),
                },
                crate::domain::models::PermissionResolution::Policy,
            )
            .await;
        blocked.fetch_add(1, Ordering::SeqCst);
        let history_source = if matches!(
            review.source,
            crate::domain::models::tool_call::ApprovalSource::RemotePeer { .. }
        ) {
            crate::domain::models::HistorySource::PeerDisplay
        } else {
            crate::domain::models::HistorySource::Local
        };
        record_blocked_action(conversation, storage, &review.tool, history_source).await;
    }
}

async fn close_tool_approval(
    gate: &ApprovalGate,
    registry: &Mutex<ConnRegistry>,
    id: &crate::domain::models::RequestId,
) {
    let mut issues = gate.issues.lock().await;
    issues.deciding.remove(id);
    if let Some(Some(conn_id)) = issues.issued.remove(id) {
        let mut registry = registry.lock().await;
        if let Some(conn) = registry.conns.iter().find(|conn| conn.id == conn_id) {
            if try_publish_team_frame(
                &conn.tx,
                &conn.terminal_tx,
                DaemonFrame::ApprovalClosed {
                    request_id: id.clone(),
                },
            ) != TeamFramePublishResult::Published
            {
                registry.remove(conn_id);
            }
        }
    }
}

/// Story 19.20c: tool approvals have no reading deadline; sender consent keeps
/// its independent admission and replay semantics.
async fn run_approval_gate(
    mut rx: tokio::sync::broadcast::Receiver<ApprovalRuntimeEvent>,
    approval: Arc<ApprovalRuntime>,
    registry: Arc<Mutex<ConnRegistry>>,
    blocked: Arc<AtomicUsize>,
    conversation: Arc<Mutex<Conversation>>,
    storage: Arc<dyn crate::domain::ports::StoragePort>,
    domain_tx: mpsc::UnboundedSender<AppEvent>,
    pending_consent_cards: Arc<
        Mutex<std::collections::HashMap<crate::domain::models::RequestId, DaemonFrame>>,
    >,
    gate: Arc<ApprovalGate>,
    mut replay: watch::Receiver<u64>,
) {
    use crate::domain::services::approval_runtime::ReviewScope;
    #[cfg(any(test, feature = "test-instrumentation"))]
    let _active_guard = {
        gate.active.fetch_add(1, Ordering::SeqCst);
        ApprovalGateRunGuard(&gate.active)
    };
    let event_emitter = crate::infrastructure::runtime::event_bus::ChannelEmitter::new(domain_tx);
    loop {
        let event = tokio::select! {
            biased;
            _ = gate.shutdown.cancelled() => break,
            changed = replay.changed() => {
                if changed.is_err() { break; }
                #[cfg(any(test, feature = "test-instrumentation"))]
                if let Some(mut release) = gate.replay_hold.lock().await.take() {
                    gate.replay_entered.add_permits(1);
                    tokio::select! {
                        _ = gate.shutdown.cancelled() => break,
                        _ = &mut release => {}
                    }
                }
                for request in approval.pending_tool_requests(ReviewScope::AllTools).await {
                    if gate.shutdown.is_cancelled() { break; }
                    if let Some(review) = issue_tool_approval(&approval, &registry, &gate, &request.id).await {
                        apply_unattended_tool_policy(&approval, review, &blocked, &conversation, &storage).await;
                    }
                }
                #[cfg(any(test, feature = "test-instrumentation"))]
                gate.replay_completed.add_permits(1);
                continue;
            }
            event = rx.recv() => event,
        };
        match event {
            Ok(ApprovalRuntimeEvent::Requested {
                id,
                tool,
                risk,
                source,
                consent_card,
            }) => {
                if tool != "a2a/sender-consent" {
                    if let Some(review) =
                        issue_tool_approval(&approval, &registry, &gate, &id).await
                    {
                        apply_unattended_tool_policy(
                            &approval,
                            review,
                            &blocked,
                            &conversation,
                            &storage,
                        )
                        .await;
                    }
                    continue;
                }
                gate.issues.lock().await.consent_ids.insert(id.clone());
                let history_source = if matches!(
                    source,
                    crate::domain::models::tool_call::ApprovalSource::RemotePeer { .. }
                ) {
                    crate::domain::models::HistorySource::PeerDisplay
                } else {
                    crate::domain::models::HistorySource::Local
                };
                let writer = { registry.lock().await.writer_tx() };
                let frame = DaemonFrame::ApprovalRequest {
                    request_id: id.clone(),
                    tool: tool.clone(),
                    source,
                    risk,
                    summary: consent_card.unwrap_or_default(),
                    input_bytes: None,
                };
                pending_consent_cards
                    .lock()
                    .await
                    .insert(id.clone(), frame.clone());
                if let Some(tx) = writer {
                    if let Err(error) = tx.try_send(frame) {
                        pending_consent_cards.lock().await.remove(&id);
                        approval
                            .resolve(
                                &id,
                                crate::domain::models::ApprovalOutcome::Reject {
                                    feedback: Some(
                                        "sender consent failed: attached writer queue is full"
                                            .to_owned(),
                                    ),
                                },
                                crate::domain::models::PermissionResolution::Policy,
                            )
                            .await;
                        blocked.fetch_add(1, Ordering::SeqCst);
                        record_blocked_action(&conversation, &storage, &tool, history_source).await;
                        crate::domain::ports::EventEmitter::emit(
                            &event_emitter,
                            AppEvent::SystemNotice {
                                conversation_id: None,
                                level: crate::domain::models::NoticeLevel::Warning,
                                message: "Sender consent card admission failed: the attached writer queue is full.".to_owned(),
                            },
                        );
                        tracing::warn!(?error, "sender consent failed: writer queue full");
                    }
                } else {
                    tracing::info!(request_id = ?id, "sender consent remains pending for the next designated writer");
                }
            }
            Ok(ApprovalRuntimeEvent::Resolved { id, .. })
            | Ok(ApprovalRuntimeEvent::Cancelled { id, .. }) => {
                pending_consent_cards.lock().await.remove(&id);
                close_tool_approval(&gate, &registry, &id).await;
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(
                    missed = n,
                    "approval gate lagged — re-scanning pending approvals"
                );
                let pending = approval.pending_tool_requests(ReviewScope::AllTools).await;
                let pending_ids = gate.pending_tool_ids().await;
                let stale: Vec<_> = gate
                    .issues
                    .lock()
                    .await
                    .issued
                    .keys()
                    .filter(|id| !pending_ids.contains(*id))
                    .cloned()
                    .collect();
                for id in stale {
                    close_tool_approval(&gate, &registry, &id).await;
                }
                for request in pending {
                    if gate.shutdown.is_cancelled() {
                        break;
                    }
                    if let Some(review) =
                        issue_tool_approval(&approval, &registry, &gate, &request.id).await
                    {
                        apply_unattended_tool_policy(
                            &approval,
                            review,
                            &blocked,
                            &conversation,
                            &storage,
                        )
                        .await;
                    }
                }
                // Sender consent deliberately retains its existing rescan.
                for (id, card, risk) in approval.pending_sender_consent().await {
                    let Some(source) = approval.pending_source(&id).await else {
                        continue;
                    };
                    gate.issues.lock().await.consent_ids.insert(id.clone());
                    let mut cards = pending_consent_cards.lock().await;
                    if cards.contains_key(&id) {
                        continue;
                    }
                    let frame = DaemonFrame::ApprovalRequest {
                        request_id: id.clone(),
                        tool: "a2a/sender-consent".to_owned(),
                        source,
                        risk,
                        summary: card,
                        input_bytes: None,
                    };
                    cards.insert(id, frame.clone());
                    drop(cards);
                    let writer = { registry.lock().await.writer_tx() };
                    if let Some(tx) = writer {
                        let _ = tx.try_send(frame);
                    }
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
    // Rule 4: a shutdown keystone can hold the real task at its exit boundary
    // and prove the server waits for its JoinHandle, without a scheduling race.
    #[cfg(any(test, feature = "test-instrumentation"))]
    if let Some(release) = gate.exit_hold.lock().await.take() {
        gate.exit_entered.add_permits(1);
        let _ = release.await;
    }
}

/// Record a denied-while-unattended action as a durable, visible, resumable
/// transcript message (AC6 #5). The on-attach render + summary is 12.2c.
async fn record_blocked_action(
    conversation: &Arc<Mutex<Conversation>>,
    storage: &Arc<dyn crate::domain::ports::StoragePort>,
    tool: &str,
    history_source: crate::domain::models::HistorySource,
) {
    let mut conv = conversation.lock().await;
    conv.messages.push(ChatMessage {
        id: generate_message_id(),
        role: MessageRole::System,
        content: format!(
            "\u{23f8} Skipped: {tool} needed approval — no one was attached. Re-run or approve now."
        ),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: crate::domain::models::session_meta::now_unix(),
        token_count: None,
        stop_reason: None,
        synthetic: true,
        images: vec![],
        origin: ChannelKind::Terminal,
        authorship: crate::domain::models::Authorship::HumanWritten,
        send_authorization: crate::domain::models::SendAuthorization::Unknown,
        retracted_at_ms: None,
        history_source,
    });
    if let Err(e) = storage.save_conversation(&conv).await {
        tracing::warn!(error = %e, "daemon: persisting blocked-action record failed");
    }
}

/// The owned slice of `AttachServer` an inbound peer turn needs after `start`
/// has returned.
///
/// `AttachServer` is held as `Arc<Self>` by its own callers but the port's
/// `start` takes `&self`, and the turn must outlive the call. Cloning the
/// handles it genuinely uses is honest about the coupling; a `Weak<Self>`
/// upgrade would hide it and add a failure mode nobody handles.
struct InboundTurnContext {
    core: Arc<DaemonCore>,
    conversation: Arc<Mutex<Conversation>>,
    peer_row_serial: Arc<Mutex<()>>,
    domain_tx: mpsc::UnboundedSender<AppEvent>,
    node_tree: crate::infrastructure::subagent::NodeTree,
    /// The daemon has ONE conversation; turns must not interleave on it.
    turn_serial: Arc<Mutex<()>>,
    active_channel_origin: Arc<Mutex<ChannelKind>>,
    inbound_results:
        Arc<Mutex<std::collections::HashMap<AgentId, crate::domain::ports::StagedPeerResult>>>,
    pending_drafts: Arc<super::response_modes::PendingDraftController>,
    peer_reply_serial: Arc<Mutex<()>>,
    registry: Arc<Mutex<ConnRegistry>>,
    blocked_waiting: Arc<AtomicUsize>,
    approval_gate: Arc<ApprovalGate>,
    pending_consent_cards:
        Arc<Mutex<std::collections::HashMap<crate::domain::models::RequestId, DaemonFrame>>>,
    peer_turn_closed: Arc<Notify>,
}

/// Tee-collector shared by the A2A front door (`InboundTurnContext::run`) and
/// the RAP delivery path. The assistant answer is committed to the shared
/// `conversation` by the attach forwarder, which only runs when a client is
/// attached — so reading the conversation afterwards would make "did the remote
/// peer get its answer?" depend on whether an operator happened to be watching.
/// Accumulating from the turn's own `ProviderChunk` stream does not.
///
/// `run_turn`'s join handle resolves `Ok` whether the turn succeeded or died,
/// because failure is reported on the event stream rather than by unwinding.
/// This is therefore the ONLY place "the turn finished" and "the turn worked"
/// are different questions — and there are three ways it can have not worked:
/// (1) an in-stream error chunk (tool loop limit, stream disconnect), (2) a
/// provider call that never produced a stream at all (`run_turn` emits an error
/// notice and returns), and (3) the catch-all: a turn that never reached a
/// final `TurnComplete` did not complete, whatever else it did or did not say.
///
/// Managed response text stays local until the operator resolves the draft.
/// Permitted tool display and terminal cleanup carry host-owned peer lineage.
fn spawn_turn_collector(
    mut tap_rx: mpsc::UnboundedReceiver<AppEvent>,
    downstream: mpsc::UnboundedSender<AppEvent>,
    buffer_response: bool,
    node: Arc<AgentId>,
    conversation_id: String,
) -> tokio::task::JoinHandle<(String, bool, bool)> {
    tokio::spawn(async move {
        let mut answer = String::new();
        let mut errored = false;
        let mut completed = false;
        while let Some(event) = tap_rx.recv().await {
            match &event {
                AppEvent::ProviderChunk {
                    chunk: StreamChunk::Error { .. },
                    ..
                } => errored = true,
                AppEvent::SystemNotice {
                    level: crate::domain::models::NoticeLevel::Error,
                    ..
                } => errored = true,
                AppEvent::ProviderChunk {
                    chunk: StreamChunk::TurnComplete { stop_reason },
                    ..
                } if *stop_reason != StopReason::ToolUse => completed = true,
                AppEvent::ProviderChunk {
                    chunk: StreamChunk::Text { content, .. },
                    ..
                } => answer.push_str(content),
                _ => {}
            }
            match event {
                AppEvent::ProviderChunk {
                    conversation_id,
                    chunk,
                } => {
                    if !buffer_response || !matches!(chunk, StreamChunk::Text { .. }) {
                        let _ = downstream.send(AppEvent::PeerProviderChunk {
                            node: node.clone(),
                            conversation_id,
                            chunk,
                        });
                    }
                }
                AppEvent::SystemNotice {
                    conversation_id,
                    level,
                    message,
                } => {
                    let _ = downstream.send(AppEvent::PeerSystemNotice {
                        node: node.clone(),
                        conversation_id,
                        level,
                        message,
                    });
                }
                other => {
                    let _ = downstream.send(other);
                }
            }
        }
        let _ = downstream.send(AppEvent::PeerTurnClosed {
            node,
            conversation_id,
        });
        (answer, errored, completed)
    })
}

impl InboundTurnContext {
    async fn ensure_approval_gate(&self, approval: Arc<ApprovalRuntime>) {
        ensure_approval_gate_once(
            approval,
            self.registry.clone(),
            self.blocked_waiting.clone(),
            self.conversation.clone(),
            self.core.storage.clone(),
            self.domain_tx.clone(),
            self.pending_consent_cards.clone(),
            self.approval_gate.clone(),
        )
        .await;
    }

    async fn cleanup_pending_response(&self, node_id: &AgentId) {
        self.pending_drafts.abandon(node_id.as_str()).await;
        if let Err(error) = remove_pending_peer_row(
            &self.peer_row_serial,
            &self.conversation,
            &self.registry,
            self.core.storage.as_ref(),
            node_id,
        )
        .await
        {
            tracing::warn!(%error, node = %node_id, "removing failed peer response placeholder failed");
        }
    }

    /// Drive one inbound peer task to a terminal node state.
    ///
    /// This is the same origination primitive the Unix socket drives —
    /// `drive_preloaded_turn` with `TurnOrigin::RemotePeer` — not a parallel
    /// injection path. The only additions are the ones the front door owes the
    /// node: a terminal transition that distinguishes cancel from failure, and
    /// capture of the answer this task produced.
    async fn run(
        self,
        node_id: AgentId,
        principal: PolicyPrincipal,
        cancel: CancellationToken,
        response_policy: crate::domain::ports::PeerResponsePolicy,
    ) {
        // Fail closed: only an explicit wait/auto policy takes another path;
        // draft and every future variant buffer until operator resolution.
        let managed_response = !matches!(
            response_policy.mode,
            crate::domain::models::ResponseMode::NotifyAndWait
                | crate::domain::models::ResponseMode::NotifyAndAuto
        );
        let response_row_id = format!("peer-response-{}", node_id.as_str());
        let runtime = match self.core.ensure_runtime().await {
            Ok(runtime) => runtime,
            Err(error) => {
                tracing::error!(%error, node = %node_id, "A2A inbound turn: runtime unavailable");
                if managed_response {
                    self.cleanup_pending_response(&node_id).await;
                }
                self.node_tree.set_state(&node_id, NodeState::Running).await;
                self.node_tree.set_state(&node_id, NodeState::Failed).await;
                self.node_tree.deregister(&node_id).await;
                return;
            }
        };
        self.ensure_approval_gate(runtime.approval.clone()).await;

        // Serialize on the same mutex every other daemon turn uses. Taken AFTER
        // registration so the node — and therefore `tasks/get` — is live and
        // reports `working` while the task waits its turn, rather than the
        // submitter seeing nothing until the queue drains.
        let _turn_guard = tokio::select! {
            guard = self.turn_serial.lock() => guard,
            () = cancel.cancelled() => {
                if managed_response {
                    self.cleanup_pending_response(&node_id).await;
                }
                self.node_tree.set_state(&node_id, NodeState::Running).await;
                self.node_tree
                    .set_state(&node_id, NodeState::Cancelled)
                    .await;
                self.node_tree.deregister(&node_id).await;
                return;
            }
        };
        if cancel.is_cancelled() {
            if managed_response {
                self.cleanup_pending_response(&node_id).await;
            }
            self.node_tree.set_state(&node_id, NodeState::Running).await;
            self.node_tree
                .set_state(&node_id, NodeState::Cancelled)
                .await;
            self.node_tree.deregister(&node_id).await;
            return;
        }
        *self.active_channel_origin.lock().await = ChannelKind::Terminal;

        self.node_tree.set_state(&node_id, NodeState::Running).await;
        let peer_turn_closed = self.peer_turn_closed.notified();
        tokio::pin!(peer_turn_closed);
        peer_turn_closed.as_mut().enable();

        // Tee the turn's event stream (see `spawn_turn_collector`). Every
        // non-buffered event is forwarded on to the real `domain_tx`, so the
        // daemon bus, the forwarder, and every other consumer see exactly what
        // they saw before.
        let conversation_id = self.conversation.lock().await.id.clone();
        let (tap_tx, tap_rx) = mpsc::unbounded_channel::<AppEvent>();
        let peer_node = Arc::new(node_id.clone());
        let collector = spawn_turn_collector(
            tap_rx,
            self.domain_tx.clone(),
            managed_response,
            peer_node.clone(),
            conversation_id.clone(),
        );

        let mut handle = {
            let mut conversation = self.conversation.lock().await;
            runtime.drive_preloaded_turn(
                &mut conversation,
                &tap_tx,
                TurnOrigin::RemotePeer { principal },
                cancel.clone(),
            )
        };
        drop(tap_tx);

        // `run_turn`'s cancellation token reaches tool execution but NOT a
        // provider stream that never yields, so awaiting the handle alone would
        // make `tasks/cancel` take effect only once the model replied. Racing the
        // token here — at the layer that owns the join handle — is what makes the
        // cancel actually prompt.
        let joined = tokio::select! {
            outcome = &mut handle => Some(outcome.is_err()),
            () = cancel.cancelled() => {
                handle.abort();
                let _ = handle.await;
                None
            }
        };
        let collector_result = match collector.await {
            Ok(result) => result,
            Err(error) => {
                tracing::warn!(?error, node = %node_id, "A2A inbound turn collector failed");
                (String::new(), true, false)
            }
        };
        let (answer, errored, completed) = collector_result;
        peer_turn_closed.await;
        let _reply_guard = self.peer_reply_serial.lock().await;
        let _row_guard = self.peer_row_serial.lock().await;

        let terminal = match joined {
            None => {
                if !completed {
                    // The abort drops `run_turn` before it can emit a terminal
                    // chunk. Drain its tee first, then enqueue a cancelled
                    // terminal event after every partial chunk. The forwarder
                    // treats that event as a buffer reset, never an answer.
                    // The reset must travel the daemon's own forwarder channel in stream order;
                    // InboundTurnContext holds no event_bus handle, and every other chunk of this
                    // turn tees through the same tx.
                    let reset = AppEvent::PeerProviderChunk {
                        node: peer_node.clone(),
                        conversation_id,
                        chunk: StreamChunk::TurnComplete {
                            stop_reason: StopReason::Cancelled,
                        },
                    };
                    let _ = self.domain_tx.send(reset); // CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS: 18-1b AC6b — ordered cancel reset via the daemon forwarder channel
                }
                NodeState::Cancelled
            }
            Some(join_failed) => {
                let terminal = if join_failed || errored || !completed {
                    // An unexpected join/runtime failure can leave streamed text
                    // without a terminal chunk too. Reset it through the same
                    // ordered path rather than allowing the next turn to own it.
                    if !completed {
                        // Same ordered-reset channel as the cancel path above; the forwarder
                        // consumes domain_tx.
                        let reset = AppEvent::PeerProviderChunk {
                            node: peer_node.clone(),
                            conversation_id,
                            chunk: StreamChunk::TurnComplete {
                                stop_reason: StopReason::Cancelled,
                            },
                        };
                        let _ = self.domain_tx.send(reset); // CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS: 18-1b AC6b — ordered cancel reset via the daemon forwarder channel
                    }
                    NodeState::Failed
                } else if managed_response && !answer.trim().is_empty() {
                    if !self
                        .pending_drafts
                        .complete(node_id.as_str(), answer.clone())
                        .await
                    {
                        let settled = {
                            let conversation = self.conversation.lock().await;
                            conversation
                                .peer_row_controls
                                .get(&response_row_id)
                                .is_some_and(|control| control.stage == PeerRowStage::Settled)
                        };
                        if settled {
                            return;
                        }
                        NodeState::Failed
                    } else {
                        let (row, control) = {
                            let conversation = self.conversation.lock().await;
                            (
                                conversation
                                    .messages
                                    .iter()
                                    .find(|message| message.id == response_row_id)
                                    .cloned(),
                                conversation
                                    .peer_row_controls
                                    .get(&response_row_id)
                                    .cloned(),
                            )
                        };
                        match (row, control) {
                            (Some(mut row), Some(mut control))
                                if control.stage == PeerRowStage::Drafting =>
                            {
                                row.content = format!(
                                    "{}{}",
                                    super::response_modes::DRAFT_APPROVAL_PREFIX,
                                    answer
                                );
                                control.stage = PeerRowStage::Ready;
                                control.node_resolvable = true;
                                match commit_peer_rows_serialized(
                                    &self.conversation,
                                    &self.registry,
                                    self.core.storage.as_ref(),
                                    vec![PeerRowMutation::Upsert {
                                        message: row,
                                        control: Some(control),
                                    }],
                                )
                                .await
                                {
                                    Ok(()) => NodeState::Waiting,
                                    Err(error) => {
                                        tracing::warn!(%error, "persisting completed peer draft failed");
                                        self.pending_drafts.abandon(node_id.as_str()).await;
                                        NodeState::Failed
                                    }
                                }
                            }
                            (_, Some(control)) if control.stage == PeerRowStage::Settled => return,
                            _ => {
                                self.pending_drafts.abandon(node_id.as_str()).await;
                                NodeState::Failed
                            }
                        }
                    }
                } else if managed_response {
                    NodeState::Failed
                } else {
                    if !answer.trim().is_empty() {
                        self.inbound_results.lock().await.insert(
                            node_id.clone(),
                            crate::domain::ports::StagedPeerResult {
                                text: answer,
                                authorship: crate::domain::models::Authorship::AgentComposed,
                                send_authorization:
                                    crate::domain::models::SendAuthorization::Automatic,
                            },
                        );
                    }
                    NodeState::Completed
                };

                terminal
            }
        };
        if managed_response && !matches!(terminal, NodeState::Completed | NodeState::Waiting) {
            self.pending_drafts.abandon(node_id.as_str()).await;
            if let Err(error) = remove_pending_peer_row_serialized(
                &self.conversation,
                &self.registry,
                self.core.storage.as_ref(),
                &node_id,
            )
            .await
            {
                tracing::warn!(%error, "removing failed peer response placeholder failed");
            }
        }
        self.node_tree.set_state(&node_id, terminal).await;
        if terminal == NodeState::Waiting {
            if let Err(error) = self
                .node_tree
                .stamp_wait_reason(
                    &node_id,
                    Some(crate::domain::models::WaitReason::AwaitingPeerResponse),
                )
                .await
            {
                tracing::error!(%error, node = %node_id, "stamping peer draft wait failed");
                self.node_tree.set_state(&node_id, NodeState::Failed).await;
                self.pending_drafts.abandon(node_id.as_str()).await;
                if let Err(remove_error) = remove_pending_peer_row_serialized(
                    &self.conversation,
                    &self.registry,
                    self.core.storage.as_ref(),
                    &node_id,
                )
                .await
                {
                    tracing::warn!(%remove_error, node = %node_id, "removing failed peer response placeholder failed");
                }
                self.node_tree.deregister(&node_id).await;
            }
            return;
        }
        self.node_tree.deregister(&node_id).await;
    }
}

fn disclosure_forbidden_fragments(system_prompt: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    system_prompt
        .lines()
        .map(str::trim)
        .filter(|fragment| fragment.chars().count() >= 32)
        .filter(|fragment| seen.insert(*fragment))
        .map(str::to_owned)
        .collect()
}

/// Story 18.1b — the daemon IS the inbound-peer execution core.
///
/// Implemented on `AttachServer` rather than on a new type because everything
/// the seam needs is already here and already coordinated: the shared
/// `NodeTree`, the one `DaemonCore`, the single `conversation`, the
/// `turn_serial` mutex that keeps turns from interleaving on it, and the
/// approval gate. A sibling struct would need clones of all five and a second
/// serialization discipline to keep them consistent — a second core in
/// everything but name.
#[async_trait::async_trait]
impl crate::domain::ports::InboundPeerRuntime for AttachServer {
    fn response_policy(
        &self,
        principal: &PolicyPrincipal,
        message_type: crate::domain::models::SemanticMessageType,
    ) -> crate::domain::ports::PeerResponsePolicy {
        self.delivery_policy
            .response_policy_for_peer(principal, message_type)
    }

    async fn park_pending_consent(
        &self,
        node_id: &AgentId,
        subagent_type: &str,
        cancel: CancellationToken,
    ) -> Result<(), crate::domain::ports::InboundPeerError> {
        use crate::domain::models::{AgentMetrics, CapabilityTokenId};
        use crate::domain::ports::InboundPeerError;
        use crate::infrastructure::subagent::{AgentHandle, MailboxBudget};

        if self.pending_peer_nodes.lock().await.contains_key(node_id) {
            return Ok(());
        }
        let mailbox_budget = MailboxBudget::new();
        let (command_tx, command_rx) = mpsc::channel(1);
        let (status_tx, _) = tokio::sync::watch::channel(NodeState::Created);
        let (_, metrics_rx) = tokio::sync::watch::channel(AgentMetrics::default());
        self.node_tree
            .register_peer(
                node_id.clone(),
                AgentHandle {
                    agent_id: node_id.clone(),
                    token: CapabilityTokenId::nil(),
                    command_tx,
                    cancel_token: cancel,
                    depth: 0,
                    subagent_type: subagent_type.to_owned(),
                    spawned_at: self.clock.wall_now_ms(),
                    status: status_tx,
                    metrics: metrics_rx,
                    isolated: false,
                    mailbox_budget: mailbox_budget.clone(),
                },
            )
            .await
            .map_err(|error| InboundPeerError::Register(error.to_string()))?;
        if let Err(error) = self
            .node_tree
            .try_set_state(node_id, NodeState::Running)
            .await
        {
            self.node_tree.deregister(node_id).await;
            return Err(InboundPeerError::Register(error.to_string()));
        }
        if let Err(error) = self
            .node_tree
            .try_set_state(node_id, NodeState::Waiting)
            .await
        {
            self.node_tree.deregister(node_id).await;
            return Err(InboundPeerError::Register(error.to_string()));
        }
        if let Err(error) = self
            .node_tree
            .stamp_wait_reason(
                node_id,
                Some(crate::domain::models::WaitReason::AwaitingPeerResponse),
            )
            .await
        {
            self.node_tree.deregister(node_id).await;
            return Err(InboundPeerError::Register(error.to_string()));
        }
        self.pending_peer_nodes.lock().await.insert(
            node_id.clone(),
            PreparedPeerNode {
                command_rx,
                mailbox_budget,
            },
        );
        Ok(())
    }

    async fn discard_pending_consent_node(&self, node_id: &AgentId) {
        if self
            .pending_peer_nodes
            .lock()
            .await
            .remove(node_id)
            .is_some()
        {
            self.node_tree.deregister(node_id).await;
        }
    }

    async fn start(
        &self,
        task: crate::domain::ports::InboundPeerTask,
        cancel: CancellationToken,
    ) -> Result<tokio::sync::watch::Receiver<NodeState>, crate::domain::ports::InboundPeerError>
    {
        use crate::domain::models::{AgentMetrics, CapabilityTokenId, Op};
        use crate::domain::ports::InboundPeerError;
        use crate::infrastructure::subagent::{AgentHandle, MailboxBudget};

        // Consent-pending tasks already own a registered `Waiting` peer node.
        // Accepted tasks consume its command receiver; ordinary trusted tasks
        // take the original fresh-registration path.
        let prepared = self.pending_peer_nodes.lock().await.remove(&task.node_id);
        let (mailbox_budget, mut command_rx) = if let Some(prepared) = prepared {
            (prepared.mailbox_budget, prepared.command_rx)
        } else {
            let mailbox_budget = MailboxBudget::new();
            let (command_tx, command_rx) = mpsc::channel(1);
            let (status_tx, _) = tokio::sync::watch::channel(NodeState::Created);
            let (_, metrics_rx) = tokio::sync::watch::channel(AgentMetrics::default());
            self.node_tree
                .register_peer(
                    task.node_id.clone(),
                    AgentHandle {
                        agent_id: task.node_id.clone(),
                        token: CapabilityTokenId::nil(),
                        command_tx,
                        cancel_token: cancel.clone(),
                        depth: 0,
                        subagent_type: task.subagent_type.clone(),
                        spawned_at: self.clock.wall_now_ms(),
                        status: status_tx,
                        metrics: metrics_rx,
                        isolated: false,
                        mailbox_budget: mailbox_budget.clone(),
                    },
                )
                .await
                .map_err(|error| InboundPeerError::Register(error.to_string()))?;
            (mailbox_budget, command_rx)
        };
        let status = self
            .node_tree
            .status_rx(&task.node_id)
            .await
            .ok_or_else(|| {
                InboundPeerError::Register("registered node has no status channel".to_owned())
            })?;

        // `Op::Kill` (cascade kill, teardown) must reach the running turn, and
        // the only thing the turn selects on is this token.
        //
        // Story 18.3 (AC2) — the loop used to be `if matches!(op, Op::Kill)`, so
        // every other op, INCLUDING `Op::Deliver`, was received and silently
        // dropped. `LocalMessageBus` had already reserved a `MailboxBudget` slot
        // for it and nothing released it: a permanent leak against
        // `MAILBOX_CAP = 64` plus a receipt the sender never got, violating
        // 14-4a's INV-DEL-2 (Σ outcomes == Σ sent, zero unaccounted).
        //
        // Not reachable today — nothing delivers to these ids — but 18-3b routes
        // peer responses through the bus and makes it reachable, so it is fixed
        // before it can bite rather than deferred (Rule 3: latent-but-reachable).
        {
            let cancel = cancel.clone();
            let mailbox_budget = mailbox_budget.clone();
            let domain_tx = self.domain_tx.clone();
            let node_id = task.node_id.clone();
            tokio::spawn(async move {
                let settle =
                    |delivery: crate::domain::models::AgentDelivery,
                     reason: crate::domain::models::RefuseReason| {
                        mailbox_budget.release();
                        let receipt = crate::domain::models::refusal_receipt(
                            &delivery.envelope.header,
                            &node_id,
                            reason,
                        );
                        let _ = domain_tx.send(receipt); // CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS: 18-3 AC2 — the receipt that stops an Op::Deliver being received-and-dropped; this spawned loop holds a domain_tx, not an EventBus
                    };
                while let Some(op) = command_rx.recv().await {
                    match op {
                        Op::Kill => {
                            cancel.cancel();
                            break;
                        }
                        Op::Deliver(delivery) => {
                            let reason = inbound_peer_refuse_reason(delivery.disposition, false);
                            settle(delivery, reason);
                        }
                        _ => {}
                    }
                }
                // Terminal drain: a delivery that raced the kill still holds a
                // reservation, so account for it too rather than leaking on exit.
                command_rx.close();
                while let Ok(op) = command_rx.try_recv() {
                    if let Op::Deliver(delivery) = op {
                        let reason = inbound_peer_refuse_reason(delivery.disposition, true);
                        settle(delivery, reason);
                    }
                }
            });
        }

        // Every mode branch below runs AFTER registration and the command-loop
        // spawn, so an early `return Err` here would orphan a live node nobody
        // can reach. Route all fallible work through one block and unwind both
        // on failure.
        enum StartRouting {
            Settled,
            DriveDraftTurn,
        }
        let fallible = async {
            let response_started_at_ms = self.clock.wall_now_ms();
            let surface_now =
                if let Some(router) = &self.urgency_router {
                    let route = router
                        .route(super::urgency::SurfaceInteraction {
                            peer: task.principal.peer().clone(),
                            node: task.node_id.clone(),
                            task: Some(task.node_id.as_str().to_owned()),
                            text: task.text.clone(),
                            notification: task.response_policy.notification,
                            provenance: task.response_policy.provenance.clone(),
                            recorded_at_ms: response_started_at_ms,
                        })
                        .await
                        .map_err(|error| InboundPeerError::Unavailable(error.to_string()))?;
                    match route {
                        super::urgency::UrgencyRoute::Immediate(_) => true,
                        super::urgency::UrgencyRoute::Queued => {
                            if !self.operator_turn_active.load(Ordering::Acquire) {
                                let queued = router.idle_queue_snapshot().await;
                                let count = queued.len();
                                self.surface_queued_interactions(queued).await.map_err(
                                    |error| InboundPeerError::Unavailable(error.to_string()),
                                )?;
                                router.drain_idle_queue(count).await;
                            }
                            false
                        }
                        super::urgency::UrgencyRoute::Digested => false,
                    }
                } else {
                    true
                };
            let (item_address, item_alias) = self
                .item_context_for_node(&task.node_id)
                .await
                .map_or((None, None), |(address, alias)| (Some(address), alias));
            // The durable ingress alias is display-only; policy authority stays on the principal.
            let sender_label = item_alias
                .as_deref()
                .unwrap_or(task.principal.peer().as_str());
            match task.response_policy.mode {
                crate::domain::models::ResponseMode::NotifyAndWait => {
                    let row_guard = self.peer_row_serial.lock().await;
                    if !self.pending_drafts.begin(task.node_id.as_str()).await {
                        return Err(InboundPeerError::Register(
                            "pending peer response already exists for inbound node".to_owned(),
                        ));
                    }
                    if !self
                        .pending_drafts
                        .complete(task.node_id.as_str(), String::new())
                        .await
                    {
                        self.pending_drafts.abandon(task.node_id.as_str()).await;
                        return Err(InboundPeerError::Register(
                            "pending peer response already exists for inbound node".to_owned(),
                        ));
                    }
                    let mut mutations = Vec::with_capacity(2);
                    if surface_now {
                        mutations.push(PeerRowMutation::Upsert {
                            message: peer_transcript_message(
                                &task.text,
                                task.principal.peer(),
                                &task.response_policy.provenance,
                            ),
                            control: None,
                        });
                    }
                    mutations.push(PeerRowMutation::Upsert {
                        message: ChatMessage {
                            id: format!("peer-response-{}", task.node_id.as_str()),
                            role: MessageRole::Assistant,
                            content: super::response_modes::AWAITING_RESPONSE_PLACEHOLDER
                                .to_owned(),
                            created_at: crate::domain::models::session_meta::now_unix(),
                            synthetic: true,
                            ..Default::default()
                        },
                        control: Some(peer_row_control(
                            &task.node_id,
                            item_address.clone(),
                            &task.response_policy.provenance,
                            sender_label,
                            PeerRowStage::AwaitingOperator,
                            true,
                        )),
                    });
                    if let Err(error) = commit_peer_rows_serialized(
                        &self.conversation,
                        &self.registry,
                        self.core.storage.as_ref(),
                        mutations,
                    )
                    .await
                    {
                        self.pending_drafts.abandon(task.node_id.as_str()).await;
                        return Err(InboundPeerError::Unavailable(error));
                    }
                    self.node_tree
                        .set_state(&task.node_id, NodeState::Running)
                        .await;
                    self.node_tree
                        .set_state(&task.node_id, NodeState::Waiting)
                        .await;
                    if let Err(error) = self
                        .node_tree
                        .stamp_wait_reason(
                            &task.node_id,
                            Some(crate::domain::models::WaitReason::AwaitingPeerResponse),
                        )
                        .await
                    {
                        self.pending_drafts.abandon(task.node_id.as_str()).await;
                        if let Err(remove_error) = remove_pending_peer_row_serialized(
                            &self.conversation,
                            &self.registry,
                            self.core.storage.as_ref(),
                            &task.node_id,
                        )
                        .await
                        {
                            tracing::warn!(%remove_error, node = %task.node_id, "removing failed peer response placeholder failed");
                        }
                        return Err(InboundPeerError::Register(error.to_string()));
                    }
                    drop(row_guard);
                    Ok(StartRouting::Settled)
                }
                crate::domain::models::ResponseMode::NotifyAndAuto => {
                    // AC4 — a conservative template, never inference. The row
                    // is persisted (and thereby visible) BEFORE the deadline is
                    // read, and it carries a real id so the retract path can
                    // address it (AC5).
                    let response = super::response_modes::auto_response_template(
                        task.response_policy.auto_response.as_ref(),
                    );
                    let mut mutations = Vec::with_capacity(2);
                    if surface_now {
                        mutations.push(PeerRowMutation::Upsert {
                            message: peer_transcript_message(
                                &task.text,
                                task.principal.peer(),
                                &task.response_policy.provenance,
                            ),
                            control: None,
                        });
                    }
                    mutations.push(PeerRowMutation::Upsert {
                        message: ChatMessage {
                            id: format!("peer-response-{}", task.node_id.as_str()),
                            role: MessageRole::Assistant,
                            content: match item_address.as_ref() {
                                Some(address) => format!(
                                    "{response}\nOpen: /team view {}",
                                    address.item().as_str()
                                ),
                                None => response.clone(),
                            },
                            created_at: crate::domain::models::session_meta::now_unix(),
                            authorship: crate::domain::models::Authorship::AgentComposed,
                            send_authorization: crate::domain::models::SendAuthorization::Automatic,
                            ..Default::default()
                        },
                        control: Some(peer_row_control(
                            &task.node_id,
                            item_address.clone(),
                            &task.response_policy.provenance,
                            sender_label,
                            PeerRowStage::Settled,
                            false,
                        )),
                    });
                    commit_peer_rows(
                        &self.peer_row_serial,
                        &self.conversation,
                        &self.registry,
                        self.core.storage.as_ref(),
                        mutations,
                    )
                    .await
                    .map_err(InboundPeerError::Unavailable)?;
                    super::response_modes::auto_response_surface(
                        self.clock.as_ref(),
                        response_started_at_ms,
                        true,
                        false,
                    )
                    .map_err(|miss| {
                        InboundPeerError::Unavailable(format!(
                            "auto response surface missed deadline by {}ms",
                            miss.elapsed_ms
                        ))
                    })?;
                    self.inbound_results.lock().await.insert(
                        task.node_id.clone(),
                        crate::domain::ports::StagedPeerResult {
                            text: response,
                            authorship: crate::domain::models::Authorship::AgentComposed,
                            send_authorization: crate::domain::models::SendAuthorization::Automatic,
                        },
                    );
                    self.node_tree
                        .set_state(&task.node_id, NodeState::Running)
                        .await;
                    self.node_tree
                        .set_state(&task.node_id, NodeState::Completed)
                        .await;
                    self.node_tree.deregister(&task.node_id).await;
                    Ok(StartRouting::Settled)
                }
                _ => {
                    // NotifyAndDraft — and any future variant, failing closed:
                    // buffer daemon-side, disclose nothing until the operator
                    // resolves the draft (AC3).
                    let row_guard = self.peer_row_serial.lock().await;
                    if !self.pending_drafts.begin(task.node_id.as_str()).await {
                        return Err(InboundPeerError::Register(
                            "pending draft already exists for inbound node".to_owned(),
                        ));
                    }
                    let mut mutations = Vec::with_capacity(2);
                    if surface_now {
                        mutations.push(PeerRowMutation::Upsert {
                            message: peer_transcript_message(
                                &task.text,
                                task.principal.peer(),
                                &task.response_policy.provenance,
                            ),
                            control: None,
                        });
                    }
                    mutations.push(PeerRowMutation::Upsert {
                        message: ChatMessage {
                            id: format!("peer-response-{}", task.node_id.as_str()),
                            role: MessageRole::Assistant,
                            content: super::response_modes::DRAFTING_PLACEHOLDER.to_owned(),
                            created_at: crate::domain::models::session_meta::now_unix(),
                            synthetic: true,
                            ..Default::default()
                        },
                        control: Some(peer_row_control(
                            &task.node_id,
                            item_address,
                            &task.response_policy.provenance,
                            sender_label,
                            PeerRowStage::Drafting,
                            true,
                        )),
                    });
                    if let Err(error) = commit_peer_rows_serialized(
                        &self.conversation,
                        &self.registry,
                        self.core.storage.as_ref(),
                        mutations,
                    )
                    .await
                    {
                        self.pending_drafts.abandon(task.node_id.as_str()).await;
                        return Err(InboundPeerError::Unavailable(error));
                    }
                    drop(row_guard);
                    Ok(StartRouting::DriveDraftTurn)
                }
            }
        };
        match fallible.await {
            Ok(StartRouting::Settled) => return Ok(status),
            Ok(StartRouting::DriveDraftTurn) => {}
            Err(error) => {
                cancel.cancel();
                self.node_tree.deregister(&task.node_id).await;
                return Err(error);
            }
        }

        let context = InboundTurnContext {
            core: self.core.clone(),
            conversation: self.conversation.clone(),
            peer_row_serial: self.peer_row_serial.clone(),
            domain_tx: self.domain_tx.clone(),
            node_tree: self.node_tree.clone(),
            registry: self.registry.clone(),
            blocked_waiting: self.blocked_waiting.clone(),
            approval_gate: self.approval_gate.clone(),
            pending_consent_cards: self.pending_consent_cards.clone(),
            turn_serial: self.turn_serial.clone(),
            peer_turn_closed: self.peer_turn_closed.clone(),
            active_channel_origin: self.active_channel_origin.clone(),
            inbound_results: self.inbound_results.clone(),
            pending_drafts: self.pending_drafts.clone(),
            peer_reply_serial: self.peer_reply_serial.clone(),
        };
        tokio::spawn(context.run(
            task.node_id.clone(),
            task.principal,
            cancel,
            task.response_policy,
        ));

        Ok(status)
    }

    fn enforces_sender_consent(&self) -> bool {
        self.pending_consent.is_some()
    }

    async fn request_admission_approval(
        &self,
        principal: &PolicyPrincipal,
        summary: &str,
    ) -> Result<crate::domain::ports::InboundApprovalTicket, crate::domain::ports::InboundPeerError>
    {
        use crate::domain::models::ApprovalOutcome;
        use crate::domain::ports::{
            InboundApprovalDecision, InboundApprovalTicket, InboundPeerError,
        };

        if self.approval_gate.shutdown.is_cancelled() {
            let (decision_tx, decision) = tokio::sync::oneshot::channel();
            let _ = decision_tx.send(InboundApprovalDecision::RuntimeShutdown);
            return Ok(InboundApprovalTicket {
                pending: false,
                decision,
            });
        }

        let approval_shutdown = self.approval_gate.shutdown.clone();
        let resolve_outcome = move |outcome: Result<
            crate::domain::services::approval_runtime::ResolvedApproval,
            tokio::sync::oneshot::error::RecvError,
        >| match outcome.map(|resolved| resolved.outcome) {
            Ok(
                ApprovalOutcome::Once
                | ApprovalOutcome::AlwaysTool { .. }
                | ApprovalOutcome::AlwaysServer { .. }
                | ApprovalOutcome::AlwaysAndSave { .. },
            ) => InboundApprovalDecision::AllowOnce,
            Ok(ApprovalOutcome::Cancel) if approval_shutdown.is_cancelled() => {
                InboundApprovalDecision::RuntimeShutdown
            }
            _ => InboundApprovalDecision::Decline,
        };

        if let Some(manager) = &self.pending_consent {
            let registration = manager
                .register(
                    principal.clone(),
                    summary,
                    crate::domain::models::relationship_disposition(
                        crate::domain::models::OwnershipKind::Peer,
                    ),
                )
                .await;
            let pending = registration.pending;
            let decision = registration.decision;
            if !registration.first_for_sender {
                return Ok(InboundApprovalTicket { pending, decision });
            }

            let runtime = match self.core.ensure_runtime().await {
                Ok(rt) => rt,
                Err(error) => {
                    // F9: admission setup failed before a card could render —
                    // drop the pending waiter so the next delivery retries
                    // instead of joining an orphaned group that never resolves.
                    manager.drop_pending(principal).await;
                    return Err(InboundPeerError::Unavailable(error.to_string()));
                }
            };
            self.ensure_approval_gate(runtime.approval.clone()).await;
            let conversation_id = self.conversation.lock().await.id.clone();
            let card = manager
                .card_text(principal)
                .await
                .expect("first pending sender has a consent card");
            let (request_id, resolved) = runtime
                .approval
                .request(
                    crate::domain::models::tool_call::ApprovalSource::remote_peer(
                        conversation_id,
                        principal,
                    ),
                    "a2a/sender-consent".to_owned(),
                    serde_json::json!({
                        "card": card,
                        "peer": principal.peer().as_str(),
                        "instruction": summary.chars().take(400).collect::<String>(),
                    }),
                    ToolRisk::Elevated,
                    None,
                    None,
                )
                .await;
            let manager = Arc::clone(manager);
            let principal = principal.clone();
            let consent_request_id = request_id.clone();
            tokio::spawn(async move {
                let mut decision = resolve_outcome(resolved.await);
                if let Some(request_id) = consent_request_id
                    && manager.take_granted(&request_id).await
                    && decision == InboundApprovalDecision::AllowOnce
                {
                    decision = InboundApprovalDecision::AllowAlways;
                }
                manager.resolve(&principal, decision).await;
            });
            return Ok(InboundApprovalTicket {
                pending: request_id.is_some(),
                decision,
            });
        }

        let runtime = self
            .core
            .ensure_runtime()
            .await
            .map_err(|error| InboundPeerError::Unavailable(error.to_string()))?;
        self.ensure_approval_gate(runtime.approval.clone()).await;
        let conversation_id = self.conversation.lock().await.id.clone();
        let (request_id, resolved) = runtime
            .approval
            .request(
                crate::domain::models::tool_call::ApprovalSource::remote_peer(
                    conversation_id,
                    principal,
                ),
                "a2a/message.send".to_owned(),
                serde_json::json!({
                    "peer": principal.peer().as_str(),
                    "instruction": summary.chars().take(400).collect::<String>(),
                }),
                ToolRisk::Elevated,
                None,
                None,
            )
            .await;
        let (decision_tx, decision) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _ = decision_tx.send(resolve_outcome(resolved.await));
        });
        Ok(InboundApprovalTicket {
            pending: request_id.is_some(),
            decision,
        })
    }

    async fn take_result(
        &self,
        node_id: &AgentId,
    ) -> Option<crate::domain::ports::StagedPeerResult> {
        self.inbound_results.lock().await.remove(node_id)
    }

    async fn disclosure_forbidden_fragments(&self) -> Vec<String> {
        let system_prompt = crate::domain::ports::PersonaPort::system_prompt(
            self.core.persona.as_ref(),
            &self.core.workspace,
        );
        disclosure_forbidden_fragments(&system_prompt)
    }

    async fn reconcile_orphaned_tasks(&self, subagent_type: &str) -> Vec<AgentId> {
        // A node left non-terminal by a previous process was rebuilt from the
        // journal by `NodeRecovery::reconcile` (as `Suspended`) or never torn
        // down. Either way nothing is driving it any more, so it is failed here
        // and its id handed back for an honest wire answer.
        let orphans: Vec<AgentId> = self
            .node_tree
            .list()
            .await
            .into_iter()
            .filter(|entry| {
                entry.subagent_type == subagent_type && !entry.current_status.is_terminal()
            })
            .map(|entry| entry.agent_id)
            .collect();
        for node_id in &orphans {
            // `Suspended -> Failed` is not an edge in the node FSM (a suspended
            // node resumes or is cancelled), so route through `Running`. Driving
            // the shipped table rather than widening it keeps the FSM the single
            // description of what a node may do.
            let _ = self
                .node_tree
                .try_set_state(node_id, NodeState::Running)
                .await;
            if let Err(error) = self
                .node_tree
                .try_set_state(node_id, NodeState::Failed)
                .await
            {
                tracing::warn!(%error, node = %node_id, "could not fail an orphaned inbound task");
            }
        }
        orphans
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::daemon::runtime::{DaemonCore, DaemonTurnRuntime};
    use crate::adapters::daemon::session_queue;
    use crate::adapters::filesystem::FileSystemStorage;
    use crate::adapters::noop::{
        NoOpApprovalPersistence, NoOpMemory, NoOpPersona, NoOpSecurity, NoOpToolSet,
        NoOpUsageLedger,
    };
    use crate::adapters::rap::AgentSigner;
    use crate::domain::errors::ProviderError;
    use crate::domain::models::{AgentEnvelope, ModelDescriptor};
    use crate::domain::models::{AppConfig, CompletionOptions, Message, StopReason};
    use crate::domain::ports::RoomJournal;
    use crate::domain::ports::{
        InboundPeerRuntime, InboundPeerTask, PersonaPort, SecurityPort, StoragePort,
        StreamingProvider, ToolSetPort,
    };
    use crate::infrastructure::runtime::event_bus::EventBus;
    use arc_swap::ArcSwap;
    use futures::stream::BoxStream;
    use std::path::Path;
    use tokio::net::UnixStream;

    struct ProjectionProvider {
        delegate: ToolThenTextProvider,
        input: serde_json::Value,
        tool: &'static str,
    }

    #[async_trait::async_trait]
    impl StreamingProvider for ProjectionProvider {
        async fn stream_completion(
            &self,
            messages: Vec<Message>,
            options: CompletionOptions,
        ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
            use futures::StreamExt;
            let input = self.input.clone();
            let tool = self.tool;
            Ok(self
                .delegate
                .stream_completion(messages, options)
                .await?
                .map(move |chunk| match chunk {
                    StreamChunk::ToolUse { id, .. } => StreamChunk::ToolUse {
                        id,
                        name: tool.to_owned(),
                        input: input.clone(),
                    },
                    other => other,
                })
                .boxed())
        }
        async fn abort(&self) -> Result<(), ProviderError> {
            self.delegate.abort().await
        }
        fn provider_id(&self) -> String {
            "projection-tools".to_owned()
        }
        fn list_models(&self) -> Vec<ModelDescriptor> {
            vec![]
        }
        async fn health_check(&self) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn connectivity_probe(
            &self,
        ) -> Result<crate::domain::ports::ProbeOutcome, ProviderError> {
            self.delegate.connectivity_probe().await
        }
    }

    async fn projection_socket(
        socket: &Path,
        tier: ConnectionTier,
        signer: u8,
    ) -> (UnixStream, DaemonFrame) {
        let mut stream = UnixStream::connect(socket).await.unwrap();
        let ack = attach_with_proof(&mut stream, false, tier, &test_signer(signer))
            .await
            .expect("signed attach acknowledgement");
        (stream, ack)
    }

    async fn projection_read(stream: &mut UnixStream) -> DaemonFrame {
        read_frame::<_, DaemonFrame>(stream)
            .await
            .unwrap()
            .expect("connection stays open")
    }

    async fn projection_history(stream: &mut UnixStream) -> DaemonFrame {
        write_frame(
            stream,
            &ClientFrame::HistoryRequest {
                before_index: None,
                count: usize::MAX,
            },
        )
        .await
        .unwrap();
        loop {
            let frame = projection_read(stream).await;
            if matches!(frame, DaemonFrame::History { .. }) {
                return frame;
            }
        }
    }

    fn projection_tool_core(
        workspace: &Path,
        tool: &'static str,
        input: serde_json::Value,
    ) -> Arc<DaemonCore> {
        let provider = Arc::new(ProjectionProvider {
            delegate: ToolThenTextProvider {
                calls: AtomicUsize::new(0),
            },
            input,
            tool,
        });
        let storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
            crate::infrastructure::paths::sessions_dir(workspace),
            workspace.to_owned(),
        ));
        let ws = workspace.to_owned();
        let factory_storage = storage.clone();
        Arc::new(DaemonCore::new(
            ws.clone(),
            Arc::new(ArcSwap::from_pointee(AppConfig::default())),
            Arc::new(NoOpMemory),
            storage,
            Arc::new(NoOpSecurity),
            Arc::new(NoOpPersona),
            Arc::new(crate::adapters::rap::PeerTopicStore::new()),
            Box::new(move || {
                let mut runtime =
                    Arc::try_unwrap(mock_runtime(provider.clone(), factory_storage.clone(), &ws))
                        .ok()
                        .expect("new unshared runtime");
                let tools: Arc<dyn ToolSetPort> =
                    Arc::new(crate::adapters::toolset_adapter::ToolSetAdapter::new(
                        ws.clone(),
                        runtime.fs_storage.clone(),
                        Arc::new(ArcSwap::from_pointee(Arc::new(
                            crate::adapters::sandbox::NoOpSandbox,
                        )
                            as Arc<dyn crate::domain::ports::SandboxManager>)),
                        Arc::new(tokio::sync::RwLock::new(
                            crate::domain::models::SandboxPolicy::Permissive,
                        )),
                    ));
                runtime.tool_scheduler =
                    crate::domain::services::tool_scheduler::ToolScheduler::new(
                        runtime.security.clone(),
                        tools.clone(),
                        runtime.approval.clone(),
                        64,
                    );
                runtime.tools = tools;
                Ok(Arc::new(runtime))
            }),
        ))
    }

    fn assert_projection_transcript(frame: &DaemonFrame, input: &serde_json::Value, peer: bool) {
        let (messages, entries) = match frame {
            DaemonFrame::AttachAck { snapshot, .. } => {
                (&snapshot.transcript, &snapshot.withheld_tool_inputs)
            }
            DaemonFrame::History {
                messages,
                withheld_tool_inputs,
                ..
            } => (messages, withheld_tool_inputs),
            other => panic!("expected transcript, got {other:?}"),
        };
        let calls: Vec<_> = messages
            .iter()
            .flat_map(|message| &message.tool_calls)
            .collect();
        assert!(
            !calls.is_empty(),
            "transcript positive control has a real invocation"
        );
        for call in calls {
            if peer {
                assert!(call.input.is_null());
                assert!(entries.iter().any(|entry| entry.tool_call_id == call.id
                    && entry.withheld == ToolInputWithheld::PeerTier));
            } else {
                assert_eq!(
                    &call.input, input,
                    "TrustedLocal mirror/writer retains exact input"
                );
            }
        }
        if !peer {
            assert!(entries.is_empty());
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn story19_20c_projection_k04_real_turn_tier_privacy() {
        projection_k04_scenario(true).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn story19_20c_projection_k04_operator_live_tier_privacy() {
        projection_k04_scenario(false).await;
    }

    /// K04 enters signed sockets and the real turn, scheduler and Bash executor.
    /// Operator ToolUse is prose-folded in retained daemon history; the peer
    /// turn supplies structured ToolCallInfo for AttachAck/History coverage.
    async fn projection_k04_scenario(peer_turn: bool) {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let workspace = tempfile::tempdir().unwrap();
            let marker = "LATE_SECRET_19_20C_NEVER_A_PEER_ARGUMENT";
            let content = format!("{}{} \nUnicode 🧪\t", "prefix ".repeat(80), marker);
            let path = workspace.path().join("exact-write.txt");
            // Bash writes the marker without echoing it into tool output. Output
            // and prose secrecy are not an attach-input projection responsibility.
            let input = serde_json::json!({"command": format!("printf '%s' '{content}' > '{}'", path.display())});
            let core = projection_tool_core(workspace.path(), "Bash", input.clone());
            let runtime = core.ensure_runtime().await.unwrap();
            let mut approvals = runtime.approval.subscribe();
            let mut scheduler_events = runtime.tool_scheduler.subscribe();
            let conversation = Arc::new(Mutex::new(Conversation {
                id: "projection-k04".to_owned(),
                ..Default::default()
            }));
            let (bus, domain_rx) = EventBus::new(256);
            let socket = workspace.path().join("projection.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let server = AttachServer::new(core, conversation.clone(), bus.domain_tx);
            let shutdown = CancellationToken::new();
            let service = {
                let server = server.clone();
                let shutdown = shutdown.clone();
                tokio::spawn(async move {
                    server.run(listener, domain_rx, None, None, shutdown).await;
                })
            };
            let (mut writer, ack) =
                projection_socket(&socket, ConnectionTier::TrustedLocal, 101).await;
            assert!(matches!(
                ack,
                DaemonFrame::AttachAck {
                    granted_mode: AttachMode::ReadWrite,
                    ..
                }
            ));
            let (mut mirror, ack) =
                projection_socket(&socket, ConnectionTier::TrustedLocal, 102).await;
            assert!(matches!(
                ack,
                DaemonFrame::AttachAck {
                    granted_mode: AttachMode::ReadOnly,
                    ..
                }
            ));
            let (mut peer, ack) = projection_socket(&socket, ConnectionTier::Peer, 103).await;
            assert!(matches!(
                ack,
                DaemonFrame::AttachAck {
                    granted_mode: AttachMode::ReadOnly,
                    ..
                }
            ));
            if peer_turn {
                let mut task = inbound_task(test_signer(113).identity().peer_id.clone(), "write the supplied content");
                task.response_policy.mode = crate::domain::models::ResponseMode::NotifyAndDraft;
                server.start(task, CancellationToken::new()).await.unwrap();
            } else {
                write_frame(&mut writer, &ClientFrame::UserMessage {
                    text: "write the supplied content".to_owned(), images: vec![],
                }).await.unwrap();
            }
            let mut writer_phases = std::collections::BTreeSet::new();
            let mut approval_count = 0;
            let mut writer_uses = 0;
            let mut writer_results = 0;
            loop {
                let frame = projection_read(&mut writer).await;
                match &frame {
                    DaemonFrame::ApprovalRequest { request_id, .. } => {
                        approval_count += 1;
                        write_frame(
                            &mut writer,
                            &ClientFrame::ApprovalResponse {
                                request_id: request_id.clone(),
                                outcome: crate::domain::models::ApprovalOutcome::Once,
                            },
                        )
                        .await
                        .unwrap();
                    }
                    DaemonFrame::Event(RawEvent {
                        kind: RawEventKind::Provider(StreamChunk::ToolUse { input: actual, .. })
                            | RawEventKind::PeerProvider(StreamChunk::ToolUse { input: actual, .. }),
                        ..
                    }) => {
                        assert_eq!(actual, &input);
                        writer_uses += 1;
                    }
                    DaemonFrame::Event(RawEvent {
                        kind: RawEventKind::Tool(transition),
                        ..
                    }) => {
                        let value = serde_json::to_value(&transition.call).unwrap();
                        assert_eq!(value["request"]["input"], input);
                        writer_phases.insert(value["status"].as_str().unwrap().to_owned());
                    }
                    DaemonFrame::Event(RawEvent {
                        kind: RawEventKind::Provider(StreamChunk::ToolResult { is_error, .. })
                            | RawEventKind::PeerProvider(StreamChunk::ToolResult { is_error, .. }), ..
                    }) => {
                        assert!(!is_error);
                        writer_results += 1;
                    }
                    DaemonFrame::Event(RawEvent {
                        kind:
                            RawEventKind::Provider(StreamChunk::TurnComplete { stop_reason: StopReason::EndTurn })
                            | RawEventKind::PeerProvider(StreamChunk::TurnComplete { stop_reason: StopReason::EndTurn }),
                        ..
                    }) => break,
                    _ => {}
                }
            }
            assert_eq!(approval_count, 1);
            assert_eq!(writer_uses, 1);
            assert_eq!(writer_results, 1, "writer retains successful tool-result activity");
            // Rule 1: daemon has no producer forwarding scheduler transitions to
            // attach clients. Observe the real scheduler directly; the Tool
            // projection arm is guard-only, covered by supporting bus evidence.
            let mut scheduler_phases = std::collections::BTreeSet::new();
            while let Ok(transition) = scheduler_events.try_recv() {
                assert_eq!(transition.call.request().input, input);
                let value = serde_json::to_value(&transition.call).unwrap();
                scheduler_phases.insert(value["status"].as_str().unwrap().to_owned());
            }
            for phase in [
                "validating",
                "scheduled",
                "awaiting_approval",
                "executing",
                "success",
            ] {
                assert!(
                    scheduler_phases.contains(phase),
                    "real turn traverses scheduler phase {phase}"
                );
            }
            let mut requested = 0;
            while let Ok(event) = approvals.try_recv() {
                assert!(
                    !serde_json::to_string(&event).unwrap().contains(marker),
                    "runtime Requested must never broadcast input values"
                );
                if matches!(event, ApprovalRuntimeEvent::Requested { .. }) {
                    requested += 1;
                }
            }
            assert_eq!(requested, 1, "runtime subscription is a nonvacuous control");
            for (stream, peer_tier) in [(&mut mirror, false), (&mut peer, true)] {
                let mut uses = 0;
                let mut results = 0;
                let mut phases = std::collections::BTreeSet::new();
                loop {
                    let frame = projection_read(stream).await;
                    assert!(
                        !matches!(
                            frame,
                            DaemonFrame::ApprovalRequest { .. }
                                | DaemonFrame::ApprovalInputChunk { .. }
                                | DaemonFrame::ApprovalInputComplete { .. }
                        ),
                        "observer never receives exact-review channel"
                    );
                    if peer_tier {
                        assert!(
                            !serde_json::to_string(&frame).unwrap().contains(marker),
                            "peer wire privacy"
                        );
                    }
                    let event = match &frame {
                        DaemonFrame::ToolEventWithheld { event, withheld } => {
                            assert!(peer_tier);
                            assert_eq!(withheld, &ToolInputWithheld::PeerTier);
                            Some(event)
                        }
                        DaemonFrame::Event(event) => Some(event),
                        _ => None,
                    };
                    if let Some(event) = event {
                        match &event.kind {
                            RawEventKind::Provider(StreamChunk::ToolUse { input: actual, .. })
                            | RawEventKind::PeerProvider(StreamChunk::ToolUse { input: actual, .. }) => {
                                if peer_tier {
                                    assert!(actual.is_null());
                                } else {
                                    assert_eq!(actual, &input);
                                }
                                uses += 1;
                            }
                            RawEventKind::Tool(transition) => {
                                let value = serde_json::to_value(&transition.call).unwrap();
                                if peer_tier {
                                    assert!(value["request"]["input"].is_null());
                                } else {
                                    assert_eq!(value["request"]["input"], input);
                                }
                                phases.insert(value["status"].as_str().unwrap().to_owned());
                            }
                            RawEventKind::Provider(StreamChunk::ToolResult { is_error, .. })
                            | RawEventKind::PeerProvider(StreamChunk::ToolResult { is_error, .. }) => {
                                assert!(!is_error);
                                results += 1;
                            }
                            RawEventKind::Provider(StreamChunk::TurnComplete { stop_reason: StopReason::EndTurn })
                            | RawEventKind::PeerProvider(StreamChunk::TurnComplete { stop_reason: StopReason::EndTurn }) => break,
                            _ => {}
                        }
                    }
                }
                assert_eq!(uses, 1);
                assert_eq!(results, 1, "Peer and mirror retain successful tool results");
                assert_eq!(
                    phases, writer_phases,
                    "observer and writer share the same transition forwarding policy"
                );
            }
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                content,
                "real executor keeps exact value"
            );
            if !peer_turn {
                shutdown.cancel();
                drop((writer, mirror, peer));
                service.await.unwrap();
                return;
            }
            assert!(
                conversation
                    .lock()
                    .await
                    .messages
                    .iter()
                    .flat_map(|message| &message.tool_calls)
                    .any(|call| call.input == input),
                "retained conversation keeps exact input"
            );
            for (stream, peer_tier) in [
                (&mut writer, false),
                (&mut mirror, false),
                (&mut peer, true),
            ] {
                let frame = projection_history(stream).await;
                assert_projection_transcript(&frame, &input, peer_tier);
                if peer_tier {
                    assert!(!serde_json::to_string(&frame).unwrap().contains(marker));
                }
            }
            // Reattach through the signed front door to inspect fresh AttachAck snapshots.
            for (tier, signer) in [
                (ConnectionTier::TrustedLocal, 104),
                (ConnectionTier::Peer, 105),
            ] {
                let (stream, frame) = projection_socket(&socket, tier, signer).await;
                assert_projection_transcript(&frame, &input, tier == ConnectionTier::Peer);
                if tier == ConnectionTier::Peer {
                    assert!(!serde_json::to_string(&frame).unwrap().contains(marker));
                }
                drop(stream);
            }
            shutdown.cancel();
            drop((writer, mirror, peer));
            service.await.unwrap();
        })
        .await
        .expect("K04 signed socket real turn completes");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn story19_20c_projection_k07_cap_keeps_attached_connections_alive() {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let workspace = tempfile::tempdir().unwrap();
            let path = workspace.path().join("large-write.txt");
            let input = serde_json::json!({"file_path": path, "content": "x".repeat(MAX_FRAME_BYTES as usize)});
            let bytes = serialized_bytes(&input) as u64;
            let core = projection_tool_core(workspace.path(), "Write", input);
            let conversation = Arc::new(Mutex::new(Conversation { id: "cap".into(), ..Default::default() }));
            let (bus, domain_rx) = EventBus::new(256);
            let socket = workspace.path().join("cap.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let server = AttachServer::new(core, conversation, bus.domain_tx);
            let shutdown = CancellationToken::new();
            let service = {
                let server = server.clone(); let shutdown = shutdown.clone();
                tokio::spawn(async move { server.run(listener, domain_rx, None, None, shutdown).await; })
            };
            let (mut writer, _) = projection_socket(&socket, ConnectionTier::TrustedLocal, 106).await;
            let (mut mirror, _) = projection_socket(&socket, ConnectionTier::TrustedLocal, 107).await;
            let (mut peer, _) = projection_socket(&socket, ConnectionTier::Peer, 108).await;
            write_frame(&mut writer, &ClientFrame::UserMessage {
                text: "write the large input".into(), images: vec![],
            }).await.unwrap();
            for (stream, tier) in [(&mut writer, ConnectionTier::TrustedLocal),
                (&mut mirror, ConnectionTier::TrustedLocal), (&mut peer, ConnectionTier::Peer)] {
                loop {
                    let frame = projection_read(stream).await;
                    if let DaemonFrame::ToolEventWithheld { event, withheld } = frame {
                        assert_eq!(withheld, if tier == ConnectionTier::Peer {
                            ToolInputWithheld::PeerTier
                        } else { ToolInputWithheld::TooLarge { bytes } });
                        assert!(matches!(event.kind, RawEventKind::Provider(StreamChunk::ToolUse { input, .. }) if input.is_null()));
                        break;
                    }
                    assert!(!matches!(frame, DaemonFrame::FrameTooLarge { .. }), "tool input must use typed withholding");
                }
            }
            let mut approved = false;
            loop {
                match projection_read(&mut writer).await {
                    DaemonFrame::ApprovalRequest { request_id, .. } => {
                        approved = true;
                        write_frame(&mut writer, &ClientFrame::ApprovalResponse {
                            request_id, outcome: crate::domain::models::ApprovalOutcome::Once,
                        }).await.unwrap();
                    }
                    DaemonFrame::Event(RawEvent { kind: RawEventKind::Provider(StreamChunk::TurnComplete { stop_reason: StopReason::EndTurn }), .. }) => break,
                    _ => {}
                }
            }
            assert!(approved);
            assert_eq!(std::fs::metadata(&path).unwrap().len(), u64::from(MAX_FRAME_BYTES),
                "real scheduler/executor keeps the over-cap input exact");
            // Non-input prose cannot be stripped: exercise the universal writer guard.
            server.fanout(DaemonFrame::TeamStatusResult {
                request: "oversize".into(), message: "p".repeat(MAX_FRAME_BYTES as usize),
            }).await;
            server.fanout(DaemonFrame::PeerAccepted { sequence: 992 }).await;
            for stream in [&mut writer, &mut mirror, &mut peer] {
                loop {
                    if let DaemonFrame::FrameTooLarge { frame, bytes } = projection_read(stream).await {
                        assert_eq!(frame, "TeamStatusResult");
                        assert!(bytes > u64::from(MAX_FRAME_BYTES)); break;
                    }
                }
                assert!(matches!(projection_read(stream).await, DaemonFrame::PeerAccepted { sequence: 992 }));
            }
            shutdown.cancel(); drop((writer, mirror, peer)); service.await.unwrap();
        }).await.expect("K07 cap guard keeps sockets connected");
    }

    /// Supporting M16 evidence, not K04: no production daemon transition
    /// producer exists today. Feed the event at the real bus-consumer seam.
    #[tokio::test]
    async fn story19_20c_projection_supporting_bus_transition_privacy() {
        use crate::domain::models::{ToolCall, ToolCallRequest, ToolCallTransition};
        let workspace = tempfile::tempdir().unwrap();
        let (core, _) = mock_core(workspace.path(), vec![]);
        let (bus, _) = EventBus::new(64);
        let server = AttachServer::new(
            core,
            Arc::new(Mutex::new(Conversation::default())),
            bus.domain_tx,
        );
        let mut sockets = Vec::new();
        let mut connections = Vec::new();
        for tier in [
            ConnectionTier::TrustedLocal,
            ConnectionTier::TrustedLocal,
            ConnectionTier::Peer,
        ] {
            let (mut socket, daemon) = UnixStream::pair().unwrap();
            let owner = server.clone();
            connections.push(tokio::spawn(async move {
                owner.handle_connection(daemon).await;
            }));
            assert!(matches!(
                attach_with_proof(
                    &mut socket,
                    false,
                    tier,
                    &test_signer(109 + sockets.len() as u8)
                )
                .await,
                Some(DaemonFrame::AttachAck { .. })
            ));
            sockets.push(socket);
        }
        let input = serde_json::json!({"secret": "M16-transition-marker"});
        server
            .handle_bus_event(
                &AppEvent::ToolCallTransitionBridged {
                    conversation_id: "c".into(),
                    transition: ToolCallTransition {
                        conversation_id: "c".into(),
                        call: ToolCall::Scheduled {
                            id: "guard-only".into(),
                            request: ToolCallRequest {
                                id: "guard-only".into(),
                                tool_name: "unknown".into(),
                                input: input.clone(),
                            },
                        },
                    },
                },
                &mut String::new(),
            )
            .await;
        for (index, socket) in sockets.iter_mut().enumerate() {
            loop {
                let frame = projection_read(socket).await;
                match frame {
                    DaemonFrame::Event(mut event)
                        if matches!(event.kind, RawEventKind::Tool(_)) =>
                    {
                        assert!(index < 2, "Peer never receives an unprojected transition");
                        assert_eq!(event_tool_input(&mut event).unwrap(), &input);
                        break;
                    }
                    DaemonFrame::ToolEventWithheld {
                        mut event,
                        withheld,
                    } => {
                        assert_eq!(index, 2);
                        assert_eq!(withheld, ToolInputWithheld::PeerTier);
                        assert!(event_tool_input(&mut event).unwrap().is_null());
                        break;
                    }
                    _ => {}
                }
            }
        }
        for socket in &mut sockets {
            write_frame(socket, &ClientFrame::Detach).await.unwrap();
        }
        drop(sockets);
        for connection in connections {
            connection.await.unwrap();
        }
    }

    #[test]
    fn story19_20c_projection_peer_provider_and_all_transition_shapes() {
        use crate::domain::models::{
            ToolCall, ToolCallRequest, ToolCallResult, ToolCallTransition,
        };
        let input = serde_json::json!(["arbitrary", {"secret": "exact scalar/array values"}]);
        let request = ToolCallRequest {
            id: "call".into(),
            tool_name: "unknown".into(),
            input: input.clone(),
        };
        let id = "call".to_owned();
        let calls = vec![
            ToolCall::Validating {
                id: id.clone(),
                request: request.clone(),
                started_at: 1,
            },
            ToolCall::Scheduled {
                id: id.clone(),
                request: request.clone(),
            },
            ToolCall::AwaitingApproval {
                id: id.clone(),
                request: request.clone(),
                approval_id: Default::default(),
            },
            ToolCall::Executing {
                id: id.clone(),
                request: request.clone(),
                started_at: 1,
            },
            ToolCall::Success {
                id: id.clone(),
                request: request.clone(),
                result: ToolCallResult {
                    output: "visible output".into(),
                    is_error: false,
                    duration_ms: 1,
                },
            },
            ToolCall::Error {
                id: id.clone(),
                request: request.clone(),
                error: "visible error".into(),
            },
            ToolCall::Cancelled {
                id: id.clone(),
                request,
                reason: "visible cancellation".into(),
            },
        ];
        let mut kinds = vec![RawEventKind::PeerProvider(StreamChunk::ToolUse {
            id,
            name: "unknown".into(),
            input: input.clone(),
        })];
        kinds.extend(calls.into_iter().map(|call| {
            RawEventKind::Tool(ToolCallTransition {
                conversation_id: "c".into(),
                call,
            })
        }));
        for kind in kinds {
            let frame = DaemonFrame::Event(RawEvent {
                conversation_id: Some("c".into()),
                timestamp_ms: 1,
                kind,
            });
            let original = serde_json::to_value(&frame).unwrap();
            assert_eq!(
                serde_json::to_value(project_attach_frame(
                    frame.clone(),
                    ConnectionTier::TrustedLocal
                ))
                .unwrap(),
                original
            );
            let projected = project_attach_frame(frame, ConnectionTier::Peer);
            let DaemonFrame::ToolEventWithheld {
                mut event,
                withheld,
            } = projected
            else {
                panic!("every tool event has typed Peer withholding");
            };
            assert_eq!(withheld, ToolInputWithheld::PeerTier);
            assert!(event_tool_input(&mut event).unwrap().is_null());
            *event_tool_input(&mut event).unwrap() = input.clone();
            assert_eq!(
                serde_json::to_value(DaemonFrame::Event(event)).unwrap(),
                original,
                "only the input changes; identity/status/results/prose remain"
            );
        }
    }

    #[test]
    fn story19_20c_projection_history_aggregate_withholds_largest_first() {
        use crate::domain::models::ToolCallInfo;
        let large = serde_json::json!({"content": "x".repeat(MAX_FRAME_BYTES as usize / 2)});
        let small = serde_json::json!({"path": "keep/me"});
        let call = |id: &str, input| ToolCallInfo {
            id: id.to_owned(),
            name: "Write".into(),
            input,
            result: None,
            started_at_ms: None,
            completed_at_ms: None,
            status: None,
        };
        let message = ChatMessage {
            id: "m".into(),
            tool_calls: vec![
                call("large-a", large.clone()),
                call("small", small.clone()),
                call("large-b", large),
            ],
            ..Default::default()
        };
        let frame = project_attach_frame(
            DaemonFrame::History {
                messages: vec![message],
                has_more: true,
                withheld_tool_inputs: vec![],
            },
            ConnectionTier::TrustedLocal,
        );
        assert!(serialized_bytes(&frame) <= MAX_FRAME_BYTES as usize);
        let DaemonFrame::History {
            messages,
            has_more,
            withheld_tool_inputs,
        } = frame
        else {
            unreachable!()
        };
        assert!(has_more);
        assert_eq!(messages[0].tool_calls[1].input, small);
        assert_eq!(
            withheld_tool_inputs.len(),
            1,
            "only one largest input needs withholding"
        );
        assert!(matches!(
            withheld_tool_inputs[0].withheld,
            ToolInputWithheld::TooLarge { .. }
        ));
    }

    #[test]
    fn team_request_gates_cover_trusted_and_peer_authority() {
        for write_required in [false, true] {
            assert_eq!(
                AttachServer::team_request_error(
                    ConnectionTier::TrustedLocal,
                    AttachMode::ReadWrite,
                    write_required,
                ),
                None
            );
            assert_eq!(
                AttachServer::team_request_error(
                    ConnectionTier::Peer,
                    AttachMode::ReadOnly,
                    write_required,
                ),
                Some(ProtocolError::PeerVerification(
                    "team commands are trusted-local only".to_owned()
                ))
            );
        }
        assert_eq!(
            AttachServer::team_request_error(
                ConnectionTier::TrustedLocal,
                AttachMode::ReadOnly,
                true,
            ),
            Some(ProtocolError::ReadOnly)
        );
        assert_eq!(
            AttachServer::team_request_error(
                ConnectionTier::TrustedLocal,
                AttachMode::ReadOnly,
                false,
            ),
            None
        );
    }

    #[tokio::test]
    async fn send_action_gate_holds_eight_admitted_tasks_until_released() {
        let hooks = TeamSendTestHooks::new(None, None, false).with_send_action_gate();
        let (tx, mut rx) = mpsc::unbounded_channel();
        for _ in 0..MAX_IN_FLIGHT_TEAM_SENDS {
            let hooks = hooks.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                hooks.before_send_action().await;
                tx.send(()).unwrap();
            });
        }
        hooks
            .wait_for_send_action_count(MAX_IN_FLIGHT_TEAM_SENDS)
            .await;
        assert_eq!(hooks.send_action_count(), MAX_IN_FLIGHT_TEAM_SENDS);
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));

        hooks.release_send_actions(MAX_IN_FLIGHT_TEAM_SENDS);
        for _ in 0..MAX_IN_FLIGHT_TEAM_SENDS {
            rx.recv().await.expect("one released send task");
        }
    }

    #[tokio::test]
    async fn daemon_send_completion_observer_is_ordered_and_counts_exits() {
        let hooks = TeamSendTestHooks::new(None, None, false);
        let semaphore = Arc::new(Semaphore::new(1));
        let permit = semaphore.clone().acquire_owned().await.unwrap();
        drop(permit);
        hooks.record_completion("finished".to_owned(), TeamSendCompletionKind::Finished);
        hooks.record_completion("cancelled".to_owned(), TeamSendCompletionKind::Cancelled);

        assert_eq!(hooks.completed_count(), 2);
        assert_eq!(
            hooks.next_completion().await,
            Some(TeamSendCompletion {
                request: "finished".to_owned(),
                kind: TeamSendCompletionKind::Finished,
            })
        );
        assert_eq!(
            hooks.next_completion().await,
            Some(TeamSendCompletion {
                request: "cancelled".to_owned(),
                kind: TeamSendCompletionKind::Cancelled,
            })
        );
        assert!(matches!(
            hooks.try_next_completion().await,
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn team_send_limit_and_targeted_overflow_are_nonblocking() {
        let slots = Arc::new(Semaphore::new(MAX_IN_FLIGHT_TEAM_SENDS));
        let permits: Vec<_> = (0..MAX_IN_FLIGHT_TEAM_SENDS)
            .map(|_| slots.clone().try_acquire_owned().expect("available slot"))
            .collect();
        assert!(slots.clone().try_acquire_owned().is_err());
        drop(permits.into_iter().next());
        assert!(slots.clone().try_acquire_owned().is_ok());

        let (tx, mut rx) = mpsc::channel(1);
        let (terminal_tx, mut terminal_rx) = watch::channel(None);
        tx.try_send(DaemonFrame::Detached).unwrap();
        assert!(!publish_team_frame(
            &tx,
            &terminal_tx,
            DaemonFrame::TeamStatusResult {
                request: "request".into(),
                message: "status".into(),
            },
        ));
        terminal_rx.changed().await.unwrap();
        assert_eq!(
            *terminal_rx.borrow(),
            Some(ProtocolError::Internal(
                "team result queue overflow; connection closed".to_owned()
            ))
        );
        assert!(matches!(rx.try_recv(), Ok(DaemonFrame::Detached)));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn team_send_fifo_hooks_hold_and_report_commands_by_connection() {
        let ticks = Arc::new(AtomicUsize::new(1));
        let clock_ticks = ticks.clone();
        let start = std::time::Instant::now();
        let hooks = TeamSendTestHooks::new(Some(1), None, false).with_board_now_source(Arc::new(
            move || {
                start
                    + std::time::Duration::from_nanos(
                        clock_ticks.fetch_add(1, Ordering::AcqRel) as u64
                    )
            },
        ));
        assert!(hooks.board_now().unwrap() < hooks.board_now().unwrap());
        hooks.record_fifo("request-2", 42, TeamSendFifoEventKind::Enqueued);
        assert_eq!(
            hooks.next_fifo_event().await,
            Some(TeamSendFifoEvent {
                request: "request-2".into(),
                connection_id: 42,
                kind: TeamSendFifoEventKind::Enqueued,
            })
        );

        let worker_hooks = hooks.clone();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (released_tx, mut released_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _ = ready_tx.send(());
            worker_hooks.before_team_send().await;
            let _ = released_tx.send(());
        });
        ready_rx.await.expect("worker reached its gate");
        assert!(released_rx.try_recv().is_err());
        hooks.release_worker();
        released_rx.await.expect("worker gate released");

        hooks.record_fifo("request-2", 42, TeamSendFifoEventKind::Dequeued);
        assert_eq!(
            hooks.next_fifo_event().await,
            Some(TeamSendFifoEvent {
                request: "request-2".into(),
                connection_id: 42,
                kind: TeamSendFifoEventKind::Dequeued,
            })
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn queued_team_send_is_observed_then_discarded_after_writer_replacement() {
        let tmp = tempfile::tempdir().unwrap();
        let (core, _) = mock_core(tmp.path(), vec![]);
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "team-fifo-hooks".into(),
            ..Default::default()
        }));
        let (bus, _domain_rx) = EventBus::new(64);
        let hooks = TeamSendTestHooks::new(Some(1), None, false);
        let server = journaled_server_with_team_hooks(
            core,
            conversation,
            bus.domain_tx.clone(),
            Arc::new(RecordingJournal::default()),
            hooks.clone(),
        );

        let (mut first_client, first_server) = UnixStream::pair().unwrap();
        let first_server_owner = server.clone();
        let first_connection =
            tokio::spawn(async move { first_server_owner.handle_connection(first_server).await });
        assert!(matches!(
            attach_with_proof(
                &mut first_client,
                false,
                ConnectionTier::TrustedLocal,
                &test_signer(61),
            )
            .await,
            Some(DaemonFrame::AttachAck {
                granted_mode: AttachMode::ReadWrite,
                ..
            })
        ));
        for request in ["queued-1", "queued-2"] {
            write_frame(
                &mut first_client,
                &ClientFrame::TeamSend {
                    request: request.to_owned(),
                    recipients: vec![],
                    text: "held before dispatch".to_owned(),
                    message_type: None,
                    subject: None,
                },
            )
            .await
            .unwrap();
        }
        for request in ["queued-1", "queued-2"] {
            assert_eq!(
                hooks.next_fifo_event().await,
                Some(TeamSendFifoEvent {
                    request: request.to_owned(),
                    connection_id: 1,
                    kind: TeamSendFifoEventKind::Enqueued,
                })
            );
        }
        hooks.wait_for_worker_gate_count(1).await;
        write_frame(&mut first_client, &ClientFrame::Detach)
            .await
            .unwrap();
        first_connection.await.unwrap();

        let (mut replacement_client, replacement_server) = UnixStream::pair().unwrap();
        let replacement_owner = server.clone();
        let replacement_connection = tokio::spawn(async move {
            replacement_owner
                .handle_connection(replacement_server)
                .await
        });
        assert!(matches!(
            attach_with_proof(
                &mut replacement_client,
                false,
                ConnectionTier::TrustedLocal,
                &test_signer(62),
            )
            .await,
            Some(DaemonFrame::AttachAck {
                granted_mode: AttachMode::ReadWrite,
                ..
            })
        ));

        hooks.release_worker();
        for expected in ["queued-1", "queued-2"] {
            assert_eq!(
                hooks.next_fifo_event().await,
                Some(TeamSendFifoEvent {
                    request: expected.to_owned(),
                    connection_id: 1,
                    kind: TeamSendFifoEventKind::Discarded,
                })
            );
        }
        assert!(matches!(
            hooks.try_next_publication().await,
            Err(mpsc::error::TryRecvError::Empty)
        ));

        write_frame(
            &mut replacement_client,
            &ClientFrame::TeamSend {
                request: "current-writer".to_owned(),
                recipients: vec![],
                text: "handled by current writer".to_owned(),
                message_type: None,
                subject: None,
            },
        )
        .await
        .unwrap();
        for kind in [
            TeamSendFifoEventKind::Enqueued,
            TeamSendFifoEventKind::Dequeued,
            TeamSendFifoEventKind::Handled,
        ] {
            assert_eq!(
                hooks.next_fifo_event().await,
                Some(TeamSendFifoEvent {
                    request: "current-writer".to_owned(),
                    connection_id: 2,
                    kind,
                })
            );
        }
        assert!(matches!(
            hooks.next_publication().await,
            Some(TeamSendPublication {
                request,
                kind: TeamSendPublicationKind::Refused {
                    action: RecipientAction::TeamSend,
                },
                published_to_conn: 2,
            }) if request == "current-writer"
        ));

        write_frame(&mut replacement_client, &ClientFrame::Detach)
            .await
            .unwrap();
        replacement_connection.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn socket_overflow_emits_terminal_error_then_eof_after_writer_release() {
        let tmp = tempfile::tempdir().unwrap();
        let (core, _) = mock_core(tmp.path(), vec![]);
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "team-overflow-hooks".into(),
            ..Default::default()
        }));
        let (bus, _domain_rx) = EventBus::new(64);
        let hooks = TeamSendTestHooks::new(None, Some(1), true);
        let server = journaled_server_with_team_hooks(
            core,
            conversation,
            bus.domain_tx.clone(),
            Arc::new(RecordingJournal::default()),
            hooks.clone(),
        );
        let (mut client, server_stream) = UnixStream::pair().unwrap();
        let server_owner = server.clone();
        let connection =
            tokio::spawn(async move { server_owner.handle_connection(server_stream).await });
        assert!(matches!(
            attach_with_proof(
                &mut client,
                false,
                ConnectionTier::TrustedLocal,
                &test_signer(63),
            )
            .await,
            Some(DaemonFrame::AttachAck {
                granted_mode: AttachMode::ReadWrite,
                ..
            })
        ));
        for request in ["board-1", "board-2"] {
            write_frame(
                &mut client,
                &ClientFrame::TeamBoard {
                    request: request.to_owned(),
                    selector: crate::adapters::daemon::protocol::TeamBoardRequest::Inventory,
                },
            )
            .await
            .unwrap();
            assert_eq!(
                hooks.next_publication().await,
                Some(TeamSendPublication {
                    request: request.to_owned(),
                    kind: TeamSendPublicationKind::Refused {
                        action: RecipientAction::TeamBoard,
                    },
                    published_to_conn: 1,
                })
            );
        }
        assert_eq!(hooks.overflow_count(), 1);
        let mut probe = [0u8; 1];
        assert!(matches!(
            client.try_read(&mut probe),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
        ));
        assert!(matches!(
            hooks.try_next_terminal_event().await,
            Err(mpsc::error::TryRecvError::Empty)
        ));

        hooks.release_writer();
        assert!(matches!(
            read_frame::<_, DaemonFrame>(&mut client).await.unwrap(),
            Some(DaemonFrame::Error(ProtocolError::Internal(message)))
                if message == "team result queue overflow; connection closed"
        ));
        assert_eq!(
            hooks.next_terminal_event().await,
            Some(TeamSendTerminalEvent {
                connection_id: 1,
                kind: TeamSendTerminalEventKind::Error(ProtocolError::Internal(
                    "team result queue overflow; connection closed".to_owned()
                )),
            })
        );
        assert!(
            read_frame::<_, DaemonFrame>(&mut client)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            hooks.next_terminal_event().await,
            Some(TeamSendTerminalEvent {
                connection_id: 1,
                kind: TeamSendTerminalEventKind::Closed,
            })
        );
        connection.await.unwrap();
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn operator_fifo_overflow_drains_exact_error_then_closes_socket() {
        let tmp = tempfile::tempdir().unwrap();
        let (core, _) = mock_core(tmp.path(), vec![]);
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "operator-fifo-overflow".into(),
            ..Default::default()
        }));
        let (bus, _domain_rx) = EventBus::new(64);
        let hooks = TeamSendTestHooks::new(Some(1), None, false);
        let server = journaled_server_with_team_hooks(
            core,
            conversation,
            bus.domain_tx.clone(),
            Arc::new(RecordingJournal::default()),
            hooks.clone(),
        );
        let (mut client, server_stream) = UnixStream::pair().unwrap();
        let owner = server.clone();
        let connection = tokio::spawn(async move { owner.handle_connection(server_stream).await });
        assert!(matches!(
            attach_with_proof(
                &mut client,
                false,
                ConnectionTier::TrustedLocal,
                &test_signer(71),
            )
            .await,
            Some(DaemonFrame::AttachAck {
                granted_mode: AttachMode::ReadWrite,
                ..
            })
        ));

        for index in 0..=(OPERATOR_QUEUE_DEPTH + 1) {
            write_frame(
                &mut client,
                &ClientFrame::TeamSend {
                    request: format!("queued-{index}"),
                    recipients: vec!["peer".into()],
                    text: "body".into(),
                    message_type: None,
                    subject: None,
                },
            )
            .await
            .unwrap();
            if index == 0 {
                hooks.wait_for_worker_gate_count(1).await;
            }
        }
        assert!(matches!(
            read_frame::<_, DaemonFrame>(&mut client).await.unwrap(),
            Some(DaemonFrame::Error(ProtocolError::Internal(message)))
                if message == "operator command queue is full; connection closed"
        ));
        assert!(
            read_frame::<_, DaemonFrame>(&mut client)
                .await
                .unwrap()
                .is_none(),
            "the diagnostic must be drained before EOF"
        );
        hooks.release_worker();
        connection.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn signed_socket_reassembles_board_larger_than_frame_cap_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let (core, _) = mock_core(tmp.path(), vec![]);
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "large-board-stream".into(),
            ..Default::default()
        }));
        let (bus, _domain_rx) = EventBus::new(64);
        let hooks = TeamSendTestHooks::new(None, Some(1), false);
        let server = journaled_server_with_team_hooks(
            core,
            conversation,
            bus.domain_tx.clone(),
            Arc::new(RecordingJournal::default()),
            hooks,
        );
        let (mut client, server_stream) = UnixStream::pair().unwrap();
        let owner = server.clone();
        let connection = tokio::spawn(async move { owner.handle_connection(server_stream).await });
        assert!(matches!(
            attach_with_proof(
                &mut client,
                false,
                ConnectionTier::TrustedLocal,
                &test_signer(72),
            )
            .await,
            Some(DaemonFrame::AttachAck { .. })
        ));
        let conn_id = server.registry.lock().await.conns[0].id;
        let expected = "row\n\\\u{0001}🧪".repeat(1_000_000);
        assert!(expected.len() > MAX_FRAME_BYTES as usize);
        let request_id = "\u{0001}".repeat(team::MAX_TEAM_REQUEST_BYTES);
        let (tx, terminal_tx) = server.team_connection(conn_id).await.unwrap();
        let hooks = server.team_send_test_hooks.clone();
        let source = expected.clone();
        let response_request = request_id.clone();
        let send = tokio::spawn(async move {
            AttachServer::send_team_view(
                tx,
                terminal_tx,
                conn_id,
                response_request,
                source,
                RecipientAction::TeamBoard,
                hooks,
            )
            .await
        });

        let mut assembled = String::new();
        let mut next_sequence = 0u32;
        loop {
            let frame = read_frame::<_, DaemonFrame>(&mut client)
                .await
                .unwrap()
                .expect("board stream remains connected through its done frame");
            assert!(serde_json::to_vec(&frame).unwrap().len() <= MAX_FRAME_BYTES as usize);
            match frame {
                DaemonFrame::TeamViewChunk {
                    request,
                    sequence,
                    view,
                } => {
                    assert_eq!(request, request_id);
                    assert_eq!(sequence, next_sequence);
                    next_sequence += 1;
                    assembled.push_str(&view);
                }
                DaemonFrame::TeamViewDone { request, chunks } => {
                    assert_eq!(request, request_id);
                    assert_eq!(chunks, next_sequence);
                    break;
                }
                other => panic!("unexpected board stream frame: {other:?}"),
            }
        }
        assert!(send.await.unwrap());
        assert_eq!(assembled, expected);
        write_frame(&mut client, &ClientFrame::Detach)
            .await
            .unwrap();
        connection.await.unwrap();
    }
    #[tokio::test]
    async fn targeted_queue_gate_and_publication_log_are_deterministic() {
        let hooks = TeamSendTestHooks::new(None, Some(1), true);
        let (tx, mut rx) = mpsc::channel(hooks.queue_capacity().unwrap());
        let (terminal_tx, mut terminal_rx) = watch::channel(None);
        assert!(publish_team_frame_to_connection(
            &tx,
            &terminal_tx,
            DaemonFrame::TeamSendStarted {
                request: "send-1".into(),
                action: crate::domain::models::SendActionId::from_replay("sa_test"),
            },
            8,
            Some(&hooks),
        ));
        assert_eq!(
            hooks.next_publication().await,
            Some(TeamSendPublication {
                request: "send-1".into(),
                kind: TeamSendPublicationKind::Started,
                published_to_conn: 8,
            })
        );
        let _ = rx.try_recv();
        assert!(publish_team_frame_to_connection(
            &tx,
            &terminal_tx,
            DaemonFrame::TeamSendSettled {
                request: "send-1".into(),
                index: 0,
                outcome: crate::adapters::daemon::protocol::TeamSendOutcome::NotSent,
            },
            8,
            Some(&hooks),
        ));
        assert_eq!(
            hooks.next_publication().await,
            Some(TeamSendPublication {
                request: "send-1".into(),
                kind: TeamSendPublicationKind::Settled { index: 0 },
                published_to_conn: 8,
            })
        );
        let _ = rx.try_recv();
        let first = DaemonFrame::TeamViewDone {
            request: "board-1".into(),
            chunks: 0,
        };
        assert!(publish_team_frame_to_connection(
            &tx,
            &terminal_tx,
            first,
            8,
            Some(&hooks),
        ));
        assert_eq!(
            hooks.next_publication().await,
            Some(TeamSendPublication {
                request: "board-1".into(),
                kind: TeamSendPublicationKind::BoardResult,
                published_to_conn: 8,
            })
        );
        assert!(!publish_team_frame_to_connection(
            &tx,
            &terminal_tx,
            DaemonFrame::TeamStatusResult {
                request: "status-2".into(),
                message: "status".into(),
            },
            8,
            Some(&hooks),
        ));
        assert_eq!(
            hooks.next_publication().await,
            Some(TeamSendPublication {
                request: "status-2".into(),
                kind: TeamSendPublicationKind::StatusResult,
                published_to_conn: 8,
            })
        );
        terminal_rx.changed().await.unwrap();
        assert_eq!(hooks.overflow_count(), 1);
        assert_eq!(
            *terminal_rx.borrow(),
            Some(ProtocolError::Internal(
                "team result queue overflow; connection closed".to_owned()
            ))
        );
        assert!(matches!(
            rx.try_recv(),
            Ok(DaemonFrame::TeamViewDone { .. })
        ));
        assert!(rx.try_recv().is_err());

        let writer_hooks = hooks.clone();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (released_tx, mut released_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _ = ready_tx.send(());
            writer_hooks.before_writer_drain().await;
            let _ = released_tx.send(());
        });
        ready_rx.await.expect("writer reached its gate");
        assert!(released_rx.try_recv().is_err());
        hooks.release_writer();
        released_rx.await.expect("writer gate released");
    }
    #[test]
    fn recipient_acknowledgement_requires_all_three_authority_gates() {
        let operator = AgentId::local_operator();
        assert_eq!(
            recipient_ack_gate(
                AttachMode::ReadWrite,
                ConnectionTier::TrustedLocal,
                &operator,
            ),
            RecipientAckGate::Allow
        );
        assert_eq!(
            recipient_ack_gate(
                AttachMode::ReadOnly,
                ConnectionTier::TrustedLocal,
                &operator,
            ),
            RecipientAckGate::ReadOnly
        );
        assert_eq!(
            recipient_ack_gate(AttachMode::ReadWrite, ConnectionTier::Peer, &operator),
            RecipientAckGate::UntrustedTier
        );
        assert_eq!(
            recipient_ack_gate(
                AttachMode::ReadWrite,
                ConnectionTier::TrustedLocal,
                &AgentId::root(),
            ),
            RecipientAckGate::RoleDenied
        );
    }

    #[test]
    fn inbound_peer_refusal_reason_separates_policy_unavailable_and_terminal() {
        use crate::domain::models::{DeliveryDisposition, RefuseReason};

        assert_eq!(
            inbound_peer_refuse_reason(DeliveryDisposition::MayRefuse, false),
            RefuseReason::Policy
        );
        assert_eq!(
            inbound_peer_refuse_reason(DeliveryDisposition::MustReport, false),
            RefuseReason::Unavailable
        );
        assert_eq!(
            inbound_peer_refuse_reason(DeliveryDisposition::MayRefuse, true),
            RefuseReason::TerminalState
        );
        assert_eq!(
            inbound_peer_refuse_reason(DeliveryDisposition::MustReport, true),
            RefuseReason::TerminalState
        );
    }

    /// A provider that replays a fixed chunk script — deterministic, no network.
    struct ScriptedProvider {
        chunks: Vec<StreamChunk>,
    }

    #[async_trait::async_trait]
    impl StreamingProvider for ScriptedProvider {
        async fn stream_completion(
            &self,
            _messages: Vec<Message>,
            _options: CompletionOptions,
        ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
            use futures::StreamExt;
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
        ) -> Result<crate::domain::ports::ProbeOutcome, crate::domain::errors::ProviderError>
        {
            Ok(crate::domain::ports::ProbeOutcome {
                latency: std::time::Duration::ZERO,
            })
        }
    }

    struct ToolThenTextProvider {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl StreamingProvider for ToolThenTextProvider {
        async fn stream_completion(
            &self,
            _messages: Vec<Message>,
            _options: CompletionOptions,
        ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
            use futures::StreamExt;
            let chunks = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                vec![
                    StreamChunk::ToolUse {
                        id: "d0-tool".to_owned(),
                        name: "Bash".to_owned(),
                        input: serde_json::json!({"command": "echo d0"}),
                    },
                    StreamChunk::TurnComplete {
                        stop_reason: StopReason::ToolUse,
                    },
                ]
            } else {
                vec![
                    StreamChunk::Text {
                        content: "done".to_owned(),
                        parent_tool_use_id: None,
                    },
                    StreamChunk::TurnComplete {
                        stop_reason: StopReason::EndTurn,
                    },
                ]
            };
            Ok(futures::stream::iter(chunks).boxed())
        }
        async fn abort(&self) -> Result<(), ProviderError> {
            Ok(())
        }
        fn provider_id(&self) -> String {
            "d0-tool-then-text".to_owned()
        }
        fn list_models(&self) -> Vec<ModelDescriptor> {
            vec![]
        }
        async fn health_check(&self) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn connectivity_probe(
            &self,
        ) -> Result<crate::domain::ports::ProbeOutcome, ProviderError> {
            Ok(crate::domain::ports::ProbeOutcome {
                latency: std::time::Duration::ZERO,
            })
        }
    }

    /// First call streams one text chunk and then stalls; the next call completes.
    /// The stall lets an inbound cancellation exercise the `handle.abort()` path
    /// after text has reached the daemon's forwarding tee.
    struct AbortThenCompleteProvider {
        calls: AtomicUsize,
        partial_streamed: Arc<Notify>,
    }

    #[async_trait::async_trait]
    impl StreamingProvider for AbortThenCompleteProvider {
        async fn stream_completion(
            &self,
            _messages: Vec<Message>,
            _options: CompletionOptions,
        ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
            use futures::StreamExt;

            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                let partial_streamed = self.partial_streamed.clone();
                let stream = futures::stream::unfold(0_u8, move |state| {
                    let partial_streamed = partial_streamed.clone();
                    async move {
                        if state == 0 {
                            Some((
                                StreamChunk::Text {
                                    content: "partial cancelled output".to_owned(),
                                    parent_tool_use_id: None,
                                },
                                1,
                            ))
                        } else {
                            partial_streamed.notify_waiters();
                            std::future::pending::<Option<(StreamChunk, u8)>>().await
                        }
                    }
                });
                Ok(stream.boxed())
            } else {
                Ok(futures::stream::iter(vec![
                    StreamChunk::Text {
                        content: "fresh output".to_owned(),
                        parent_tool_use_id: None,
                    },
                    StreamChunk::TurnComplete {
                        stop_reason: StopReason::EndTurn,
                    },
                ])
                .boxed())
            }
        }

        async fn abort(&self) -> Result<(), ProviderError> {
            Ok(())
        }

        fn provider_id(&self) -> String {
            "abort-then-complete".to_owned()
        }

        fn list_models(&self) -> Vec<ModelDescriptor> {
            vec![]
        }

        async fn health_check(&self) -> Result<(), ProviderError> {
            Ok(())
        }

        async fn connectivity_probe(
            &self,
        ) -> Result<crate::domain::ports::ProbeOutcome, ProviderError> {
            Ok(crate::domain::ports::ProbeOutcome {
                latency: std::time::Duration::ZERO,
            })
        }
    }

    fn mock_runtime(
        provider: Arc<dyn StreamingProvider>,
        storage: Arc<dyn StoragePort>,
        workspace: &Path,
    ) -> Arc<DaemonTurnRuntime> {
        let security: Arc<dyn SecurityPort> = Arc::new(NoOpSecurity);
        let tools: Arc<dyn ToolSetPort> = Arc::new(NoOpToolSet);
        let approval = ApprovalRuntime::new(64, Arc::new(NoOpApprovalPersistence));
        let tool_scheduler = crate::domain::services::tool_scheduler::ToolScheduler::new(
            security.clone(),
            tools.clone(),
            approval.clone(),
            64,
        );
        Arc::new(DaemonTurnRuntime {
            recipient_context: Arc::new(Default::default()),
            provider,
            app_config: Arc::new(ArcSwap::from_pointee(AppConfig::default())),
            security,
            tools,
            tool_scheduler,
            persona: Arc::new(NoOpPersona),
            context_assembler: Arc::new(ArcSwap::from_pointee(None)),
            context: Arc::new(ArcSwap::from_pointee(
                Arc::new(crate::adapters::noop::NoOpContext)
                    as Arc<dyn crate::domain::ports::ContextPort>,
            )),
            storage: storage.clone(),
            fs_storage: Arc::new(FileSystemStorage::with_workspace_root(
                crate::infrastructure::paths::sessions_dir(workspace),
                workspace.to_path_buf(),
            )),
            usage_ledger: Arc::new(NoOpUsageLedger),
            telemetry: crate::infrastructure::telemetry::ActiveRatioWindow::new_in_memory(),
            plan_injector: Arc::new(
                crate::domain::services::plan_mode_injector::DefaultPlanInjector::new(),
            ),
            approval,
            workspace: workspace.to_path_buf(),
            #[cfg(feature = "mcp")]
            mcp_task_runtimes: Vec::new(),
        })
    }

    pub(super) fn mock_core(
        workspace: &Path,
        chunks: Vec<StreamChunk>,
    ) -> (Arc<DaemonCore>, Arc<dyn StoragePort>) {
        mock_core_with_memory(workspace, chunks, Arc::new(NoOpMemory))
    }

    fn mock_core_with_provider(
        workspace: &Path,
        provider: Arc<dyn StreamingProvider>,
    ) -> (Arc<DaemonCore>, Arc<dyn StoragePort>) {
        let storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
            crate::infrastructure::paths::sessions_dir(workspace),
            workspace.to_path_buf(),
        ));
        let workspace_for_factory = workspace.to_path_buf();
        let storage_for_factory = storage.clone();
        let core = DaemonCore::new(
            workspace.to_path_buf(),
            Arc::new(ArcSwap::from_pointee(AppConfig::default())),
            Arc::new(NoOpMemory),
            storage.clone(),
            Arc::new(NoOpSecurity),
            Arc::new(NoOpPersona),
            Arc::new(crate::adapters::rap::PeerTopicStore::new()),
            Box::new(move || {
                Ok(mock_runtime(
                    provider.clone(),
                    storage_for_factory.clone(),
                    &workspace_for_factory,
                ))
            }),
        );
        (Arc::new(core), storage)
    }

    /// Like [`mock_core`] but with an injectable [`MemoryPort`] — lets a test drive the
    /// consolidation-resolve apply path against a memory that errors on `remember_fact`
    /// (Story 12.2d review P2/G9 — write-failure must preserve the marker).
    fn mock_core_with_memory(
        workspace: &Path,
        chunks: Vec<StreamChunk>,
        memory: Arc<dyn crate::domain::ports::MemoryPort>,
    ) -> (Arc<DaemonCore>, Arc<dyn StoragePort>) {
        let storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
            crate::infrastructure::paths::sessions_dir(workspace),
            workspace.to_path_buf(),
        ));
        mock_core_with_storage(workspace, chunks, memory, storage)
    }

    /// Like [`mock_core`] but with an injectable [`StoragePort`]. The same
    /// `storage` handle is wired into both the [`DaemonCore`] and the per-turn
    /// runtime factory, so a wrapping storage observes every save the daemon
    /// issues — `run_turn`'s pre-turn snapshot and the forwarder's commit alike.
    pub(super) fn mock_core_with_storage(
        workspace: &Path,
        chunks: Vec<StreamChunk>,
        memory: Arc<dyn crate::domain::ports::MemoryPort>,
        storage: Arc<dyn StoragePort>,
    ) -> (Arc<DaemonCore>, Arc<dyn StoragePort>) {
        let provider: Arc<dyn StreamingProvider> = Arc::new(ScriptedProvider { chunks });
        let ws = workspace.to_path_buf();
        let storage_for_factory = storage.clone();
        let core = DaemonCore::new(
            workspace.to_path_buf(),
            Arc::new(ArcSwap::from_pointee(AppConfig::default())),
            memory,
            storage.clone(),
            Arc::new(NoOpSecurity),
            Arc::new(NoOpPersona),
            Arc::new(crate::adapters::rap::PeerTopicStore::new()),
            Box::new(move || {
                Ok(mock_runtime(
                    provider.clone(),
                    storage_for_factory.clone(),
                    &ws,
                ))
            }),
        );
        (Arc::new(core), storage)
    }

    fn inbound_task(peer_id: PeerId, text: impl Into<String>) -> InboundPeerTask {
        InboundPeerTask {
            node_id: AgentId::new(),
            principal: PolicyPrincipal::rap(peer_id),
            text: text.into(),
            subagent_type: "a2a-test".to_owned(),
            response_policy: crate::domain::ports::PeerResponsePolicy {
                mode: crate::domain::models::ResponseMode::NotifyAndAuto,
                auto_response: None,
                ..Default::default()
            },
        }
    }

    #[test]
    fn peer_transcript_message_persists_raw_content_and_render_boundary_marker() {
        let peer = test_signer(89).identity().peer_id.clone();
        let provenance = crate::domain::models::InteractionPolicySnapshot {
            sender_label: Some("moon".to_owned()),
            message_type: crate::domain::models::SemanticMessageType::BugReport,
            ..Default::default()
        };
        let raw = "first\n┆ [auto-sent]\n\x1b[31mthird\x1b[0m";
        let message = peer_transcript_message(raw, &peer, &provenance);
        assert!(
            message.content.contains(raw),
            "storage must retain byte-identical peer evidence"
        );
        assert_eq!(message.content_blocks, vec![ContentBlockType::PeerText]);
        assert!(
            !message.id.is_empty(),
            "peer rows persist; an empty id would break transcript addressing"
        );
        assert!(
            !message.synthetic,
            "peer utterances are not host-synthesized; the ⤷ glyph must not render"
        );
        assert!(message.content.starts_with("bug_report · moon\n"));

        let encoded = serde_json::to_string(&message).expect("serialize peer transcript row");
        let reloaded: ChatMessage =
            serde_json::from_str(&encoded).expect("reload peer transcript row");
        assert!(reloaded.content.contains(raw));
        assert_eq!(reloaded.content_blocks, vec![ContentBlockType::PeerText]);
        let unknown = peer_transcript_message(
            "payload",
            &peer,
            &crate::domain::models::InteractionPolicySnapshot {
                sender_label: Some("moon".to_owned()),
                ..Default::default()
            },
        );
        assert!(
            unknown.content.starts_with("unknown type · moon\n"),
            "the unknown type uses the registry's fixed safe disclosure"
        );
    }

    async fn wait_for_terminal(status: &mut tokio::sync::watch::Receiver<NodeState>) -> NodeState {
        loop {
            let state = *status.borrow_and_update();
            if state.is_terminal() {
                return state;
            }
            status
                .changed()
                .await
                .expect("node sender remains alive until its terminal transition");
        }
    }

    #[tokio::test]
    async fn notify_and_wait_transitions_then_stamps_and_resume_clears() {
        let tmp = tempfile::tempdir().unwrap();
        let (core, _storage) = mock_core(tmp.path(), vec![]);
        let conversation = Arc::new(Mutex::new(Conversation::default()));
        let (domain_tx, _domain_rx) = mpsc::unbounded_channel();
        let server = AttachServer::new(core, conversation, domain_tx);
        let mut task = inbound_task(test_signer(90).identity().peer_id.clone(), "status?");
        task.response_policy = crate::domain::ports::PeerResponsePolicy::default();
        let node_id = task.node_id.clone();

        let status = server
            .start(task, CancellationToken::new())
            .await
            .expect("wait mode registers a durable peer node");
        assert_eq!(*status.borrow(), NodeState::Waiting);
        let waiting = server
            .node_tree()
            .list()
            .await
            .into_iter()
            .find(|node| node.agent_id == node_id)
            .expect("registered waiting node");
        assert_eq!(
            waiting.wait_reason,
            Some(crate::domain::models::WaitReason::AwaitingPeerResponse)
        );

        server
            .node_tree()
            .set_state(&node_id, NodeState::Running)
            .await;
        let resumed = server
            .node_tree()
            .list()
            .await
            .into_iter()
            .find(|node| node.agent_id == node_id)
            .expect("resumed node");
        assert_eq!(resumed.wait_reason, None);

        // Restart reconciliation may discover that a peer task which was
        // persisted as waiting is actually terminal. The terminal transition
        // must clear the replayed side-state rather than render a false wait.
        server
            .node_tree()
            .set_state(&node_id, NodeState::Waiting)
            .await;
        server
            .node_tree()
            .stamp_wait_reason(
                &node_id,
                Some(crate::domain::models::WaitReason::AwaitingPeerResponse),
            )
            .await
            .expect("replayed peer wait");
        server
            .node_tree()
            .set_state(&node_id, NodeState::Running)
            .await;
        server
            .node_tree()
            .set_state(&node_id, NodeState::Failed)
            .await;
        let reconciled = server
            .node_tree()
            .list()
            .await
            .into_iter()
            .find(|node| node.agent_id == node_id)
            .expect("reconciled node");
        assert_eq!(reconciled.wait_reason, None);
    }

    #[tokio::test]
    async fn notify_and_draft_buffers_provider_output_until_resolution() {
        let tmp = tempfile::tempdir().unwrap();
        let (server, conversation, _journal, shutdown, handle) = spawn_inbound_forwarder(
            tmp.path(),
            vec![
                StreamChunk::Text {
                    content: "private draft".to_owned(),
                    parent_tool_use_id: None,
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                },
            ],
        )
        .await;
        let mut client = UnixStream::connect(tmp.path().join("inbound-forwarder.sock"))
            .await
            .unwrap();
        assert!(matches!(
            attach_with_proof(
                &mut client,
                false,
                ConnectionTier::TrustedLocal,
                &test_signer(90)
            )
            .await,
            Some(DaemonFrame::AttachAck {
                granted_mode: AttachMode::ReadWrite,
                ..
            })
        ));
        let mut task = inbound_task(test_signer(91).identity().peer_id.clone(), "draft this");
        task.response_policy.mode = crate::domain::models::ResponseMode::NotifyAndDraft;
        let node_id = task.node_id.clone();

        let mut status = server
            .start(task, CancellationToken::new())
            .await
            .expect("draft mode starts inference");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while *status.borrow_and_update() != NodeState::Waiting {
                status
                    .changed()
                    .await
                    .expect("draft node remains registered");
            }
        })
        .await
        .expect("completed draft parks for operator resolution");

        assert_eq!(
            server.pending_drafts.state(node_id.as_str()).await,
            Some(crate::adapters::daemon::response_modes::DraftState::Ready {
                content: "private draft".to_owned(),
            })
        );
        assert!(
            server.inbound_results.lock().await.get(&node_id).is_none(),
            "an unresolved draft must never enter the peer-disclosable result map"
        );
        write_frame(
            &mut client,
            &ClientFrame::DeferRecipientItem {
                item_id: "ri_stream_probe".to_owned(),
            },
        )
        .await
        .unwrap();
        loop {
            let frame = read_frame::<_, DaemonFrame>(&mut client).await.unwrap();
            assert!(
                !matches!(
                    &frame,
                    Some(DaemonFrame::Event(RawEvent {
                        kind: RawEventKind::Provider(StreamChunk::Text { .. }),
                        ..
                    }))
                ),
                "draft provider text must remain buffered"
            );
            if matches!(frame, Some(DaemonFrame::RecipientActionResult {
                target: super::super::protocol::RecipientActionTarget::Item { item_id }, ..
            }) if item_id == "ri_stream_probe")
            {
                break;
            }
        }
        let draft_row = conversation
            .lock()
            .await
            .messages
            .iter()
            .find(|message| message.id == format!("peer-response-{}", node_id.as_str()))
            .cloned()
            .expect("draft placeholder becomes the pending local draft row");
        assert!(
            draft_row
                .content
                .starts_with("[y] Approve  [e] Edit  [n] Reject")
        );
        assert!(draft_row.content.ends_with("private draft"));
        shutdown.cancel();
        drop(client);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn notify_and_auto_fixed_response_is_immediate_persisted_and_marked() {
        let tmp = tempfile::tempdir().unwrap();
        let (core, storage) = mock_core(tmp.path(), vec![]);
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "notify-and-auto-fixed".to_owned(),
            ..Default::default()
        }));
        let (domain_tx, _domain_rx) = mpsc::unbounded_channel();
        let server = AttachServer::new(core, conversation.clone(), domain_tx);
        let mut task = inbound_task(test_signer(92).identity().peer_id.clone(), "status?");
        task.response_policy = crate::domain::ports::PeerResponsePolicy {
            mode: crate::domain::models::ResponseMode::NotifyAndAuto,
            auto_response: Some("Acknowledged.".to_owned()),
            ..Default::default()
        };
        let node_id = task.node_id.clone();

        let status = server
            .start(task, CancellationToken::new())
            .await
            .expect("fixed auto response completes at admission");
        assert_eq!(*status.borrow(), NodeState::Completed);
        assert_eq!(
            server
                .take_result(&node_id)
                .await
                .map(|result| result.text)
                .as_deref(),
            Some("Acknowledged.")
        );
        let row = conversation
            .lock()
            .await
            .messages
            .last()
            .cloned()
            .expect("auto response persisted locally");
        assert_eq!(row.content, "Acknowledged.");
        assert_eq!(
            row.id,
            format!("peer-response-{}", node_id.as_str()),
            "the retract path must be able to address the template fast-path row (AC5)"
        );
        assert_eq!(
            row.authorship,
            crate::domain::models::Authorship::AgentComposed
        );
        assert_eq!(
            row.send_authorization,
            crate::domain::models::SendAuthorization::Automatic
        );
        assert!(
            !conversation.lock().await.messages.iter().any(|message| {
                message.content == crate::adapters::daemon::response_modes::DRAFTING_PLACEHOLDER
            }),
            "the template fast path must never emit a drafting placeholder"
        );
        let reloaded = storage
            .load_conversation("notify-and-auto-fixed")
            .await
            .expect("auto response reload succeeds")
            .expect("auto response persists on disk");
        let reloaded_row = reloaded
            .messages
            .iter()
            .find(|message| message.id == format!("peer-response-{}", node_id.as_str()))
            .expect("persisted auto response remains addressable");
        assert_eq!(
            reloaded_row.authorship,
            crate::domain::models::Authorship::AgentComposed,
            "agent authorship must persist on the auto response"
        );
    }

    /// Supersedes 19.20a K01r: RAP has no reply rail, so no response is composed.
    #[tokio::test]
    async fn rap_ingest_is_notify_only_in_every_response_mode() {
        use crate::adapters::rap::VerifiedPeerConsumer as _;
        use crate::adapters::tui::{state::TabRenderState, theme::Theme, widgets::chat_pane};
        use crate::domain::models::ResponseMode;
        use crate::domain::models::StreamingState;
        use crate::domain::ports::TaskNodes as _;
        use ratatui::{Terminal, backend::TestBackend};
        for mode in [
            ResponseMode::NotifyAndWait,
            ResponseMode::NotifyAndAuto,
            ResponseMode::NotifyAndDraft,
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let provider = Arc::new(ToolThenTextProvider {
                calls: AtomicUsize::new(0),
            });
            let (core, storage) = mock_core_with_provider(tmp.path(), provider.clone());
            let (domain_tx, _domain_rx) = mpsc::unbounded_channel();
            let conversation = Arc::new(Mutex::new(Conversation {
                id: "rap-one-way".into(),
                ..Default::default()
            }));
            let server = Arc::new(AttachServer::new(core, conversation, domain_tx));
            let peer = test_signer(92).identity().peer_id.clone();
            let node = AgentId::from_peer_path(&format!("{peer}/inbox")).unwrap();
            let _handle = server
                .node_tree
                .register_task_node(&node, "remote-peer")
                .await
                .unwrap();
            let before = server.node_tree.delivery_target(&node).await.unwrap().state;
            let policy = crate::domain::ports::PeerResponsePolicy {
                mode,
                auto_response: Some("RAP_UNSENT_BODY_END".into()),
                ..Default::default()
            };
            DaemonPeerConsumer::new(Arc::downgrade(&server))
                .ingest_with_policy(
                    &node,
                    AgentMessage::new("RAP question"),
                    &peer,
                    policy.clone(),
                )
                .await
                .unwrap();
            assert!(
                !server.core.is_runtime_initialized(),
                "RAP must not initialize a runtime"
            );
            let reloaded = storage
                .load_conversation("rap-one-way")
                .await
                .unwrap()
                .unwrap();
            let expected = format!(
                "[no reply over RAP] RAP carries messages one way: nothing can be sent back, so your {} response setting does not apply here.",
                mode.as_str()
            );
            assert_eq!(reloaded.messages.len(), 2);
            assert_eq!(reloaded.messages[1].content, expected);
            let row = &reloaded.messages[1];
            assert!(!row.id.is_empty() && !row.id.starts_with("peer-response-"));
            assert_eq!(row.role, MessageRole::Assistant);
            assert!(row.synthetic);
            assert_eq!(row.authorship, ChatMessage::default().authorship);
            assert_eq!(
                row.send_authorization,
                ChatMessage::default().send_authorization
            );
            assert_eq!(
                reloaded.peer_row_controls[&row.id],
                peer_row_control(
                    &node,
                    None,
                    &policy.provenance,
                    peer.as_str(),
                    PeerRowStage::Settled,
                    false
                )
            );
            let transcript = &reloaded.messages[0];
            assert_eq!(transcript.role, MessageRole::User);
            assert!(!transcript.synthetic);
            assert!(transcript.content.contains("RAP question"));
            assert!(
                transcript
                    .content
                    .contains(&policy.provenance.response_clause())
            );
            assert!(
                transcript
                    .content
                    .contains(&policy.provenance.notification_clause())
            );
            assert_eq!(reloaded.peer_row_revision, 2);
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            let mut cache = TabRenderState::default();
            terminal
                .draw(|frame| {
                    chat_pane::render_with_search(
                        frame,
                        frame.area(),
                        &reloaded,
                        None,
                        &StreamingState::default(),
                        &Default::default(),
                        &crate::domain::clock::MockClock::at_wall_ms(1_700_000_005_000),
                        0,
                        false,
                        &Theme::dark(),
                        &mut cache,
                        &Default::default(),
                        &Default::default(),
                        None,
                        None,
                        &[],
                        &[],
                        None,
                        None,
                        None,
                        None,
                        None,
                    );
                })
                .unwrap();
            let painted = terminal
                .backend()
                .buffer()
                .content
                .chunks(80)
                .map(|line| line.iter().map(|cell| cell.symbol()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n");
            assert!(painted.contains("[no reply over RAP]"));
            let visible = painted
                .lines()
                .flat_map(|line| line.trim_start_matches('│').split_whitespace())
                .collect::<Vec<_>>()
                .join(" ");
            assert!(visible.contains(&expected), "{painted}");
            for absent in [
                "[agent-composed]",
                "[auto-sent]",
                "RAP_UNSENT_BODY_END",
                "[awaiting your response]",
                "[drafting...]",
            ] {
                assert!(!painted.contains(absent), "{painted}");
            }
            assert!(server.inbound_results.lock().await.is_empty());
            assert!(server.pending_drafts.phase(node.as_str()).await.is_none());
            assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
            let after = server.node_tree.delivery_target(&node).await.unwrap().state;
            assert_eq!(before, after);
            assert_ne!(
                crate::domain::models::agent_message::delivery_decision(after),
                crate::domain::models::agent_message::DeliveryMode::Refuse
            );
            assert!(
                server
                    .node_tree
                    .list()
                    .await
                    .iter()
                    .find(|entry| entry.agent_id == node)
                    .unwrap()
                    .wait_reason
                    .is_none()
            );
            let runtime = server.core.ensure_runtime().await.unwrap();
            let mut approvals = runtime.approval.subscribe();
            let turn_guard = server.turn_serial.lock().await;
            tokio::time::timeout(
                std::time::Duration::from_secs(3),
                DaemonPeerConsumer::new(Arc::downgrade(&server)).ingest_with_policy(
                    &node,
                    AgentMessage::new("second RAP question"),
                    &peer,
                    policy,
                ),
            )
            .await
            .unwrap()
            .unwrap();
            drop(turn_guard);
            let second = storage
                .load_conversation("rap-one-way")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(second.messages.len(), 4);
            assert_eq!(second.messages[3].content, expected);
            assert_eq!(
                server.node_tree.delivery_target(&node).await.unwrap().state,
                before
            );
            assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
            assert!(matches!(
                approvals.try_recv(),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            ));
            assert!(server.inbound_results.lock().await.is_empty());
            assert!(server.pending_drafts.phase(node.as_str()).await.is_none());
            let mut task = inbound_task(peer, "A2A control");
            task.response_policy = crate::domain::ports::PeerResponsePolicy {
                mode: ResponseMode::NotifyAndAuto,
                auto_response: Some("A2A template".into()),
                ..Default::default()
            };
            let task_node = task.node_id.clone();
            server.start(task, CancellationToken::new()).await.unwrap();
            assert_eq!(
                server.take_result(&task_node).await.unwrap().text,
                "A2A template"
            );
        }
    }

    #[tokio::test]
    async fn legacy_rap_card_refuses_resolution_and_settles() {
        use super::super::protocol::PeerDraftAction;
        use super::super::response_modes::{AWAITING_RESPONSE_PLACEHOLDER, DRAFT_APPROVAL_PREFIX};
        for live_controller in [false, true] {
            for (stage, action) in [
                (
                    PeerRowStage::AwaitingOperator,
                    PeerDraftAction::WriteOwn {
                        content: "mine".into(),
                    },
                ),
                (PeerRowStage::AwaitingOperator, PeerDraftAction::Reject),
                (PeerRowStage::Ready, PeerDraftAction::Approve),
                (
                    PeerRowStage::Ready,
                    PeerDraftAction::Edit {
                        content: "edited".into(),
                    },
                ),
            ] {
                let tmp = tempfile::tempdir().unwrap();
                let peer = test_signer(92).identity().peer_id.clone();
                let node = AgentId::from_peer_path(&format!("{peer}/inbox")).unwrap();
                let row_id = format!("peer-response-{node}");
                let mode = if stage == PeerRowStage::Ready {
                    crate::domain::models::ResponseMode::NotifyAndDraft
                } else {
                    crate::domain::models::ResponseMode::NotifyAndWait
                };
                let (core, storage) = mock_core(tmp.path(), vec![]);
                let mut conversation = Conversation {
                    id: "legacy-rap".into(),
                    ..Default::default()
                };
                conversation.messages.push(ChatMessage {
                    id: row_id.clone(),
                    role: MessageRole::Assistant,
                    synthetic: true,
                    content: if stage == PeerRowStage::Ready {
                        format!("{DRAFT_APPROVAL_PREFIX}draft")
                    } else {
                        AWAITING_RESPONSE_PLACEHOLDER.into()
                    },
                    ..Default::default()
                });
                conversation.peer_row_controls.insert(
                    row_id.clone(),
                    peer_row_control(&node, None, &Default::default(), peer.as_str(), stage, true),
                );
                storage.save_conversation(&conversation).await.unwrap();
                let conversation = Arc::new(Mutex::new(
                    storage
                        .load_conversation("legacy-rap")
                        .await
                        .unwrap()
                        .unwrap(),
                ));
                let (domain_tx, domain_rx) = mpsc::unbounded_channel();
                let journal = Arc::new(RecordingJournal::default());
                let server =
                    journaled_server(core, conversation.clone(), domain_tx, journal.clone());
                if live_controller {
                    assert!(server.pending_drafts.begin(node.as_str()).await);
                    assert!(
                        server
                            .pending_drafts
                            .complete(node.as_str(), "legacy draft")
                            .await
                    );
                }
                let socket = tmp.path().join("legacy.sock");
                let listener = UnixListener::bind(&socket).unwrap();
                let shutdown = CancellationToken::new();
                let service = {
                    let server = server.clone();
                    let shutdown = shutdown.clone();
                    tokio::spawn(async move {
                        server.run(listener, domain_rx, None, None, shutdown).await;
                    })
                };
                let mut client = UnixStream::connect(&socket).await.unwrap();
                let ack = attach_with_proof(
                    &mut client,
                    false,
                    ConnectionTier::TrustedLocal,
                    &test_signer(94),
                )
                .await;
                assert!(matches!(
                    ack,
                    Some(DaemonFrame::AttachAck {
                        granted_mode: AttachMode::ReadWrite,
                        ..
                    })
                ));
                for attempt in 0..2 {
                    write_frame(
                        &mut client,
                        &ClientFrame::ResolvePeerDraft {
                            node: node.to_string(),
                            action: action.clone(),
                        },
                    )
                    .await
                    .unwrap();
                    let outcome = tokio::time::timeout(std::time::Duration::from_secs(3), async {
                        loop {
                            if let Some(DaemonFrame::RecipientActionResult { outcome, .. }) =
                                read_frame::<_, DaemonFrame>(&mut client).await.unwrap()
                            {
                                break outcome;
                            }
                        }
                    })
                    .await
                    .unwrap();
                    let expected = if attempt == 0 {
                        RecipientActionOutcome::Refused {
                            error: ProtocolError::PeerVerification(
                                "RAP carries messages one way; no reply was sent.".into(),
                            ),
                        }
                    } else {
                        RecipientActionOutcome::Idempotent
                    };
                    assert_eq!(outcome, expected);
                }
                let persisted = storage
                    .load_conversation("legacy-rap")
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    persisted.messages[0].content,
                    format!(
                        "[no reply over RAP] RAP carries messages one way: nothing can be sent back, so your {} response setting does not apply here.",
                        mode.as_str()
                    )
                );
                let controls = snapshot_peer_row_controls(&persisted, &server.pending_drafts).await;
                assert_eq!(controls[&row_id].stage, PeerRowStage::Settled);
                assert!(!controls[&row_id].node_resolvable);
                assert_eq!(persisted.peer_row_controls[&row_id], controls[&row_id]);
                assert!(server.inbound_results.lock().await.is_empty());
                assert!(server.pending_drafts.phase(node.as_str()).await.is_none());
                assert!(journal.events.lock().await.is_empty());
                shutdown.cancel();
                drop(client);
                service.await.unwrap();
            }
        }
        #[cfg(feature = "a2a")]
        {
            use super::super::response_modes::DraftResolution;
            let tmp = tempfile::tempdir().unwrap();
            let (server, journal, node) =
                reply_card_fixture(tmp.path(), &format!("{DRAFT_APPROVAL_PREFIX}A2A draft")).await;
            assert!(rap_rooted_peer(&node).is_none());
            assert!(server.pending_drafts.begin(node.as_str()).await);
            assert!(
                server
                    .pending_drafts
                    .complete(node.as_str(), "A2A draft")
                    .await
            );
            assert!(matches!(
                server
                    .resolve_peer_draft(&node, DraftResolution::Approve)
                    .await
                    .unwrap(),
                PeerDraftResolutionOutcome::Applied { .. }
            ));
            assert_eq!(server.take_result(&node).await.unwrap().text, "A2A draft");
            assert!(journal.events.lock().await.iter().any(|event| matches!(
                event,
                crate::domain::models::RoomEvent::PeerDraftResolved { .. }
            )));
        }
    }

    #[tokio::test]
    async fn notify_and_auto_without_override_dispatches_the_conservative_template() {
        let tmp = tempfile::tempdir().unwrap();
        // No scripted chunks: any provider call would produce nothing, so a
        // completed response here proves no inference ran (AC4 — templates,
        // never LLM text).
        let (core, storage) = mock_core(tmp.path(), vec![]);
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "notify-and-auto-template".to_owned(),
            ..Default::default()
        }));
        let (domain_tx, mut domain_rx) = mpsc::unbounded_channel();
        let server = AttachServer::new(core, conversation.clone(), domain_tx);
        let task = inbound_task(test_signer(93).identity().peer_id.clone(), "status?");
        let node_id = task.node_id.clone();

        let status = server
            .start(task, CancellationToken::new())
            .await
            .expect("template auto response completes at admission");
        assert_eq!(*status.borrow(), NodeState::Completed);
        assert_eq!(
            server
                .take_result(&node_id)
                .await
                .map(|result| result.text)
                .as_deref(),
            Some(crate::adapters::daemon::response_modes::DEFAULT_AUTO_RESPONSE_TEMPLATE)
        );
        assert!(
            !std::iter::from_fn(|| domain_rx.try_recv().ok())
                .any(|event| matches!(event, AppEvent::ProviderChunk { .. })),
            "a template dispatch drives no provider turn"
        );
        let row = conversation
            .lock()
            .await
            .messages
            .iter()
            .find(|message| message.id == format!("peer-response-{}", node_id.as_str()))
            .cloned()
            .expect("template response persisted with an addressable id");
        assert_eq!(
            row.content,
            crate::adapters::daemon::response_modes::DEFAULT_AUTO_RESPONSE_TEMPLATE
        );
        assert_eq!(
            row.authorship,
            crate::domain::models::Authorship::AgentComposed
        );
        assert!(
            !conversation.lock().await.messages.iter().any(|message| {
                message.content == crate::adapters::daemon::response_modes::DRAFTING_PLACEHOLDER
            }),
            "the template fast path must never emit a drafting placeholder"
        );
        let reloaded = storage
            .load_conversation("notify-and-auto-template")
            .await
            .expect("template auto response reload succeeds")
            .expect("template auto response persists on disk");
        let reloaded_row = reloaded
            .messages
            .iter()
            .find(|message| message.id == format!("peer-response-{}", node_id.as_str()))
            .expect("persisted template auto response remains addressable");
        assert_eq!(
            reloaded_row.authorship,
            crate::domain::models::Authorship::AgentComposed,
            "agent authorship must persist on the template auto response"
        );
    }

    #[derive(Default)]
    pub(super) struct RecordingJournal {
        pub(super) events: Mutex<Vec<crate::domain::models::RoomEvent>>,
        fail: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl crate::domain::ports::RoomJournal for RecordingJournal {
        async fn record_event(
            &self,
            event: crate::domain::models::RoomEvent,
        ) -> Result<(), crate::domain::ports::RoomJournalError> {
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(crate::domain::ports::RoomJournalError::Append(
                    "disk full".to_owned(),
                ));
            }
            self.events.lock().await.push(event);
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl crate::domain::ports::RoomJournalReader for RecordingJournal {
        async fn load_entries(
            &self,
        ) -> Result<Vec<crate::domain::models::JournalEntry>, crate::domain::ports::RoomJournalError>
        {
            Ok(self
                .events
                .lock()
                .await
                .iter()
                .enumerate()
                .map(|(index, event)| {
                    crate::domain::models::JournalEntry::new(
                        (index + 1) as u64,
                        crate::domain::models::JournalRecord::Room(event.clone()),
                        0,
                    )
                })
                .collect())
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn journaled_server(
        core: Arc<DaemonCore>,
        conversation: Arc<Mutex<Conversation>>,
        domain_tx: mpsc::UnboundedSender<AppEvent>,
        journal: Arc<RecordingJournal>,
    ) -> Arc<AttachServer> {
        journaled_server_with_projection(
            core,
            conversation,
            domain_tx,
            journal,
            Arc::new(crate::adapters::policy::JournalConsentProjection::default()),
        )
    }

    fn journaled_server_with_projection(
        core: Arc<DaemonCore>,
        conversation: Arc<Mutex<Conversation>>,
        domain_tx: mpsc::UnboundedSender<AppEvent>,
        journal: Arc<RecordingJournal>,
        consent_projection: Arc<crate::adapters::policy::JournalConsentProjection>,
    ) -> Arc<AttachServer> {
        let node_tree = crate::infrastructure::subagent::NodeTree::with_event_tx(
            domain_tx.clone(),
            Arc::new(|| 123_i64),
        );
        let delivery_policy: Arc<dyn crate::domain::ports::DeliveryPolicy> =
            Arc::new(crate::domain::ports::RelationshipDeliveryPolicy);
        let peer_bus = peer_bus_slot_with_policy(&node_tree, delivery_policy.clone());
        AttachServer::new_with_node_tree_bus_policy_and_journal(
            core,
            conversation,
            domain_tx,
            node_tree,
            peer_bus,
            delivery_policy,
            journal.clone(),
            journal,
            Some(consent_projection),
        )
    }

    fn journaled_server_with_team_hooks(
        core: Arc<DaemonCore>,
        conversation: Arc<Mutex<Conversation>>,
        domain_tx: mpsc::UnboundedSender<AppEvent>,
        journal: Arc<RecordingJournal>,
        hooks: TeamSendTestHooks,
    ) -> Arc<AttachServer> {
        let node_tree = crate::infrastructure::subagent::NodeTree::with_event_tx(
            domain_tx.clone(),
            Arc::new(|| 123_i64),
        );
        let delivery_policy: Arc<dyn crate::domain::ports::DeliveryPolicy> =
            Arc::new(crate::domain::ports::RelationshipDeliveryPolicy);
        let peer_bus = peer_bus_slot_with_policy(&node_tree, delivery_policy.clone());
        AttachServer::new_with_node_tree_bus_policy_journal_and_team(
            core,
            conversation,
            domain_tx,
            node_tree,
            peer_bus,
            delivery_policy,
            journal.clone(),
            journal.clone(),
            Some(Arc::new(
                crate::adapters::policy::JournalConsentProjection::default(),
            )),
            #[cfg(feature = "a2a")]
            None,
            Vec::new(),
            Arc::new(super::super::team_proposal::TeamProposalRegistry::new()),
            Some(hooks),
        )
    }

    fn journaled_server_with_urgency(
        core: Arc<DaemonCore>,
        conversation: Arc<Mutex<Conversation>>,
        domain_tx: mpsc::UnboundedSender<AppEvent>,
        journal: Arc<RecordingJournal>,
        urgency: Arc<crate::adapters::daemon::urgency::UrgencyRouter>,
    ) -> Arc<AttachServer> {
        let node_tree = crate::infrastructure::subagent::NodeTree::with_event_tx(
            domain_tx.clone(),
            Arc::new(|| 123_i64),
        );
        let delivery_policy: Arc<dyn crate::domain::ports::DeliveryPolicy> =
            Arc::new(crate::domain::ports::RelationshipDeliveryPolicy);
        let peer_bus = peer_bus_slot_with_policy(&node_tree, delivery_policy.clone());
        AttachServer::new_with_node_tree_bus_policy_journal_and_urgency(
            core,
            conversation,
            domain_tx,
            node_tree,
            peer_bus,
            delivery_policy,
            journal.clone(),
            journal,
            None,
            Some(urgency),
        )
    }

    #[tokio::test]
    async fn acknowledge_frame_appends_then_folds_the_deliberate_human_act() {
        let tmp = tempfile::tempdir().unwrap();
        let (core, _storage) = mock_core(tmp.path(), vec![]);
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "recipient-ack".to_owned(),
            ..Default::default()
        }));
        let (domain_tx, _domain_rx) = mpsc::unbounded_channel();
        let journal = Arc::new(RecordingJournal::default());
        let address = crate::domain::models::ItemAddress::from_a2a_ingress(
            test_signer(111).identity().peer_id.clone(),
            crate::domain::models::ItemId::from_replay("ri_ack_front_door"),
        );
        journal
            .record_event(crate::domain::models::RoomEvent::RecipientItemReceived {
                address: address.clone(),
                task: "sender-task".to_owned(),
                alias: None,
                message_type: crate::domain::models::SemanticMessageType::Unknown,
                content: "review".to_owned(),
                in_reply_to: None,
                capability_hash: None,
                reply_capability_matched: false,
                authorship: crate::domain::models::Authorship::Unknown,
                send_authorization: crate::domain::models::SendAuthorization::Unknown,
            })
            .await
            .unwrap();
        let server = journaled_server(core, conversation, domain_tx, journal.clone());
        let (writer_tx, mut writer_rx) = mpsc::channel(4);
        server.registry.lock().await.conns.push(Conn {
            id: 1,
            tx: writer_tx,
            terminal_tx: watch::channel(None).0,
            mode: AttachMode::ReadWrite,
            tier: ConnectionTier::TrustedLocal,
            approval_ready: true,
        });

        server
            .handle_client_frame_tiered(
                ClientFrame::AcknowledgeRecipientItem {
                    item_id: "ri_ack_front_door".to_owned(),
                },
                AttachMode::ReadWrite,
                ConnectionTier::TrustedLocal,
                1,
            )
            .await;
        assert!(matches!(
            writer_rx.try_recv().unwrap(),
            DaemonFrame::RecipientActionResult {
                action: RecipientAction::Acknowledge,
                target: RecipientActionTarget::Item { item_id },
                outcome: RecipientActionOutcome::Applied { reply: None },
            } if item_id == "ri_ack_front_door"
        ));

        let entries = crate::domain::ports::RoomJournalReader::load_entries(journal.as_ref())
            .await
            .unwrap();
        let Some(crate::domain::models::JournalRecord::Room(
            crate::domain::models::RoomEvent::RecipientItemAcknowledged {
                address: acked_address,
                alias: acked_alias,
            },
        )) = entries.last().map(|entry| &entry.record)
        else {
            panic!("the acknowledge frame must journal RecipientItemAcknowledged");
        };
        assert_eq!(acked_address, &address);
        assert_eq!(
            acked_alias, &None,
            "attribution is the arrival alias, never a fabricated operator string"
        );
        crate::adapters::policy::recipient_item::reset_recipient_item_transition_count();
        let projection =
            crate::adapters::policy::JournalRecipientItemProjection::from_entries(&entries);
        assert!(matches!(
            projection.get(&address).unwrap().state,
            crate::domain::models::RecipientItemState::Acknowledged { .. }
        ));
        assert_eq!(
            crate::adapters::policy::recipient_item::recipient_item_transition_count(),
            1,
            "one acknowledgement command must produce one state transition"
        );
    }

    /// Story 19.16c — the one fixture every removal keystone seeds from: a
    /// journaled server, a **registered writer connection** so the refusal
    /// frames are observable (the seam a real client would occupy), and one
    /// recipient item already received.
    async fn recipient_item_fixture(
        tmp: &std::path::Path,
        seeds: &[(&str, Option<&str>, &str)],
    ) -> (
        Arc<AttachServer>,
        Arc<RecordingJournal>,
        Vec<crate::domain::models::ItemAddress>,
        mpsc::Receiver<DaemonFrame>,
    ) {
        let (core, _storage) = mock_core(tmp, vec![]);
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "recipient-removal".to_owned(),
            ..Default::default()
        }));
        let (domain_tx, _domain_rx) = mpsc::unbounded_channel();
        let journal = Arc::new(RecordingJournal::default());
        let mut addresses = Vec::new();
        for (index, (item_id, alias, content)) in seeds.iter().enumerate() {
            let address = crate::domain::models::ItemAddress::from_a2a_ingress(
                test_signer(150 + u8::try_from(index).unwrap())
                    .identity()
                    .peer_id
                    .clone(),
                crate::domain::models::ItemId::from_replay(*item_id),
            );
            journal
                .record_event(crate::domain::models::RoomEvent::RecipientItemReceived {
                    address: address.clone(),
                    task: format!("{item_id}-sender-task"),
                    message_type: crate::domain::models::SemanticMessageType::Unknown,
                    alias: alias.map(ToOwned::to_owned),
                    content: (*content).to_owned(),
                    in_reply_to: None,
                    capability_hash: None,
                    reply_capability_matched: false,
                    authorship: crate::domain::models::Authorship::Unknown,
                    send_authorization: crate::domain::models::SendAuthorization::Unknown,
                })
                .await
                .unwrap();
            addresses.push(address);
        }
        let server = journaled_server(core, conversation, domain_tx, journal.clone());
        let (writer_tx, writer_rx) = mpsc::channel(8);
        server.registry.lock().await.conns.push(Conn {
            id: 1,
            tx: writer_tx,
            terminal_tx: watch::channel(None).0,
            mode: AttachMode::ReadWrite,
            tier: ConnectionTier::TrustedLocal,
            approval_ready: true,
        });
        (server, journal, addresses, writer_rx)
    }

    fn refusal_text(frame: Option<DaemonFrame>) -> String {
        let error = match frame {
            Some(DaemonFrame::Error(error)) => error,
            Some(DaemonFrame::RecipientActionResult {
                outcome: RecipientActionOutcome::Refused { error },
                ..
            }) => error,
            other => panic!("expected a refusal frame, got {other:?}"),
        };
        match error {
            ProtocolError::Malformed(message) | ProtocolError::PeerVerification(message) => message,
            ProtocolError::ReadOnly => "read-only".to_owned(),
            ProtocolError::Internal(message) => message,
            other => panic!("unexpected refusal kind: {other:?}"),
        }
    }

    async fn recipient_items(
        journal: &RecordingJournal,
    ) -> crate::adapters::policy::JournalRecipientItemProjection {
        let entries = crate::domain::ports::RoomJournalReader::load_entries(journal)
            .await
            .unwrap();
        crate::adapters::policy::JournalRecipientItemProjection::from_entries(&entries)
    }

    /// Story 19.16c AC1(a)(b)(e) — the removal verb's front door: one gated
    /// frame, **one** durable record, and a tombstone the fold KEEPS.
    ///
    /// Mutant → RED: implement removal as `HashMap::remove`. The entry
    /// vanishes, `find_by_id` answers `None`, and the removed id becomes
    /// byte-identical to an id that never existed.
    #[tokio::test]
    async fn removal_frame_appends_a_record_and_the_fold_keeps_the_tombstone() {
        let tmp = tempfile::tempdir().unwrap();
        let (server, journal, addresses, mut frames) = recipient_item_fixture(
            tmp.path(),
            &[
                ("ri_removed", None, "peer content"),
                ("ri_untouched", None, "still here"),
            ],
        )
        .await;

        server
            .handle_client_frame_tiered(
                ClientFrame::RemoveRecipientItem {
                    item_id: "ri_removed".to_owned(),
                },
                AttachMode::ReadWrite,
                ConnectionTier::TrustedLocal,
                1,
            )
            .await;

        let entries = crate::domain::ports::RoomJournalReader::load_entries(journal.as_ref())
            .await
            .unwrap();
        let Some(crate::domain::models::JournalRecord::Room(
            crate::domain::models::RoomEvent::RecipientItemRemoved { address },
        )) = entries.last().map(|entry| &entry.record)
        else {
            panic!("the removal frame must journal RecipientItemRemoved");
        };
        assert_eq!(address, &addresses[0]);

        let projection = recipient_items(journal.as_ref()).await;
        let tombstone = projection
            .find_by_id("ri_removed")
            .expect("the projection RETAINS the entry — a tombstone, not an absence");
        assert!(matches!(
            tombstone.state,
            crate::domain::models::RecipientItemState::Removed { .. }
        ));

        // Positive control: removal is scoped to its target.
        let untouched = projection.find_by_id("ri_untouched").expect("still there");
        assert!(matches!(
            untouched.state,
            crate::domain::models::RecipientItemState::Received { content, .. } if content == "still here"
        ));
        assert!(frames.try_recv().is_err(), "a legal removal remains silent");
    }

    /// AC1(b)(c)(d)(g) + the distinctness ratchet — in ONE run, a **removed**
    /// id and a **never-minted** id must refuse differently, byte for byte. A
    /// one-sided assertion cannot see the collapse.
    ///
    /// Mutants → RED: `HashMap::remove` (both refusals become "not found");
    /// a silent no-op refusal (no frame at all).
    #[tokio::test]
    async fn a_removed_id_and_an_unknown_id_refuse_differently() {
        let tmp = tempfile::tempdir().unwrap();
        let (server, journal, _addresses, mut frames) =
            recipient_item_fixture(tmp.path(), &[("ri_twice", None, "peer content")]).await;

        for item_id in ["ri_twice", "ri_twice", "ri_never_minted"] {
            server
                .handle_client_frame_tiered(
                    ClientFrame::RemoveRecipientItem {
                        item_id: item_id.to_owned(),
                    },
                    AttachMode::ReadWrite,
                    ConnectionTier::TrustedLocal,
                    1,
                )
                .await;
        }

        let already_removed = refusal_text(frames.try_recv().ok());
        let never_existed = refusal_text(frames.try_recv().ok());
        assert_eq!(already_removed, "recipient item already removed");
        assert_eq!(never_existed, "recipient item not found");
        assert_ne!(
            already_removed, never_existed,
            "a tombstone the operator cannot tell from a never-minted id is the \
             collapse AD-1822 forbids"
        );

        let entries = crate::domain::ports::RoomJournalReader::load_entries(journal.as_ref())
            .await
            .unwrap();
        assert_eq!(
            entries
                .iter()
                .filter(|entry| matches!(
                    entry.record,
                    crate::domain::models::JournalRecord::Room(
                        crate::domain::models::RoomEvent::RecipientItemRemoved { .. }
                    )
                ))
                .count(),
            1,
            "idempotent-by-refusal: the second removal writes nothing"
        );
    }

    /// AC1(c) — **the headline.** `/team remove ri_x` then `/team ack ri_x`:
    /// two commands, no concurrency. The shipped `!=` idempotence guard let
    /// the acknowledgement through, journaling a line that a cold re-fold
    /// replays forever.
    ///
    /// Mutant → RED: keep the `!= <state>` guard in the frame handler instead
    /// of routing through `resolve_item_for_edit`'s legality check. ⛔ It does
    /// NOT assert `state == Removed && content == None`: the fold's own table
    /// holds those under the mutant. What moves is a spurious journal line and
    /// a refusal the operator never hears.
    #[tokio::test]
    async fn acknowledging_a_removed_item_journals_nothing_and_is_told() {
        let tmp = tempfile::tempdir().unwrap();
        let (server, journal, addresses, mut frames) = recipient_item_fixture(
            tmp.path(),
            &[("ri_zombie", Some("arrival-alias"), "peer content")],
        )
        .await;

        server
            .handle_client_frame_tiered(
                ClientFrame::RemoveRecipientItem {
                    item_id: "ri_zombie".to_owned(),
                },
                AttachMode::ReadWrite,
                ConnectionTier::TrustedLocal,
                1,
            )
            .await;
        let after_removal = crate::domain::ports::RoomJournalReader::load_entries(journal.as_ref())
            .await
            .unwrap()
            .len();

        server
            .handle_client_frame_tiered(
                ClientFrame::AcknowledgeRecipientItem {
                    item_id: "ri_zombie".to_owned(),
                },
                AttachMode::ReadWrite,
                ConnectionTier::TrustedLocal,
                1,
            )
            .await;

        let entries = crate::domain::ports::RoomJournalReader::load_entries(journal.as_ref())
            .await
            .unwrap();
        assert_eq!(
            entries.len(),
            after_removal,
            "acknowledging a tombstone must append NOTHING"
        );
        assert_eq!(
            refusal_text(frames.try_recv().ok()),
            "recipient item already removed",
            "told, not swallowed: the operator named a disposed item"
        );
        let item = crate::adapters::policy::JournalRecipientItemProjection::from_entries(&entries)
            .get(&addresses[0])
            .expect("the tombstone survives a cold re-fold");
        assert_eq!(
            item.alias.as_deref(),
            Some("arrival-alias"),
            "no field of a Removed item is touched"
        );
    }

    /// AC3(f) + AC1's gate — one gate serves both acts, and each refuses in
    /// **its own** words. The acknowledge strings are byte-identical to the
    /// ones 19.16 shipped.
    ///
    /// Mutant → RED: drop the `TrustedLocal` check.
    #[tokio::test]
    async fn the_gate_refuses_each_act_in_its_own_words() {
        let tmp = tempfile::tempdir().unwrap();
        let (server, _journal, _addresses, mut frames) =
            recipient_item_fixture(tmp.path(), &[("ri_gated", None, "peer content")]).await;

        for (frame, expected) in [
            (
                ClientFrame::RemoveRecipientItem {
                    item_id: "ri_gated".to_owned(),
                },
                "recipient item removal is same-host trusted-local only",
            ),
            (
                ClientFrame::AcknowledgeRecipientItem {
                    item_id: "ri_gated".to_owned(),
                },
                "recipient acknowledgement is same-host trusted-local only",
            ),
        ] {
            server
                .handle_client_frame_tiered(frame, AttachMode::ReadWrite, ConnectionTier::Peer, 1)
                .await;
            assert_eq!(refusal_text(frames.try_recv().ok()), expected);
        }

        server
            .handle_client_frame_tiered(
                ClientFrame::RemoveRecipientItem {
                    item_id: "ri_gated".to_owned(),
                },
                AttachMode::ReadOnly,
                ConnectionTier::TrustedLocal,
                1,
            )
            .await;
        assert_eq!(refusal_text(frames.try_recv().ok()), "read-only");
    }

    #[cfg(not(feature = "a2a"))]
    #[tokio::test]
    async fn defer_cannot_write_an_unservable_turn_without_the_a2a_feature() {
        let tmp = tempfile::tempdir().unwrap();
        let (server, journal, addresses, mut frames) = recipient_item_fixture(
            tmp.path(),
            &[("ri_historical_consultation", None, "historical question")],
        )
        .await;
        let before = journal.events.lock().await.len();
        server
            .handle_client_frame_tiered(
                ClientFrame::DeferRecipientItem {
                    item_id: addresses[0].item().as_str().to_owned(),
                },
                AttachMode::ReadWrite,
                ConnectionTier::TrustedLocal,
                1,
            )
            .await;
        assert!(matches!(
            frames.try_recv().unwrap(),
            DaemonFrame::RecipientActionResult {
                action: RecipientAction::Defer,
                target: RecipientActionTarget::Item { item_id },
                outcome: RecipientActionOutcome::Refused {
                    error: ProtocolError::Malformed(_),
                },
            } if item_id == addresses[0].item().as_str()
        ));
        assert_eq!(
            journal.events.lock().await.len(),
            before,
            "a valid historical item cannot acquire an unservable defer turn"
        );
        server
            .handle_client_frame_tiered(
                ClientFrame::AcknowledgeRecipientItem {
                    item_id: addresses[0].item().as_str().to_owned(),
                },
                AttachMode::ReadWrite,
                ConnectionTier::TrustedLocal,
                1,
            )
            .await;
        assert!(
            journal.events.lock().await.iter().any(|event| matches!(
                event,
                crate::domain::models::RoomEvent::RecipientItemAcknowledged { address, .. } if address == &addresses[0]
            )),
            "positive control: the historical item and writable journal remain usable"
        );
    }

    #[tokio::test]
    async fn concurrent_acknowledgements_append_once_and_report_applied_and_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let (server, journal, _addresses, mut frames) =
            recipient_item_fixture(tmp.path(), &[("ri_twice_acked", None, "peer content")]).await;

        tokio::join!(
            server.handle_client_frame_tiered(
                ClientFrame::AcknowledgeRecipientItem {
                    item_id: "ri_twice_acked".to_owned(),
                },
                AttachMode::ReadWrite,
                ConnectionTier::TrustedLocal,
                1,
            ),
            server.handle_client_frame_tiered(
                ClientFrame::AcknowledgeRecipientItem {
                    item_id: "ri_twice_acked".to_owned(),
                },
                AttachMode::ReadWrite,
                ConnectionTier::TrustedLocal,
                1,
            ),
        );

        let mut outcomes = Vec::new();
        for frame in [frames.try_recv().unwrap(), frames.try_recv().unwrap()] {
            let DaemonFrame::RecipientActionResult {
                action,
                target,
                outcome,
            } = frame
            else {
                panic!("acknowledgement result is correlated: {frame:?}");
            };
            assert_eq!(action, RecipientAction::Acknowledge);
            assert_eq!(
                target,
                RecipientActionTarget::Item {
                    item_id: "ri_twice_acked".to_owned(),
                }
            );
            outcomes.push(outcome);
        }
        assert!(outcomes.contains(&RecipientActionOutcome::Applied { reply: None }));
        assert!(outcomes.contains(&RecipientActionOutcome::Idempotent));
        assert!(frames.try_recv().is_err(), "one result per action");

        let entries = crate::domain::ports::RoomJournalReader::load_entries(journal.as_ref())
            .await
            .unwrap();
        assert_eq!(
            entries
                .iter()
                .filter(|entry| matches!(
                    entry.record,
                    crate::domain::models::JournalRecord::Room(
                        crate::domain::models::RoomEvent::RecipientItemAcknowledged { .. }
                    )
                ))
                .count(),
            1
        );
    }

    /// Story 19.16c AC2 — the disposal is complete at **every layer the
    /// operator can read**, and the ledger never quoted the payload to begin
    /// with.
    ///
    /// Driven with TWO unmistakable peer-supplied strings — the content and the
    /// sender-selected task correlation — because a well-meaning "the row
    /// should still say something" swap of one for the other would otherwise
    /// pass.
    ///
    /// Mutants → RED: `content = Some(String::new())` instead of `None`;
    /// leaving `transparency_row` quoting `content`.
    #[tokio::test]
    async fn the_disposal_reaches_the_projection_and_every_rendered_surface() {
        const PAYLOAD: &str = "PEER-SUPPLIED-PAYLOAD-19-16C";

        let tmp = tempfile::tempdir().unwrap();
        let (server, journal, _addresses, _frames) =
            recipient_item_fixture(tmp.path(), &[("ri_disposed", None, PAYLOAD)]).await;
        // The fixture's task correlation is the second peer-controlled string.
        let sender_task = "ri_disposed-sender-task";

        // Positive control: there IS something to dispose of.
        assert!(matches!(
            recipient_items(journal.as_ref())
                .await
                .find_by_id("ri_disposed")
                .unwrap()
                .state,
            crate::domain::models::RecipientItemState::Received { content, .. } if content == PAYLOAD
        ));
        let before = rendered_surfaces(journal.as_ref()).await;
        for (surface, text) in &before {
            assert!(
                !text.contains(PAYLOAD) && !text.contains(sender_task),
                "a transparency LEDGER records that an interaction happened, never its \
                 payload — {surface} still quotes a peer-chosen byte:\n{text}"
            );
        }

        server
            .handle_client_frame_tiered(
                ClientFrame::RemoveRecipientItem {
                    item_id: "ri_disposed".to_owned(),
                },
                AttachMode::ReadWrite,
                ConnectionTier::TrustedLocal,
                1,
            )
            .await;

        assert!(matches!(
            recipient_items(journal.as_ref())
                .await
                .find_by_id("ri_disposed")
                .unwrap()
                .state,
            crate::domain::models::RecipientItemState::Removed { .. }
        ));
        for (surface, text) in rendered_surfaces(journal.as_ref()).await {
            assert!(
                text.contains("ri_disposed removed — its content is no longer shown here"),
                "{surface} must state what the act did:\n{text}"
            );
            assert!(
                !text.contains(PAYLOAD) && !text.contains(sender_task),
                "{surface} still carries a peer-chosen byte:\n{text}"
            );
            for lie in [
                "deleted",
                "erased",
                "purged",
                "scrubbed",
                "wiped",
                "no copy remains",
            ] {
                assert!(
                    !text.contains(lie),
                    "the journal line keeps the content forever — {surface} must not say \
                     `{lie}`:\n{text}"
                );
            }
        }
    }

    /// Every surface that renders `TransparencyRow::summary`, rendered for
    /// real: the CLI text report, the `--json` envelope, and the TUI panel.
    /// Patching one of them instead of `transparency_row` is the forbidden fix.
    async fn rendered_surfaces(journal: &RecordingJournal) -> Vec<(&'static str, String)> {
        let entries = crate::domain::ports::RoomJournalReader::load_entries(journal)
            .await
            .unwrap();
        let report = crate::domain::services::transparency::TransparencyReport {
            rows: crate::domain::services::transparency::fold_transparency(&entries),
            ..Default::default()
        };
        let render = |json: bool| {
            let mut out = Vec::new();
            crate::adapters::cli::team::log::render_team_log(None, json, &report, None, &mut out)
                .expect("team log renders");
            String::from_utf8(out).expect("utf-8")
        };

        let mut state = crate::adapters::tui::state::TuiState::new(200, 40);
        state.transparency_panel.apply_read(report.rows.clone(), 1);
        let area = ratatui::layout::Rect::new(0, 0, 200, 40);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        crate::adapters::tui::widgets::transparency_panel::render(
            area,
            &mut buffer,
            &mut state.transparency_panel,
            0,
            &state.focus,
            &state.theme,
        );
        let panel = buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        vec![
            ("/team log", render(false)),
            ("/team log --json", render(true)),
            ("the TUI transparency panel", panel),
        ]
    }

    /// A runtime that keeps every inbound task alive: the point of the restart
    /// keystone is the durable item, not the task's terminal transition.
    #[cfg(feature = "a2a")]
    #[derive(Default)]
    struct RestartRuntime {
        senders:
            tokio::sync::Mutex<Vec<tokio::sync::watch::Sender<crate::domain::models::NodeState>>>,
    }

    #[cfg(feature = "a2a")]
    #[async_trait::async_trait]
    impl crate::domain::ports::InboundPeerRuntime for RestartRuntime {
        async fn start(
            &self,
            _task: crate::domain::ports::InboundPeerTask,
            _cancel: CancellationToken,
        ) -> Result<
            tokio::sync::watch::Receiver<crate::domain::models::NodeState>,
            crate::domain::ports::InboundPeerError,
        > {
            let (tx, rx) = tokio::sync::watch::channel(crate::domain::models::NodeState::Running);
            self.senders.lock().await.push(tx);
            Ok(rx)
        }

        async fn request_admission_approval(
            &self,
            _principal: &crate::domain::models::PolicyPrincipal,
            _summary: &str,
        ) -> Result<
            crate::domain::ports::InboundApprovalTicket,
            crate::domain::ports::InboundPeerError,
        > {
            Err(crate::domain::ports::InboundPeerError::unavailable(
                "unused",
            ))
        }

        async fn take_result(
            &self,
            _node_id: &AgentId,
        ) -> Option<crate::domain::ports::StagedPeerResult> {
            None
        }

        async fn disclosure_forbidden_fragments(&self) -> Vec<String> {
            Vec::new()
        }

        async fn reconcile_orphaned_tasks(&self, _subagent_type: &str) -> Vec<AgentId> {
            Vec::new()
        }
    }

    /// Story 19.16c AC4 — **the tombstone survives a real restart.**
    ///
    /// Both front doors are real: the item is created by an inbound
    /// `message/send` over a bound listener, and it is removed through the
    /// gated daemon frame. Then the process's objects are discarded and a
    /// second listener is started over the **same workspace journal**, which
    /// runs the production restart reconciliation before anything is read.
    ///
    /// ⛔ No mutant: every byte whose mutation breaks restart recovery is the
    /// fold arm, and `the_fold_refuses_an_acknowledgement_that_follows_a_removal`
    /// already turns that RED.
    #[cfg(feature = "a2a")]
    #[tokio::test]
    async fn the_tombstone_survives_a_real_restart() {
        use crate::domain::ports::RoomJournalReader;

        async fn listener_over(
            workspace: &std::path::Path,
            keys: &std::path::Path,
            journal: Arc<crate::infrastructure::subagent::NodeJournal>,
            domain_tx: mpsc::UnboundedSender<AppEvent>,
        ) -> (
            String,
            CancellationToken,
            tokio::task::JoinHandle<anyhow::Result<()>>,
        ) {
            let room: Arc<dyn crate::domain::ports::RoomJournal> = Arc::new(
                crate::infrastructure::subagent::NodeRoomJournal::new(journal, Some(domain_tx)),
            );
            let signer = crate::adapters::rap::IdentityKeyStore::new(keys)
                .load_or_generate()
                .expect("identity");
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}/", listener.local_addr().unwrap());
            let cancel = CancellationToken::new();
            let handle = tokio::spawn(crate::adapters::a2a::server::serve(
                listener,
                crate::adapters::a2a::server::ServeConfig {
                    registry: Arc::new(
                        crate::domain::models::capability_registry::CapabilityRegistry::new(None),
                    ),
                    signer,
                    security: crate::adapters::a2a::auth::A2aServerSecurity::default(),
                    runtime: Some(Arc::new(RestartRuntime::default())
                        as Arc<dyn crate::domain::ports::InboundPeerRuntime>),
                    transparency: Arc::new(
                        crate::adapters::a2a::transparency::TransparencySink::new(room),
                    ),
                    policy: crate::adapters::a2a::admission::A2aAdmissionPolicy::Allow,
                    workspace: workspace.to_path_buf(),
                    advertised_host: None,
                    cards: Arc::new(crate::adapters::a2a::card_cache::SignedCardCache::new()),
                },
                cancel.child_token(),
            ));
            (endpoint, cancel, handle)
        }

        let tmp = tempfile::tempdir().unwrap();
        let keys = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_path_buf();
        let (domain_tx, _domain_rx) = mpsc::unbounded_channel();

        // ── Process #1: a real inbound send, then a real removal frame. ──
        let journal = Arc::new(
            crate::infrastructure::subagent::NodeJournal::open_workspace(&ws)
                .await
                .expect("journal"),
        );
        let (endpoint, cancel, http) =
            listener_over(&ws, keys.path(), journal.clone(), domain_tx.clone()).await;
        let response: serde_json::Value = reqwest::Client::new()
            .post(&endpoint)
            .json(&crate::adapters::a2a::jsonrpc::JsonRpcRequest::new(
                1,
                "message/send",
                serde_json::json!({
                    "message": {
                        "messageId": "restart-task",
                        "role": "user",
                        "parts": [{ "kind": "text", "text": "content that outlives the process" }]
                    }
                }),
            ))
            .send()
            .await
            .expect("listener responds")
            .json()
            .await
            .expect("json-rpc");
        let item_id = response["result"]["metadata"]["x-rustain-item-id"]
            .as_str()
            .unwrap_or_else(|| panic!("message/send mints a recipient item: {response}"))
            .to_owned();

        let (core, _storage) = mock_core(&ws, vec![]);
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "restart-removal".to_owned(),
            ..Default::default()
        }));
        let node_tree = crate::infrastructure::subagent::NodeTree::with_event_tx(
            domain_tx.clone(),
            Arc::new(|| 123_i64),
        );
        let delivery_policy: Arc<dyn crate::domain::ports::DeliveryPolicy> =
            Arc::new(crate::domain::ports::RelationshipDeliveryPolicy);
        let peer_bus = peer_bus_slot_with_policy(&node_tree, delivery_policy.clone());
        let reader: Arc<dyn RoomJournalReader> = Arc::new(
            crate::infrastructure::subagent::node_journal::WorkspaceJournalReader::open_workspace(
                &ws,
            ),
        );
        let server = AttachServer::new_with_node_tree_bus_policy_and_journal(
            core,
            conversation,
            domain_tx.clone(),
            node_tree,
            peer_bus,
            delivery_policy,
            Arc::new(crate::infrastructure::subagent::NodeRoomJournal::new(
                journal.clone(),
                Some(domain_tx.clone()),
            )),
            reader.clone(),
            None,
        );
        server
            .handle_client_frame_tiered(
                ClientFrame::RemoveRecipientItem {
                    item_id: item_id.clone(),
                },
                AttachMode::ReadWrite,
                ConnectionTier::TrustedLocal,
                1,
            )
            .await;
        let before_entries = reader.load_entries().await.unwrap();
        let before =
            crate::adapters::policy::JournalRecipientItemProjection::from_entries(&before_entries);
        let tombstone = before.find_by_id(&item_id).expect("tombstone");
        assert!(matches!(
            tombstone.state,
            crate::domain::models::RecipientItemState::Removed { .. }
        ));
        // AC1(e): the removed id stays claimed. The a2a server seeds its
        // allocator's `claimed_ids` from exactly this snapshot's keys
        // (adapters/a2a/server.rs:220-222), and the in-crate collision tests
        // (`a_history_collision_is_refused_without_reminting`) prove a claimed
        // id is never re-minted — retention of the entry IS retention of the claim.
        assert!(
            before
                .snapshot()
                .items
                .keys()
                .any(|address| address == &tombstone.address),
            "the tombstone keeps its key in the allocator's seed set"
        );
        // The journal carries non-recipient records too — an inbound admission
        // row at minimum — so the comparison below is not a recipient-only fold.
        assert!(
            before_entries.iter().any(|entry| !matches!(
                entry.record,
                crate::domain::models::JournalRecord::Room(
                    crate::domain::models::RoomEvent::RecipientItemReceived { .. }
                        | crate::domain::models::RoomEvent::RecipientItemRemoved { .. }
                )
            )),
            "a realistic journal interleaves other records"
        );

        // …and the process vanishes here.
        cancel.cancel();
        let _ = http.await;
        drop(server);
        drop(journal);

        // ── Process #2: a fresh journal handle over the same workspace, and a
        //    fresh listener whose startup runs the production reconciliation. ──
        let restarted = Arc::new(
            crate::infrastructure::subagent::NodeJournal::open_workspace(&ws)
                .await
                .expect("journal reopens"),
        );
        let (_endpoint, cancel2, http2) =
            listener_over(&ws, keys.path(), restarted.clone(), domain_tx.clone()).await;

        let after_reader: Arc<dyn RoomJournalReader> = Arc::new(
            crate::infrastructure::subagent::node_journal::WorkspaceJournalReader::open_workspace(
                &ws,
            ),
        );
        let after_entries = after_reader.load_entries().await.unwrap();
        let after =
            crate::adapters::policy::JournalRecipientItemProjection::from_entries(&after_entries);
        let recovered = after
            .find_by_id(&item_id)
            .expect("the tombstone is recovered by folding the journal");
        assert!(matches!(
            recovered.state,
            crate::domain::models::RecipientItemState::Removed { .. }
        ));

        // The comparison contract, stated: both sides are COLD folds
        // (`from_entries`) of the same durable stream, so every field —
        // including `journal_order`, which is the absolute journal index under
        // this scheme — must match exactly. ⚠ `journal_order` must be EXCLUDED
        // from any comparison that crosses schemes: the live `apply` path
        // assigns `max(existing) + 1` instead, and the two disagree on any
        // journal containing non-recipient records. Both are monotonic in
        // append order, so no ordering consumer can tell them apart.
        assert_eq!(
            before.snapshot(),
            after.snapshot(),
            "a restart must reconstruct the same projection, field for field"
        );

        cancel2.cancel();
        let _ = http2.await;
    }

    #[tokio::test]
    async fn production_consent_gate_groups_once_and_persists_sender_only_always() {
        use crate::domain::models::{ApprovalOutcome, ApprovalScope};
        use crate::domain::ports::{ConsentProjectionQuery, ConsentState, InboundApprovalDecision};

        let tmp = tempfile::tempdir().unwrap();
        let (core, _storage) = mock_core(tmp.path(), vec![]);
        let runtime = core.ensure_runtime().await.unwrap();
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "consent-gate".to_owned(),
            ..Default::default()
        }));
        let (domain_tx, _domain_rx) = mpsc::unbounded_channel();
        let journal = Arc::new(RecordingJournal::default());
        let projection = Arc::new(crate::adapters::policy::JournalConsentProjection::default());
        let server = journaled_server_with_projection(
            core,
            conversation,
            domain_tx,
            journal.clone(),
            projection.clone(),
        );
        let (writer_tx, mut writer_rx) = mpsc::channel(8);
        server.registry.lock().await.conns.push(Conn {
            id: 1,
            tx: writer_tx,
            terminal_tx: watch::channel(None).0,
            mode: AttachMode::ReadWrite,
            tier: ConnectionTier::TrustedLocal,
            approval_ready: true,
        });
        let sender = PolicyPrincipal::rap(test_signer(100).identity().peer_id.clone());

        let first = server
            .request_admission_approval(&sender, "first task")
            .await
            .unwrap();
        let second = server
            .request_admission_approval(&sender, "second task")
            .await
            .unwrap();
        assert!(first.pending && second.pending);
        assert_eq!(
            server
                .pending_consent
                .as_ref()
                .unwrap()
                .waiting_count(&sender)
                .await,
            2
        );
        let request_id = match writer_rx.recv().await.unwrap() {
            DaemonFrame::ApprovalRequest {
                request_id, tool, ..
            } => {
                assert_eq!(tool, "a2a/sender-consent");
                request_id
            }
            frame => panic!("expected consent card, got {frame:?}"),
        };
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), writer_rx.recv())
                .await
                .is_err(),
            "a second task from the same sender must not mint a second card"
        );

        runtime
            .approval
            .resolve(
                &request_id,
                ApprovalOutcome::Once,
                crate::domain::models::PermissionResolution::Operator,
            )
            .await;
        assert_eq!(
            first.decision.await.unwrap(),
            InboundApprovalDecision::AllowOnce
        );
        assert_eq!(
            second.decision.await.unwrap(),
            InboundApprovalDecision::AllowOnce
        );
        assert_eq!(projection.consent_for(&sender), ConsentState::None);
        assert!(journal.events.lock().await.is_empty());

        let always = server
            .request_admission_approval(&sender, "third task")
            .await
            .unwrap();
        let request_id = match writer_rx.recv().await.unwrap() {
            DaemonFrame::ApprovalRequest { request_id, .. } => request_id,
            frame => panic!("expected second consent card, got {frame:?}"),
        };
        let mut approval_events = runtime.approval.subscribe();
        server
            .handle_client_frame_tiered(
                ClientFrame::ApprovalResponse {
                    request_id: request_id.clone(),
                    outcome: ApprovalOutcome::AlwaysAndSave {
                        scope: ApprovalScope::Tool("a2a/sender-consent".to_owned()),
                    },
                },
                AttachMode::ReadWrite,
                ConnectionTier::TrustedLocal,
                1,
            )
            .await;
        match approval_events.recv().await.unwrap() {
            ApprovalRuntimeEvent::Resolved { id, outcome } => {
                assert_eq!(id, request_id);
                assert_eq!(outcome, ApprovalOutcome::Once);
                assert_eq!(projection.consent_for(&sender), ConsentState::Trusted);
                assert_eq!(
                    journal.events.lock().await.len(),
                    1,
                    "the grant must be durable before ApprovalOutcome::Once is published"
                );
            }
            event => panic!("expected sender-consent resolution, got {event:?}"),
        }
        server
            .handle_client_frame_tiered(
                ClientFrame::ApprovalResponse {
                    request_id: request_id.clone(),
                    outcome: ApprovalOutcome::AlwaysAndSave {
                        scope: ApprovalScope::Tool("a2a/sender-consent".to_owned()),
                    },
                },
                AttachMode::ReadWrite,
                ConnectionTier::TrustedLocal,
                1,
            )
            .await;
        assert_eq!(
            journal.events.lock().await.len(),
            1,
            "duplicate `[a]` frames are idempotent"
        );
        assert_eq!(
            always.decision.await.unwrap(),
            InboundApprovalDecision::AllowAlways
        );
        assert_eq!(projection.consent_for(&sender), ConsentState::Trusted);
        assert_eq!(journal.events.lock().await.len(), 1);

        let trusted = server
            .request_admission_approval(&sender, "later task")
            .await
            .unwrap();
        assert!(!trusted.pending);
        assert_eq!(
            trusted.decision.await.unwrap(),
            InboundApprovalDecision::AllowAlways
        );

        let other = PolicyPrincipal::rap(test_signer(101).identity().peer_id.clone());
        let unrelated = server
            .request_admission_approval(&other, "different sender")
            .await
            .unwrap();
        assert!(
            unrelated.pending,
            "always-trust must be scoped to the selected sender"
        );
        let other_request = match writer_rx.recv().await.unwrap() {
            DaemonFrame::ApprovalRequest { request_id, .. } => request_id,
            frame => panic!("expected other sender card, got {frame:?}"),
        };
        runtime
            .approval
            .resolve(
                &other_request,
                ApprovalOutcome::Reject { feedback: None },
                crate::domain::models::PermissionResolution::Operator,
            )
            .await;
        assert_eq!(
            unrelated.decision.await.unwrap(),
            InboundApprovalDecision::Decline
        );
    }

    #[cfg(feature = "a2a")]
    #[tokio::test]
    async fn k02_attach_consent_separates_equal_peer_rails() {
        use crate::adapters::rap::VerifiedPeerConsumer as _;
        use crate::domain::models::{ApprovalOutcome, PrincipalKind, RoomEvent};
        use crate::domain::ports::{
            ConsentProjectionQuery, ConsentSlot, ConsentState, InboundApprovalDecision,
        };

        let tmp = tempfile::tempdir().unwrap();
        let (core, _) = mock_core(tmp.path(), vec![]);
        let journal = Arc::new(RecordingJournal::default());
        let projection = Arc::new(crate::adapters::policy::JournalConsentProjection::default());
        let (domain_tx, _domain_rx) = mpsc::unbounded_channel();
        let server = journaled_server_with_projection(
            core,
            Arc::new(Mutex::new(Conversation {
                id: "d0-k02".to_owned(),
                ..Default::default()
            })),
            domain_tx,
            journal.clone(),
            projection.clone(),
        );
        let (writer_tx, mut writer_rx) = mpsc::channel(32);
        server.registry.lock().await.conns.push(Conn {
            id: 1,
            tx: writer_tx,
            terminal_tx: watch::channel(None).0,
            mode: AttachMode::ReadWrite,
            tier: ConnectionTier::TrustedLocal,
            approval_ready: true,
        });
        let pseudo =
            crate::adapters::a2a::exec::SubmitterKey::from_api_key("d0-k02").policy_principal();
        let peer = pseudo.peer().clone();
        let pinned = PolicyPrincipal::rap(peer.clone());
        assert_ne!(pinned, pseudo);

        let first = server
            .request_admission_approval(&pseudo, "same collision task")
            .await
            .unwrap();
        let second = server
            .request_admission_approval(&pseudo, "second pseudonymous task")
            .await
            .unwrap();
        assert!(first.pending && second.pending);
        let (pseudo_request, pseudo_preview) = next_consent_card(&mut writer_rx).await;
        let consumer = DaemonPeerConsumer::new(Arc::downgrade(&server));
        let rap_peer = peer.clone();
        let mut rap = tokio::spawn(async move {
            consumer
                .consent(
                    &AgentId::root(),
                    &AgentMessage::new("same collision task"),
                    &rap_peer,
                )
                .await
        });
        let (pinned_request, pinned_preview) = next_consent_card(&mut writer_rx).await;
        assert_ne!(pseudo_request, pinned_request);
        assert_eq!(
            pseudo_preview, pinned_preview,
            "rail typing must not change card copy"
        );
        assert!(
            pinned_preview.contains(peer.as_str()),
            "RAP card must name the actual ingress peer"
        );
        let manager = server.pending_consent.as_ref().unwrap();
        assert_eq!(manager.waiting_count(&pseudo).await, 2);
        assert_eq!(manager.waiting_count(&pinned).await, 1);
        assert!(
            writer_rx.try_recv().is_err(),
            "same-kind waiters share one card"
        );

        answer_consent_always(&server, pseudo_request.clone()).await;
        assert_eq!(
            first.decision.await.unwrap(),
            InboundApprovalDecision::AllowAlways
        );
        assert_eq!(
            second.decision.await.unwrap(),
            InboundApprovalDecision::AllowAlways
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), &mut rap)
                .await
                .is_err()
        );
        assert_eq!(manager.waiting_count(&pinned).await, 1);
        assert_eq!(
            projection.slot_state(ConsentSlot::Pinned, &peer),
            ConsentState::None
        );
        assert_eq!(
            projection.slot_state(ConsentSlot::Pseudonymous, &peer),
            ConsentState::Trusted
        );
        assert!(matches!(
            journal.events.lock().await.as_slice(),
            [RoomEvent::ConsentGranted { sender: Some(sender), principal: Some(PrincipalKind::Pseudonymous), .. }]
                if sender == &peer
        ));
        answer_consent_always(&server, pseudo_request).await;
        assert_eq!(
            journal.events.lock().await.len(),
            1,
            "duplicate frame must not append"
        );
        let same_kind = server
            .request_admission_approval(&pseudo, "later pseudonymous task")
            .await
            .unwrap();
        assert!(!same_kind.pending);
        assert_eq!(
            same_kind.decision.await.unwrap(),
            InboundApprovalDecision::AllowAlways
        );
        assert!(writer_rx.try_recv().is_err());

        answer_consent_always(&server, pinned_request).await;
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(2), rap)
                .await
                .unwrap()
                .unwrap(),
            Ok(crate::adapters::rap::VerifiedPeerConsent::Accept)
        ));
        assert!(matches!(
            journal.events.lock().await.as_slice(),
            [_, RoomEvent::ConsentGranted { sender: Some(sender), principal: Some(PrincipalKind::Pinned), .. }]
                if sender == &peer
        ));
        assert_eq!(projection.consent_for(&pinned), ConsentState::Trusted);

        // The opposite grant order also preserves the collision boundary.
        let other_pseudo = crate::adapters::a2a::exec::SubmitterKey::from_api_key("d0-k02-other")
            .policy_principal();
        let other_pin = PolicyPrincipal::rap(other_pseudo.peer().clone());
        let pin_ticket = server
            .request_admission_approval(&other_pin, "pin first")
            .await
            .unwrap();
        let (pin_request, _) = next_consent_card(&mut writer_rx).await;
        answer_consent_always(&server, pin_request).await;
        assert_eq!(
            pin_ticket.decision.await.unwrap(),
            InboundApprovalDecision::AllowAlways
        );
        let pseudo_ticket = server
            .request_admission_approval(&other_pseudo, "pseudo after pin")
            .await
            .unwrap();
        assert!(pseudo_ticket.pending);
        let (request, _) = next_consent_card(&mut writer_rx).await;
        server
            .handle_client_frame_tiered(
                ClientFrame::ApprovalResponse {
                    request_id: request,
                    outcome: ApprovalOutcome::Reject { feedback: None },
                },
                AttachMode::ReadWrite,
                ConnectionTier::TrustedLocal,
                1,
            )
            .await;
        assert_eq!(
            pseudo_ticket.decision.await.unwrap(),
            InboundApprovalDecision::Decline
        );

        // Historical grants admit both rails without typed grant churn.
        let legacy_peer = test_signer(219).identity().peer_id.clone();
        journal.events.lock().await.push(RoomEvent::ConsentGranted {
            sender: Some(legacy_peer.clone()),
            granted_at: 1,
            principal: None,
        });
        let before = journal.events.lock().await.len();
        for principal in [
            PolicyPrincipal::rap(legacy_peer.clone()),
            PolicyPrincipal::Pseudonymous(legacy_peer.clone()),
        ] {
            let ticket = server
                .request_admission_approval(&principal, "legacy")
                .await
                .unwrap();
            assert!(!ticket.pending);
            assert_eq!(
                ticket.decision.await.unwrap(),
                InboundApprovalDecision::AllowAlways
            );
        }
        let consumer = DaemonPeerConsumer::new(Arc::downgrade(&server));
        assert!(matches!(
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                consumer.consent(
                    &AgentId::root(),
                    &AgentMessage::new("legacy RAP"),
                    &legacy_peer
                )
            )
            .await
            .unwrap(),
            Ok(crate::adapters::rap::VerifiedPeerConsent::Accept)
        ));
        assert_eq!(journal.events.lock().await.len(), before);
        assert!(writer_rx.try_recv().is_err());
    }

    async fn next_consent_card(
        rx: &mut mpsc::Receiver<DaemonFrame>,
    ) -> (crate::domain::models::RequestId, String) {
        match tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap()
        {
            DaemonFrame::ApprovalRequest {
                request_id,
                tool,
                summary,
                ..
            } => {
                assert_eq!(tool, "a2a/sender-consent");
                (request_id, summary)
            }
            frame => panic!("expected consent card, got {frame:?}"),
        }
    }

    async fn answer_consent_always(
        server: &AttachServer,
        request_id: crate::domain::models::RequestId,
    ) {
        server
            .handle_client_frame_tiered(
                ClientFrame::ApprovalResponse {
                    request_id,
                    outcome: crate::domain::models::ApprovalOutcome::AlwaysAndSave {
                        scope: crate::domain::models::ApprovalScope::Tool(
                            "a2a/sender-consent".to_owned(),
                        ),
                    },
                },
                AttachMode::ReadWrite,
                ConnectionTier::TrustedLocal,
                1,
            )
            .await;
    }

    #[tokio::test]
    async fn k06b_real_turns_retain_ingress_principal_in_tool_approval() {
        use crate::domain::models::{ApprovalOutcome, ResponseMode};
        let peer = test_signer(231).identity().peer_id.clone();
        for principal in [
            PolicyPrincipal::rap(peer.clone()),
            PolicyPrincipal::Pseudonymous(peer.clone()),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let provider = Arc::new(ToolThenTextProvider {
                calls: AtomicUsize::new(0),
            });
            let (core, _) = mock_core_with_provider(tmp.path(), provider);
            let runtime = core.ensure_runtime().await.unwrap();
            let mut approval_events = runtime.approval.subscribe();
            let (domain_tx, domain_rx) = mpsc::unbounded_channel();
            let server = AttachServer::new(
                core,
                Arc::new(Mutex::new(Conversation::default())),
                domain_tx,
            );
            let listener = UnixListener::bind(tmp.path().join("approval-forwarder.sock")).unwrap();
            let shutdown = CancellationToken::new();
            let service = {
                let server = server.clone();
                let shutdown = shutdown.clone();
                tokio::spawn(async move {
                    server.run(listener, domain_rx, None, None, shutdown).await;
                })
            };
            let (writer_tx, _writer_rx) = mpsc::channel(16);
            server.registry.lock().await.conns.push(Conn {
                id: 1,
                tx: writer_tx,
                terminal_tx: watch::channel(None).0,
                mode: AttachMode::ReadWrite,
                tier: ConnectionTier::TrustedLocal,
                approval_ready: true,
            });
            let mut task = inbound_task(peer.clone(), "request a tool");
            task.principal = principal.clone();
            task.response_policy.mode = ResponseMode::NotifyAndDraft;
            server.start(task, CancellationToken::new()).await.unwrap();
            let id = match tokio::time::timeout(
                std::time::Duration::from_secs(3),
                approval_events.recv(),
            )
            .await
            .unwrap()
            .unwrap()
            {
                ApprovalRuntimeEvent::Requested {
                    id, source, tool, ..
                } => {
                    assert_eq!(tool, "Bash");
                    assert_eq!(source.policy_principal(), Some(&principal));
                    assert!(matches!(
                        source,
                        crate::domain::models::tool_call::ApprovalSource::RemotePeer { peer_id, .. }
                            if peer_id == peer
                    ));
                    id
                }
                event => panic!("expected real tool approval, got {event:?}"),
            };
            runtime
                .approval
                .resolve(
                    &id,
                    ApprovalOutcome::Reject { feedback: None },
                    crate::domain::models::PermissionResolution::Operator,
                )
                .await;
            let guard =
                tokio::time::timeout(std::time::Duration::from_secs(3), server.turn_serial.lock())
                    .await
                    .unwrap();
            drop(guard);
            shutdown.cancel();
            service.await.unwrap();
        }
    }

    #[cfg(feature = "a2a")]
    #[tokio::test]
    async fn k09d_attach_response_policy_preserves_collision_provenance() {
        use crate::domain::models::{NotificationUrgency, ResponseMode, SemanticMessageType};
        let workspace = tempfile::tempdir().unwrap();
        let (policy, peer) =
            crate::adapters::daemon::consent::loopback_collision_policy(workspace.path());
        let policy = Arc::new(policy);
        let (core, _) = mock_core(workspace.path(), vec![]);
        let (domain_tx, _domain_rx) = mpsc::unbounded_channel();
        let tree = crate::infrastructure::subagent::NodeTree::with_event_tx(
            domain_tx.clone(),
            Arc::new(|| 123_i64),
        );
        let delivery: Arc<dyn crate::domain::ports::DeliveryPolicy> = Arc::new(
            crate::domain::ports::EffectiveDeliveryPolicy::new(policy.clone()),
        );
        let bus = peer_bus_slot_with_policy(&tree, delivery.clone());
        let journal = Arc::new(RecordingJournal::default());
        let server = AttachServer::new_with_node_tree_bus_policy_and_journal(
            core,
            Arc::new(Mutex::new(Conversation::default())),
            domain_tx,
            tree,
            bus,
            delivery,
            journal.clone(),
            journal,
            Some(Arc::new(
                crate::adapters::policy::JournalConsentProjection::default(),
            )),
        );
        server.configure_consent_policy(policy).await;
        let pinned = server.response_policy(
            &PolicyPrincipal::rap(peer.clone()),
            SemanticMessageType::Unknown,
        );
        assert_eq!(pinned.mode, ResponseMode::NotifyAndAuto);
        assert_eq!(pinned.notification, NotificationUrgency::Immediate);
        assert_eq!(pinned.auto_response.as_deref(), Some("pinned reply"));
        assert_eq!(
            pinned.provenance.sender_label.as_deref(),
            Some("loopback-pin")
        );
        let pseudonymous = server.response_policy(
            &PolicyPrincipal::Pseudonymous(peer),
            SemanticMessageType::Unknown,
        );
        assert_eq!(pseudonymous.mode, ResponseMode::NotifyAndWait);
        assert_eq!(pseudonymous.notification, NotificationUrgency::Queue);
        assert_eq!(pseudonymous.auto_response, None);
        assert_eq!(pseudonymous.provenance.sender_label, None);
    }

    /// F8 behavioral keystone: grant via the card → delivery stops prompting →
    /// a `ConsentRevoked` appended out-of-band (e.g. `/team untrust` from
    /// another client) → the next delivery prompts again. This is the cycle the
    /// source-grep "wiring" tests cannot prove, and it exercises the D1 live
    /// journal refold. Every step is bounded — a mutant that fails to refresh
    /// must FAIL this test, never hang.
    #[tokio::test]
    async fn consent_revocation_via_journal_takes_effect_live_and_re_prompts() {
        use crate::domain::models::{ApprovalOutcome, ApprovalScope, RoomEvent};
        use crate::domain::ports::{ConsentProjectionQuery, ConsentState, InboundApprovalDecision};

        let tmp = tempfile::tempdir().unwrap();
        let (core, _storage) = mock_core(tmp.path(), vec![]);
        let runtime = core.ensure_runtime().await.unwrap();
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "consent-revoke".to_owned(),
            ..Default::default()
        }));
        let (domain_tx, _domain_rx) = mpsc::unbounded_channel();
        let journal = Arc::new(RecordingJournal::default());
        let projection = Arc::new(crate::adapters::policy::JournalConsentProjection::default());
        let server = journaled_server_with_projection(
            core,
            conversation,
            domain_tx,
            journal.clone(),
            projection.clone(),
        );
        let (writer_tx, mut writer_rx) = mpsc::channel(8);
        server.registry.lock().await.conns.push(Conn {
            id: 1,
            tx: writer_tx,
            terminal_tx: watch::channel(None).0,
            mode: AttachMode::ReadWrite,
            tier: ConnectionTier::TrustedLocal,
            approval_ready: true,
        });
        let sender = PolicyPrincipal::rap(test_signer(200).identity().peer_id.clone());

        // First contact: a consent card is rendered.
        let first = server
            .request_admission_approval(&sender, "first task")
            .await
            .unwrap();
        assert!(first.pending);
        let request_id = match writer_rx.recv().await.unwrap() {
            DaemonFrame::ApprovalRequest { request_id, .. } => request_id,
            frame => panic!("expected consent card, got {frame:?}"),
        };

        // Grant via [a] (AlwaysAndSave on the sender-consent tool). The manager
        // persists ConsentGranted first, then resolves the ticket Once.
        let mut approval_events = runtime.approval.subscribe();
        server
            .handle_client_frame_tiered(
                ClientFrame::ApprovalResponse {
                    request_id: request_id.clone(),
                    outcome: ApprovalOutcome::AlwaysAndSave {
                        scope: ApprovalScope::Tool("a2a/sender-consent".to_owned()),
                    },
                },
                AttachMode::ReadWrite,
                ConnectionTier::TrustedLocal,
                1,
            )
            .await;
        match approval_events.recv().await.unwrap() {
            ApprovalRuntimeEvent::Resolved { outcome, .. } => {
                assert_eq!(outcome, ApprovalOutcome::Once);
            }
            event => panic!("expected sender-consent resolution, got {event:?}"),
        }
        assert_eq!(projection.consent_for(&sender), ConsentState::Trusted);

        // Delivery no longer prompts: a new task from this sender short-circuits.
        let trusted = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            server.request_admission_approval(&sender, "second task"),
        )
        .await
        .expect("trusted sender must resolve without hanging")
        .unwrap();
        assert!(!trusted.pending);

        // Wait for the grant's watcher to clear the pending group, so the next
        // register for this sender is first-contact (requests a fresh card)
        // rather than joining the already-resolved group.
        let manager = server.pending_consent.as_ref().unwrap().clone();
        while manager.waiting_count(&sender).await > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }

        // Out-of-band revocation — mimics `/team untrust` written by another
        // client. D1's register-time refold must pick this up live.
        journal.events.lock().await.push(RoomEvent::ConsentRevoked {
            sender: Some(sender.peer().clone()),
            revoked_at: 99,
        });

        // The next delivery MUST prompt again — the revocation takes effect
        // without a daemon restart.
        let re_prompted = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            server.request_admission_approval(&sender, "third task"),
        )
        .await
        .expect("a mutant that fails to refresh the projection must FAIL, not hang")
        .unwrap();
        assert!(
            re_prompted.pending,
            "revocation must re-prompt the next delivery (live projection refresh)"
        );
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), writer_rx.recv())
            .await
            .expect("the re-prompted consent card must be sent to the writer");
    }

    #[tokio::test]
    async fn sender_consent_without_writer_remains_pending_for_next_writer() {
        use crate::domain::models::ApprovalOutcome;
        use crate::domain::ports::InboundApprovalDecision;

        let tmp = tempfile::tempdir().unwrap();
        let (core, _storage) = mock_core(tmp.path(), vec![]);
        let runtime = core.ensure_runtime().await.unwrap();
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "consent-next-writer".to_owned(),
            ..Default::default()
        }));
        let (domain_tx, _domain_rx) = mpsc::unbounded_channel();
        let server = journaled_server(
            core,
            conversation,
            domain_tx,
            Arc::new(RecordingJournal::default()),
        );
        let sender = PolicyPrincipal::rap(test_signer(102).identity().peer_id.clone());

        let ticket = server
            .request_admission_approval(&sender, "wait for a writer")
            .await
            .unwrap();
        assert!(ticket.pending);
        let mut decision = ticket.decision;
        for _ in 0..32 {
            if server.pending_consent_cards.lock().await.len() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(server.pending_consent_cards.lock().await.len(), 1);
        assert!(matches!(
            decision.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));

        let (writer_tx, mut writer_rx) = mpsc::channel(1);
        server.registry.lock().await.conns.push(Conn {
            id: 9,
            tx: writer_tx,
            terminal_tx: watch::channel(None).0,
            mode: AttachMode::ReadWrite,
            tier: ConnectionTier::TrustedLocal,
            approval_ready: true,
        });
        server.emit_pending_consent_cards(9).await;
        let request_id = match writer_rx.recv().await.unwrap() {
            DaemonFrame::ApprovalRequest {
                request_id,
                summary,
                ..
            } => {
                assert!(summary.contains(sender.peer().as_str()));
                request_id
            }
            frame => panic!("expected retained sender consent card, got {frame:?}"),
        };

        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
        assert!(
            matches!(
                decision.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ),
            "an unresponsive writer must leave sender consent pending without a timeout"
        );
        runtime
            .approval
            .resolve(
                &request_id,
                ApprovalOutcome::Reject { feedback: None },
                crate::domain::models::PermissionResolution::Operator,
            )
            .await;
        assert_eq!(decision.await.unwrap(), InboundApprovalDecision::Decline);
    }

    #[tokio::test]
    async fn sender_consent_writer_queue_full_fails_closed_with_warning() {
        use crate::domain::ports::InboundApprovalDecision;

        let tmp = tempfile::tempdir().unwrap();
        let (core, _storage) = mock_core(tmp.path(), vec![]);
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "consent-full-writer".to_owned(),
            ..Default::default()
        }));
        let (domain_tx, mut domain_rx) = mpsc::unbounded_channel();
        let server = journaled_server(
            core,
            conversation,
            domain_tx,
            Arc::new(RecordingJournal::default()),
        );
        let (writer_tx, _writer_rx) = mpsc::channel(1);
        writer_tx.try_send(DaemonFrame::Detached).unwrap();
        server.registry.lock().await.conns.push(Conn {
            id: 10,
            tx: writer_tx,
            terminal_tx: watch::channel(None).0,
            mode: AttachMode::ReadWrite,
            tier: ConnectionTier::TrustedLocal,
            approval_ready: true,
        });

        let ticket = server
            .request_admission_approval(
                &PolicyPrincipal::rap(test_signer(103).identity().peer_id.clone()),
                "cannot enter a full queue",
            )
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), ticket.decision)
                .await
                .expect("queue-full rejection must not hang")
                .unwrap(),
            InboundApprovalDecision::Decline
        );
        assert_eq!(server.blocked_waiting.load(Ordering::SeqCst), 1);
        assert!(server.pending_consent_cards.lock().await.is_empty());
        let warning = tokio::time::timeout(std::time::Duration::from_secs(1), domain_rx.recv())
            .await
            .expect("queue-full admission warning must be emitted")
            .expect("domain receiver remains open");
        let AppEvent::SystemNotice { level, message, .. } = warning else {
            panic!("expected queue-full SystemNotice warning");
        };
        assert_eq!(level, crate::domain::models::NoticeLevel::Warning);
        assert!(message.contains("Sender consent card admission failed"));
    }

    #[tokio::test]
    async fn pending_consent_node_waits_and_escalates_hazard_once() {
        let tmp = tempfile::tempdir().unwrap();
        let (core, _storage) = mock_core(tmp.path(), vec![]);
        let conversation = Arc::new(Mutex::new(Conversation::default()));
        let (domain_tx, _domain_rx) = mpsc::unbounded_channel();
        let node_journal = Arc::new(
            crate::infrastructure::subagent::NodeJournal::open_workspace(tmp.path())
                .await
                .unwrap(),
        );
        let now = Arc::new(std::sync::atomic::AtomicI64::new(1_000));
        let now_for_tree = now.clone();
        let node_tree = crate::infrastructure::subagent::NodeTree::with_event_tx(
            domain_tx.clone(),
            Arc::new(move || now_for_tree.load(Ordering::SeqCst)),
        )
        .with_journal(node_journal);
        let delivery_policy: Arc<dyn crate::domain::ports::DeliveryPolicy> =
            Arc::new(crate::domain::ports::RelationshipDeliveryPolicy);
        let peer_bus = peer_bus_slot_with_policy(&node_tree, delivery_policy.clone());
        let room_journal = Arc::new(RecordingJournal::default());
        let server = AttachServer::new_with_node_tree_bus_policy_and_journal(
            core,
            conversation,
            domain_tx,
            node_tree,
            peer_bus,
            delivery_policy,
            room_journal.clone(),
            room_journal,
            Some(Arc::new(
                crate::adapters::policy::JournalConsentProjection::default(),
            )),
        );
        let node_id = AgentId::new();
        server
            .park_pending_consent(&node_id, "a2a-pending-consent", CancellationToken::new())
            .await
            .unwrap();
        let waiting = server
            .node_tree()
            .list()
            .await
            .into_iter()
            .find(|node| node.agent_id == node_id)
            .expect("pending-consent node remains visible");
        assert_eq!(waiting.current_status, NodeState::Waiting);
        assert_eq!(
            waiting.wait_reason,
            Some(crate::domain::models::WaitReason::AwaitingPeerResponse)
        );

        now.store(1_001, Ordering::SeqCst);
        assert_eq!(
            server.node_tree().raise_due_hazards(0).await,
            vec![node_id.clone()]
        );
        assert!(
            server.node_tree().raise_due_hazards(0).await.is_empty(),
            "the same waiting epoch must not repaint its hazard"
        );
        server.discard_pending_consent_node(&node_id).await;
    }

    #[tokio::test]
    async fn digest_flush_surfaces_one_batched_drillable_conversation_row() {
        let tmp = tempfile::tempdir().unwrap();
        let (core, _storage) = mock_core(tmp.path(), vec![]);
        let conversation = Arc::new(Mutex::new(Conversation::default()));
        let (domain_tx, _domain_rx) = mpsc::unbounded_channel();
        let server = journaled_server(
            core,
            conversation.clone(),
            domain_tx,
            Arc::new(RecordingJournal::default()),
        );
        let make_item = |byte: u8| crate::adapters::daemon::urgency::SurfaceInteraction {
            peer: test_signer(byte).identity().peer_id.clone(),
            node: AgentId::from_validated(format!("digest-node-{byte}")),
            task: Some(format!("digest-task-{byte}")),
            text: format!("message-{byte}"),
            notification: crate::domain::models::NotificationUrgency::Digest,
            provenance: crate::domain::models::InteractionPolicySnapshot::default(),
            recorded_at_ms: 10,
        };

        server
            .surface_digest_batch(crate::adapters::daemon::urgency::DigestBatch {
                items: vec![make_item(110), make_item(111)],
                flushed_at_ms: 20,
            })
            .await
            .unwrap();

        let conversation = conversation.lock().await;
        assert_eq!(conversation.messages.len(), 1);
        let content = &conversation.messages[0].content;
        assert!(content.contains("Team digest — 2 interactions"));
        assert!(content.contains("digest-task-110"));
        assert!(content.contains("digest-task-111"));
        assert!(content.contains("response: notify-and-wait · via default"));
        assert!(content.contains("notification: queue · via default"));
    }

    #[tokio::test]
    async fn queue_urgency_surfaces_at_the_next_turn_complete_without_notice_severity() {
        let tmp = tempfile::tempdir().unwrap();
        let (core, _storage) = mock_core(tmp.path(), vec![]);
        let conversation = Arc::new(Mutex::new(Conversation::default()));
        let (domain_tx, mut domain_rx) = mpsc::unbounded_channel();
        let journal = Arc::new(RecordingJournal::default());
        let router = Arc::new(crate::adapters::daemon::urgency::UrgencyRouter::new(
            Arc::new(crate::domain::clock::MockClock::at_wall_ms(10)),
            journal.clone(),
            &[],
            60_000,
        ));
        let sender = test_signer(112).identity().peer_id.clone();
        router
            .route(crate::adapters::daemon::urgency::SurfaceInteraction {
                peer: sender,
                node: AgentId::from_validated("queue-node"),
                task: Some("queue-task".to_owned()),
                text: "queued peer message".to_owned(),
                notification: crate::domain::models::NotificationUrgency::Queue,
                provenance: crate::domain::models::InteractionPolicySnapshot::default(),
                recorded_at_ms: 0,
            })
            .await
            .unwrap();
        let server =
            journaled_server_with_urgency(core, conversation.clone(), domain_tx, journal, router);
        assert!(conversation.lock().await.messages.is_empty());

        let mut assistant_buf = String::new();
        server
            .handle_bus_event(
                &AppEvent::ProviderChunk {
                    conversation_id: "queue-test".to_owned(),
                    chunk: StreamChunk::TurnComplete {
                        stop_reason: StopReason::EndTurn,
                    },
                },
                &mut assistant_buf,
            )
            .await;

        let conversation = conversation.lock().await;
        assert_eq!(conversation.messages.len(), 1);
        assert!(
            conversation.messages[0]
                .content
                .contains("queued peer message")
        );
        assert!(
            conversation.messages[0]
                .content
                .contains("notification: queue")
        );
        assert!(
            std::iter::from_fn(|| domain_rx.try_recv().ok())
                .all(|event| !matches!(event, AppEvent::SystemNotice { .. })),
            "urgency must not be encoded as NoticeLevel"
        );
    }

    #[cfg(feature = "a2a")]
    async fn reply_card_fixture(
        workspace: &std::path::Path,
        card_content: &str,
    ) -> (Arc<AttachServer>, Arc<RecordingJournal>, AgentId) {
        use crate::adapters::a2a::exec::{SubmitterKey, mint_inbound_node_id};
        use crate::domain::models::{ItemAddress, ItemId, RoomEvent};

        let submitter = SubmitterKey::loopback();
        let node = mint_inbound_node_id(&submitter, "reply-task");
        let conversation = Arc::new(Mutex::new(Conversation {
            messages: vec![ChatMessage {
                id: format!("peer-response-{}", node.as_str()),
                role: MessageRole::Assistant,
                content: card_content.to_owned(),
                synthetic: true,
                ..Default::default()
            }],
            ..Default::default()
        }));
        let journal = Arc::new(RecordingJournal::default());
        journal
            .record_event(RoomEvent::RecipientItemReceived {
                address: ItemAddress::from_a2a_ingress(
                    submitter.pseudonymous_peer_id(),
                    ItemId::from_replay("ri_reply-test"),
                ),
                task: "reply-task".to_owned(),
                alias: None,
                message_type: crate::domain::models::SemanticMessageType::Unknown,
                content: "question".to_owned(),
                in_reply_to: None,
                capability_hash: None,
                reply_capability_matched: false,
                authorship: crate::domain::models::Authorship::Unknown,
                send_authorization: crate::domain::models::SendAuthorization::Unknown,
            })
            .await
            .unwrap();
        let (core, _storage) = mock_core(workspace, vec![]);
        let (domain_tx, _domain_rx) = mpsc::unbounded_channel();
        (
            journaled_server(core, conversation, domain_tx, journal.clone()),
            journal,
            node,
        )
    }

    #[cfg(feature = "a2a")]
    #[tokio::test]
    async fn drafting_card_blocks_a_direct_reply_but_a_crashed_draft_does_not() {
        use crate::adapters::daemon::response_modes::DRAFTING_PLACEHOLDER;
        use crate::adapters::policy::JournalRecipientItemProjection;
        let tmp = tempfile::tempdir().unwrap();
        let (server, journal, node) = reply_card_fixture(tmp.path(), DRAFTING_PLACEHOLDER).await;
        assert!(server.pending_drafts.begin(node.as_str()).await);
        let projection = JournalRecipientItemProjection::from_entries(
            &crate::domain::ports::RoomJournalReader::load_entries(journal.as_ref())
                .await
                .unwrap(),
        );
        let item = projection.find_by_id("ri_reply-test").unwrap();
        assert_eq!(
            server.pending_card_for_item(&projection, &item).await,
            Some(node.clone())
        );
        assert!(
            server
                .resolve_peer_draft(
                    &node,
                    crate::adapters::daemon::response_modes::DraftResolution::WriteOwn(
                        "human answer".to_owned()
                    )
                )
                .await
                .is_err()
        );
        assert_eq!(journal.events.lock().await.len(), 1);
        server.pending_drafts.abandon(node.as_str()).await;
        assert_eq!(server.pending_card_for_item(&projection, &item).await, None);
    }

    #[cfg(feature = "a2a")]
    #[tokio::test]
    async fn draft_reply_size_refusal_preserves_card_and_journal() {
        use crate::adapters::daemon::response_modes::{
            DRAFT_APPROVAL_PREFIX, DraftResolution, DraftState,
        };
        let tmp = tempfile::tempdir().unwrap();
        let card = format!("{DRAFT_APPROVAL_PREFIX}draft");
        let (server, journal, node) = reply_card_fixture(tmp.path(), &card).await;
        assert!(server.pending_drafts.begin(node.as_str()).await);
        assert!(server.pending_drafts.complete(node.as_str(), "draft").await);
        let large = "x".repeat(crate::domain::models::MAX_REPLY_BYTES + 1);
        assert!(
            server
                .resolve_peer_draft(&node, DraftResolution::Edit(large))
                .await
                .is_err()
        );
        assert_eq!(
            server.pending_drafts.state(node.as_str()).await,
            Some(DraftState::Ready {
                content: "draft".to_owned()
            })
        );
        assert_eq!(journal.events.lock().await.len(), 1);
    }

    #[cfg(feature = "a2a")]
    #[tokio::test]
    async fn concurrent_card_resolutions_report_applied_and_idempotent_once() {
        use crate::adapters::daemon::response_modes::{DRAFT_APPROVAL_PREFIX, DraftResolution};
        let tmp = tempfile::tempdir().unwrap();
        let card = format!("{DRAFT_APPROVAL_PREFIX}draft");
        let (server, journal, node) = reply_card_fixture(tmp.path(), &card).await;
        assert!(server.pending_drafts.begin(node.as_str()).await);
        assert!(server.pending_drafts.complete(node.as_str(), "draft").await);
        let (first, second) = tokio::join!(
            server.resolve_peer_draft(&node, DraftResolution::Approve),
            server.resolve_peer_draft(&node, DraftResolution::WriteOwn("human answer".to_owned()))
        );
        let outcomes = [first.unwrap(), second.unwrap()];
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(
                    outcome,
                    PeerDraftResolutionOutcome::Applied { reply: Some(_) }
                ))
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, PeerDraftResolutionOutcome::Idempotent))
                .count(),
            1
        );
        assert_eq!(
            journal
                .events
                .lock()
                .await
                .iter()
                .filter(|event| matches!(
                    event,
                    crate::domain::models::RoomEvent::RecipientItemReplied { .. }
                ))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn trusted_local_retract_dispatch_journals_once_then_marks_without_deleting() {
        let tmp = tempfile::tempdir().unwrap();
        let (core, _storage) = mock_core(tmp.path(), vec![]);
        let auto_node = AgentId::from_validated("auto-node");
        let conversation = Arc::new(Mutex::new(Conversation {
            messages: vec![ChatMessage {
                id: format!("peer-response-{}", auto_node.as_str()),
                role: MessageRole::Assistant,
                content: "retained response".to_owned(),
                authorship: crate::domain::models::Authorship::AgentComposed,
                send_authorization: crate::domain::models::SendAuthorization::Automatic,
                ..Default::default()
            }],
            ..Default::default()
        }));
        let (domain_tx, _domain_rx) = mpsc::unbounded_channel();
        let journal = Arc::new(RecordingJournal::default());
        // The retract target is derived from this disclosure, never trusted
        // from the client's frame (AC5).
        journal
            .record_event(crate::domain::models::RoomEvent::PeerDisclosure {
                peer: None,
                node: auto_node.clone(),
                task: None,
                disclosed_bytes: 17,
                authorship: crate::domain::models::Authorship::AgentComposed,
                send_authorization: crate::domain::models::SendAuthorization::Automatic,
            })
            .await
            .expect("seed disclosure");
        let server = journaled_server(core, conversation.clone(), domain_tx, journal.clone());
        let row_id = format!("peer-response-{}", auto_node.as_str());

        assert!(
            server
                .retract_auto_response(&row_id, Some(2))
                .await
                .is_err(),
            "a target_seq that does not match the journaled disclosure is refused"
        );
        server
            .retract_auto_response(&row_id, None)
            .await
            .expect("first same-host retraction derives the disclosure seq");
        assert!(
            server
                .retract_auto_response(&row_id, Some(1))
                .await
                .is_err(),
            "the same row can retract only once"
        );
        assert!(matches!(
            journal.events.lock().await.as_slice(),
            [
                crate::domain::models::RoomEvent::PeerDisclosure { .. },
                crate::domain::models::RoomEvent::AutoResponseRetracted { target_seq: 1, .. }
            ]
        ));
        let row = conversation.lock().await.messages[0].clone();
        assert_eq!(row.content, "retained response");
        assert!(row.retracted_at_ms.is_some());

        let draft_node = AgentId::from_validated("draft-node");
        conversation.lock().await.messages.push(ChatMessage {
            id: "peer-response-draft-node".to_owned(),
            role: MessageRole::Assistant,
            content: "generated draft".to_owned(),
            synthetic: true,
            ..Default::default()
        });
        assert!(server.pending_drafts.begin(draft_node.as_str()).await);
        assert!(
            server
                .pending_drafts
                .complete(draft_node.as_str(), "generated draft")
                .await
        );
        server
            .resolve_peer_draft(
                &draft_node,
                crate::adapters::daemon::response_modes::DraftResolution::WriteOwn(
                    "human answer".to_owned(),
                ),
            )
            .await
            .expect("blank-composer resolution is dispatched and journaled");
        assert_eq!(
            journal.events.lock().await.last(),
            Some(&crate::domain::models::RoomEvent::PeerDraftResolved {
                node: draft_node,
                agent_composed: false,
                sent: true,
            })
        );
        let draft = conversation.lock().await.messages[1].clone();
        assert_eq!(draft.content, "human answer");
        assert_eq!(
            draft.authorship,
            crate::domain::models::Authorship::HumanWritten
        );

        let failed_auto_node = AgentId::from_validated("failed-auto-node");
        conversation.lock().await.messages.push(ChatMessage {
            id: format!("peer-response-{}", failed_auto_node.as_str()),
            role: MessageRole::Assistant,
            content: "still visible".to_owned(),
            authorship: crate::domain::models::Authorship::AgentComposed,
            send_authorization: crate::domain::models::SendAuthorization::Automatic,
            ..Default::default()
        });
        journal
            .record_event(crate::domain::models::RoomEvent::PeerDisclosure {
                peer: None,
                node: failed_auto_node.clone(),
                task: None,
                disclosed_bytes: 13,
                authorship: crate::domain::models::Authorship::AgentComposed,
                send_authorization: crate::domain::models::SendAuthorization::Automatic,
            })
            .await
            .expect("seed failed-node disclosure");
        journal
            .fail
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(
            server
                .retract_auto_response(
                    &format!("peer-response-{}", failed_auto_node.as_str()),
                    Some(4)
                )
                .await
                .is_err()
        );
        assert_eq!(
            conversation.lock().await.messages[2].retracted_at_ms,
            None,
            "journal failure must leave the persisted row unmodified"
        );

        let failed_draft_node = AgentId::from_validated("failed-draft-node");
        conversation.lock().await.messages.push(ChatMessage {
            id: "peer-response-failed-draft-node".to_owned(),
            role: MessageRole::Assistant,
            content: "retryable draft".to_owned(),
            synthetic: true,
            ..Default::default()
        });
        assert!(
            server
                .pending_drafts
                .begin(failed_draft_node.as_str())
                .await
        );
        assert!(
            server
                .pending_drafts
                .complete(failed_draft_node.as_str(), "retryable draft")
                .await
        );
        assert!(
            server
                .resolve_peer_draft(
                    &failed_draft_node,
                    crate::adapters::daemon::response_modes::DraftResolution::Approve,
                )
                .await
                .is_err()
        );
        assert_eq!(
            server
                .pending_drafts
                .state(failed_draft_node.as_str())
                .await,
            Some(crate::adapters::daemon::response_modes::DraftState::Ready {
                content: "retryable draft".to_owned(),
            }),
            "journal failure must restore the retryable draft state"
        );
        assert_eq!(
            conversation.lock().await.messages[3].content,
            "retryable draft"
        );

        journal
            .fail
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let wait_node = AgentId::from_validated("reattached-wait-node");
        conversation.lock().await.messages.push(ChatMessage {
            id: "peer-response-reattached-wait-node".to_owned(),
            role: MessageRole::Assistant,
            content: crate::adapters::daemon::response_modes::AWAITING_RESPONSE_PLACEHOLDER
                .to_owned(),
            synthetic: true,
            ..Default::default()
        });
        conversation.lock().await.peer_row_controls.insert(
            "peer-response-reattached-wait-node".to_owned(),
            PeerRowControl {
                node_id: wait_node.as_str().to_owned(),
                item_address: None,
                message_type: crate::domain::models::SemanticMessageType::Unknown,
                sender_label: "peer".to_owned(),
                stage: PeerRowStage::AwaitingOperator,
                node_resolvable: true,
            },
        );
        assert_eq!(server.pending_drafts.state(wait_node.as_str()).await, None);
        server
            .resolve_peer_draft(
                &wait_node,
                crate::adapters::daemon::response_modes::DraftResolution::WriteOwn(
                    "manual response after reattach".to_owned(),
                ),
            )
            .await
            .expect("persisted pending response reconstructs on reattach");
        let resumed = conversation.lock().await.messages[4].clone();
        assert_eq!(resumed.content, "manual response after reattach");
        assert_eq!(
            resumed.authorship,
            crate::domain::models::Authorship::HumanWritten
        );
    }

    async fn wait_for_deregistration(server: &AttachServer, node_id: &AgentId) {
        while server.node_tree().status_rx(node_id).await.is_some() {
            tokio::task::yield_now().await;
        }
    }

    async fn spawn_inbound_forwarder(
        workspace: &Path,
        chunks: Vec<StreamChunk>,
    ) -> (
        Arc<AttachServer>,
        Arc<Mutex<Conversation>>,
        Arc<RecordingJournal>,
        CancellationToken,
        tokio::task::JoinHandle<()>,
    ) {
        let (core, _storage) = mock_core(workspace, chunks);
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "inbound-forwarder".to_owned(),
            ..Default::default()
        }));
        let (bus, domain_rx) = EventBus::new(64);
        let listener = UnixListener::bind(workspace.join("inbound-forwarder.sock")).unwrap();
        let journal = Arc::new(RecordingJournal::default());
        let server = journaled_server(
            core,
            conversation.clone(),
            bus.domain_tx.clone(),
            journal.clone(),
        );
        let shutdown = CancellationToken::new();
        let srv = server.clone();
        let shutdown_for_run = shutdown.clone();
        let handle = tokio::spawn(async move {
            srv.run(listener, domain_rx, None, None, shutdown_for_run)
                .await;
        });
        (server, conversation, journal, shutdown, handle)
    }

    struct PromptPersona(String);

    impl PersonaPort for PromptPersona {
        fn system_prompt(&self, _workspace_path: &Path) -> String {
            self.0.clone()
        }
    }

    fn mock_core_with_persona(workspace: &Path, prompt: &str) -> Arc<DaemonCore> {
        let storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
            crate::infrastructure::paths::sessions_dir(workspace),
            workspace.to_path_buf(),
        ));
        let provider: Arc<dyn StreamingProvider> = Arc::new(ScriptedProvider { chunks: vec![] });
        let workspace_for_factory = workspace.to_path_buf();
        let storage_for_factory = storage.clone();
        Arc::new(DaemonCore::new(
            workspace.to_path_buf(),
            Arc::new(ArcSwap::from_pointee(AppConfig::default())),
            Arc::new(NoOpMemory),
            storage,
            Arc::new(NoOpSecurity),
            Arc::new(PromptPersona(prompt.to_owned())),
            Arc::new(crate::adapters::rap::PeerTopicStore::new()),
            Box::new(move || {
                Ok(mock_runtime(
                    provider.clone(),
                    storage_for_factory.clone(),
                    &workspace_for_factory,
                ))
            }),
        ))
    }

    // ── Story 17.1a attach-proof test helpers ────────────────────────────────

    /// A deterministic identity for tests: one signing key per `seed` byte, no
    /// disk I/O or wall clock. Distinct seeds → distinct peer ids.
    pub(super) fn test_signer(seed: u8) -> AgentSigner {
        AgentSigner::from_signing_key(ed25519_dalek::SigningKey::from_bytes(&[seed; 32]))
    }

    /// Read the server's `AttachChallenge` and return its nonce (server-first
    /// handshake). Panics on any other frame so a test fails loudly.
    async fn read_attach_challenge(stream: &mut UnixStream) -> Vec<u8> {
        match read_frame::<_, DaemonFrame>(stream).await.unwrap() {
            Some(DaemonFrame::AttachChallenge { nonce }) => nonce,
            other => panic!("expected AttachChallenge, got {other:?}"),
        }
    }

    /// Happy-path server-first attach over a single stream (lockstep, no split):
    /// read the challenge, build a proof with `signer`, send the proof-bearing
    /// `Attach`, and return the daemon's response frame.
    pub(super) async fn attach_with_proof(
        stream: &mut UnixStream,
        read_only_ok: bool,
        tier: ConnectionTier,
        signer: &AgentSigner,
    ) -> Option<DaemonFrame> {
        let nonce = read_attach_challenge(stream).await;
        let proof = signer.attach_proof(&nonce, PROTOCOL_VERSION, tier.proof_tag(), read_only_ok);
        write_frame(
            stream,
            &ClientFrame::Attach {
                protocol_version: PROTOCOL_VERSION,
                read_only_ok,
                tier,
                challenge_nonce: nonce,
                identity: signer.identity().clone(),
                proof,
            },
        )
        .await
        .unwrap();
        read_frame::<_, DaemonFrame>(stream).await.unwrap()
    }

    /// Read the next protocol response, skipping domain-event broadcasts that
    /// can race the response on the same connection.
    async fn read_control_frame(stream: &mut UnixStream) -> Option<DaemonFrame> {
        loop {
            match read_frame::<_, DaemonFrame>(stream).await.unwrap() {
                Some(DaemonFrame::Event(_)) => continue,
                frame => return frame,
            }
        }
    }

    /// Build a signed peer envelope rooted at `signer`'s PeerId (the peer-path
    /// invariant the daemon enforces). `not_after` is in the SAME unit the
    /// daemon's verify seam uses — wall milliseconds (driven by the injected
    /// `Clock`) — so a `MockClock` controls expiry deterministically.
    fn peer_envelope(
        signer: &AgentSigner,
        sequence: u64,
        not_after: i64,
    ) -> Box<AgentEnvelope<serde_json::Value>> {
        use crate::domain::models::{AgentId, CorrelationId, MessageKind};
        let pid = signer.identity().peer_id.as_str();
        let sender = AgentId::from_peer_path(&format!("{pid}/agent")).expect("peer-rooted sender");
        // ⚑ Rooted at the sender's own namespace too — the recipient rule
        // `DF-18-4d-RECIPIENT-NAMESPACE` (closed by 18.4a) enforces. Pre-18.4a
        // this fixture addressed a bare `daemon`, which is refused now.
        let recipient =
            AgentId::from_peer_path(&format!("{pid}/daemon")).expect("peer-rooted recipient");
        Box::new(
            signer
                .sign(
                    sender,
                    recipient,
                    CorrelationId::new("corr"),
                    MessageKind::PeerMessage,
                    String::new(),
                    sequence,
                    not_after,
                    "env-nonce".to_string(),
                    Vec::new(),
                    serde_json::json!({"msg": "hello peer"}),
                )
                .expect("signing succeeds when sender is rooted at signer"),
        )
    }

    struct AcceptingPeerRecorder;

    #[async_trait::async_trait]
    impl crate::domain::ports::PeerInteractionRecorder for AcceptingPeerRecorder {
        async fn record_peer_delivery(
            &self,
            _record: crate::domain::ports::PeerDeliveryRecord,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn record_transport_refusal(
            &self,
            _record: crate::domain::ports::TransportRefusalRecord,
        ) -> Result<(), String> {
            Ok(())
        }
    }

    /// Stand up an `AttachServer` on a temp socket. `clock = None` uses the
    /// production `SystemClock`; `Some(clock)` injects it (deterministic TTL).
    async fn spawn_proof_server(
        ws: &Path,
        socket_name: &str,
        clock: Option<Arc<dyn Clock>>,
    ) -> (
        Arc<AttachServer>,
        std::path::PathBuf,
        CancellationToken,
        tokio::task::JoinHandle<()>,
    ) {
        let (core, _storage) = mock_core(ws, vec![]);
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "proof-conv".into(),
            ..Default::default()
        }));
        let (bus, domain_rx) = EventBus::new(64);
        let socket = ws.join(socket_name);
        let listener = UnixListener::bind(&socket).unwrap();
        let server = match clock {
            Some(c) => AttachServer::with_clock(core, conversation, bus.domain_tx.clone(), c),
            None => AttachServer::new(core, conversation, bus.domain_tx.clone()),
        };
        server
            .configure_peer_recorder(
                Arc::new(AcceptingPeerRecorder),
                Arc::new(crate::adapters::rap::PeerTopicStore::new()),
            )
            .await;
        let shutdown = CancellationToken::new();
        let srv = server.clone();
        let sd = shutdown.clone();
        let handle =
            tokio::spawn(async move { srv.run(listener, domain_rx, None, None, sd).await });
        (server, socket, shutdown, handle)
    }

    /// Test-only [`StoragePort`] that tallies how many *user-only* snapshots are
    /// persisted — a conversation that carries a user message but no assistant
    /// message.
    ///
    /// `run_turn` persists exactly one such snapshot before the provider call.
    /// The daemon turn driver used to persist a *second* user-only snapshot
    /// after spawning the turn; that snapshot raced the forwarder's assistant
    /// commit and could overwrite the completed transcript on fast provider
    /// paths. Counting at entry (before the delegate) makes the tally
    /// independent of which save the scheduler completes first, so a regression
    /// fails deterministically rather than only when the race lands the wrong way.
    struct UserOnlySaveCountingStorage {
        inner: Arc<dyn StoragePort>,
        user_only_saves: Arc<AtomicUsize>,
    }

    impl UserOnlySaveCountingStorage {
        fn new(inner: Arc<dyn StoragePort>, user_only_saves: Arc<AtomicUsize>) -> Self {
            Self {
                inner,
                user_only_saves,
            }
        }
    }

    #[async_trait::async_trait]
    impl StoragePort for UserOnlySaveCountingStorage {
        /// Test double: counts saves only, holds no snapshots. Explicit
        /// because the port has no default body (Story 19.1 code review).
        async fn read_snapshot(
            &self,
            _conversation_id: &str,
            _checkpoint: crate::domain::models::checkpoint::CheckpointId,
            _path: &std::path::Path,
        ) -> Result<Option<Vec<u8>>, crate::domain::errors::StorageError> {
            Ok(None)
        }
        async fn save_conversation(
            &self,
            conv: &Conversation,
        ) -> Result<(), crate::domain::errors::StorageError> {
            let has_user = conv.messages.iter().any(|m| m.role == MessageRole::User);
            let has_assistant = conv
                .messages
                .iter()
                .any(|m| m.role == MessageRole::Assistant);
            if has_user && !has_assistant {
                self.user_only_saves.fetch_add(1, Ordering::SeqCst);
            }
            self.inner.save_conversation(conv).await
        }

        async fn load_conversation(
            &self,
            id: &str,
        ) -> Result<Option<Conversation>, crate::domain::errors::StorageError> {
            self.inner.load_conversation(id).await
        }

        async fn list_conversations(
            &self,
        ) -> Result<
            Vec<crate::domain::models::ConversationSummary>,
            crate::domain::errors::StorageError,
        > {
            self.inner.list_conversations().await
        }
    }

    /// AC2/AC3/AC4/AC5: attach → UserMessage → forwarded provider chunks →
    /// assembled response; the turn persists (with `origin` tags) after the
    /// client reads it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn attach_drives_a_turn_and_persists_origin_tagged_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let chunks = vec![
            StreamChunk::Text {
                content: "hello world".into(),
                parent_tool_use_id: None,
            },
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
        ];
        let (core, storage) = mock_core(ws, chunks);
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "daemon-conv".into(),
            ..Default::default()
        }));

        let (bus, domain_rx) = EventBus::new(64);
        let domain_tx = bus.domain_tx.clone();
        let socket = ws.join("attach.sock");
        let listener = UnixListener::bind(&socket).unwrap();

        let server = AttachServer::new(core, conversation.clone(), domain_tx);
        let shutdown = CancellationToken::new();
        let srv = server.clone();
        let sd = shutdown.clone();
        let handle =
            tokio::spawn(async move { srv.run(listener, domain_rx, None, None, sd).await });

        // ── Client side ──
        let mut stream = UnixStream::connect(&socket).await.unwrap();
        // ── Client side ── server-first challenge handshake with a valid proof.
        let signer = test_signer(1);
        match attach_with_proof(&mut stream, false, ConnectionTier::TrustedLocal, &signer).await {
            Some(DaemonFrame::AttachAck { granted_mode, .. }) => {
                assert_eq!(
                    granted_mode,
                    AttachMode::ReadWrite,
                    "first attach is the writer"
                );
            }
            other => panic!("expected AttachAck, got {other:?}"),
        }

        write_frame(
            &mut stream,
            &ClientFrame::UserMessage {
                text: "hi daemon".into(),
                images: vec![],
            },
        )
        .await
        .unwrap();

        // Collect forwarded events until TurnComplete.
        let mut got_text = String::new();
        loop {
            match read_frame::<_, DaemonFrame>(&mut stream).await.unwrap() {
                Some(DaemonFrame::Event(raw)) => match raw.kind {
                    RawEventKind::Provider(StreamChunk::Text { content, .. }) => {
                        got_text.push_str(&content)
                    }
                    RawEventKind::Provider(StreamChunk::TurnComplete { .. }) => break,
                    _ => {}
                },
                Some(_) => {}
                None => panic!("stream closed before TurnComplete"),
            }
        }
        assert_eq!(
            got_text, "hello world",
            "streamed assistant text forwarded over the socket"
        );

        // AC4/AC5: the turn persisted with origin-tagged messages.
        // Give the forwarder a beat to commit the assistant turn.
        for _ in 0..50 {
            if let Ok(Some(conv)) = storage.load_conversation("daemon-conv").await {
                if conv
                    .messages
                    .iter()
                    .any(|m| m.role == MessageRole::Assistant)
                {
                    let user = conv
                        .messages
                        .iter()
                        .find(|m| m.role == MessageRole::User)
                        .expect("user message persisted");
                    let asst = conv
                        .messages
                        .iter()
                        .find(|m| m.role == MessageRole::Assistant)
                        .expect("assistant message persisted");
                    assert_eq!(user.content, "hi daemon");
                    assert_eq!(
                        user.origin,
                        ChannelKind::Terminal,
                        "inbound tagged Terminal (AC5)"
                    );
                    assert_eq!(asst.content, "hello world");
                    assert_eq!(
                        asst.origin,
                        ChannelKind::Terminal,
                        "assistant tagged Terminal (AC5)"
                    );
                    shutdown.cancel();
                    handle.abort();
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("conversation was not persisted with an assistant turn");
    }

    /// Story 12.3 G7/G8: channel turns are tagged with their origin and the
    /// committed assistant text resolves the channel response oneshot.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn channel_turn_tags_telegram_origin_and_routes_response() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let chunks = vec![
            StreamChunk::Text {
                content: "telegram response".into(),
                parent_tool_use_id: None,
            },
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
        ];
        let (core, storage) = mock_core(ws, chunks);
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "telegram-conv".into(),
            ..Default::default()
        }));

        let (bus, domain_rx) = EventBus::new(64);
        let domain_tx = bus.domain_tx.clone();
        let (channel_tx, channel_rx) = mpsc::unbounded_channel::<ChannelTurnRequest>();
        let socket = ws.join("telegram.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = AttachServer::new(core, conversation.clone(), domain_tx);
        let shutdown = CancellationToken::new();
        let srv = server.clone();
        let sd = shutdown.clone();
        let handle = tokio::spawn(async move {
            srv.run(listener, domain_rx, Some(channel_rx), None, sd)
                .await
        });

        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        channel_tx
            .send(ChannelTurnRequest {
                text: "hi from telegram".into(),
                origin: ChannelKind::Telegram,
                response_tx,
            })
            .unwrap();

        let response = tokio::time::timeout(std::time::Duration::from_secs(2), response_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response, "telegram response");

        for _ in 0..50 {
            if let Ok(Some(conv)) = storage.load_conversation("telegram-conv").await {
                if conv
                    .messages
                    .iter()
                    .any(|m| m.role == MessageRole::Assistant)
                {
                    let user = conv
                        .messages
                        .iter()
                        .find(|m| m.role == MessageRole::User)
                        .expect("user message persisted");
                    let asst = conv
                        .messages
                        .iter()
                        .find(|m| m.role == MessageRole::Assistant)
                        .expect("assistant message persisted");
                    assert_eq!(user.origin, ChannelKind::Telegram);
                    assert_eq!(asst.origin, ChannelKind::Telegram);
                    shutdown.cancel();
                    handle.abort();
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("telegram channel turn was not persisted");
    }

    /// Regression for the daemon turn-driver save race (AC4): a channel turn
    /// must persist a user-only snapshot exactly once — `run_turn`'s pre-turn
    /// save, before the provider streams — and then again with the assistant
    /// commit. A *second* user-only snapshot saved after the turn is spawned
    /// races the forwarder's `commit_assistant_turn` and overwrites the
    /// completed transcript on fast provider paths. Asserting on the save tally
    /// (counted at entry) catches the redundant save deterministically,
    /// independent of which save the scheduler happens to complete first.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn channel_turn_persists_user_snapshot_once_before_assistant_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let chunks = vec![
            StreamChunk::Text {
                content: "telegram response".into(),
                parent_tool_use_id: None,
            },
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
        ];

        // Wrap the real storage so every persisted user-only snapshot is tallied.
        let user_only_saves = Arc::new(AtomicUsize::new(0));
        let inner: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::with_workspace_root(
            crate::infrastructure::paths::sessions_dir(ws),
            ws.to_path_buf(),
        ));
        let storage: Arc<dyn StoragePort> = Arc::new(UserOnlySaveCountingStorage::new(
            inner,
            user_only_saves.clone(),
        ));
        let (core, storage) = mock_core_with_storage(ws, chunks, Arc::new(NoOpMemory), storage);
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "race-conv".into(),
            ..Default::default()
        }));

        let (bus, domain_rx) = EventBus::new(64);
        let domain_tx = bus.domain_tx.clone();
        let (channel_tx, channel_rx) = mpsc::unbounded_channel::<ChannelTurnRequest>();
        let socket = ws.join("race.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = AttachServer::new(core, conversation, domain_tx);
        let shutdown = CancellationToken::new();
        let srv = server.clone();
        let sd = shutdown.clone();
        let handle = tokio::spawn(async move {
            srv.run(listener, domain_rx, Some(channel_rx), None, sd)
                .await
        });

        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        channel_tx
            .send(ChannelTurnRequest {
                text: "hi from telegram".into(),
                origin: ChannelKind::Telegram,
                response_tx,
            })
            .unwrap();

        // The response is sent from `commit_assistant_turn` *after* it persists
        // the assistant message, so receiving it implies the commit save landed.
        let response = tokio::time::timeout(std::time::Duration::from_secs(2), response_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response, "telegram response");

        // Exactly one user-only snapshot may be persisted — `run_turn`'s
        // pre-turn save. A second one (the removed post-spawn snapshot) would
        // race and overwrite this commit; the tally is deterministic regardless
        // of save completion order.
        assert_eq!(
            user_only_saves.load(Ordering::SeqCst),
            1,
            "a second user-only snapshot save would race the assistant commit"
        );

        // The assistant commit survived (not clobbered by a stale snapshot).
        let conv = storage
            .load_conversation("race-conv")
            .await
            .unwrap()
            .expect("conversation persisted");
        let asst = conv
            .messages
            .iter()
            .find(|m| m.role == MessageRole::Assistant)
            .expect("assistant commit persisted");
        assert_eq!(asst.content, "telegram response");

        shutdown.cancel();
        handle.abort();
    }
    /// Story 19.20c: v7 is rejected before proof validation or registration at v8.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn previous_protocol_version_is_rejected_before_proof() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let (core, _storage) = mock_core(ws, vec![]);
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "v".into(),
            ..Default::default()
        }));
        let (bus, domain_rx) = EventBus::new(16);
        let socket = ws.join("v.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = AttachServer::new(core, conversation, bus.domain_tx.clone());
        let shutdown = CancellationToken::new();
        let srv = server.clone();
        let sd = shutdown.clone();
        let h = tokio::spawn(async move { srv.run(listener, domain_rx, None, None, sd).await });

        let mut stream = UnixStream::connect(&socket).await.unwrap();
        // Server issues a challenge first; the version check precedes proof work,
        // so a wrong declared version is rejected before the proof is examined.
        // Story 19.21c: a v9 client cannot decode the shared view/retract
        // frames; v10 rejects it before proof work or registration.
        let signer = test_signer(1);
        let _ = read_attach_challenge(&mut stream).await;
        write_frame(
            &mut stream,
            &ClientFrame::Attach {
                protocol_version: 9,
                read_only_ok: false,
                tier: ConnectionTier::TrustedLocal,
                challenge_nonce: vec![],
                identity: signer.identity().clone(),
                proof: crate::domain::models::Ed25519Sig(vec![0u8; 64]),
            },
        )
        .await
        .unwrap();
        match read_frame::<_, DaemonFrame>(&mut stream).await.unwrap() {
            Some(DaemonFrame::Error(ProtocolError::VersionMismatch { daemon, client })) => {
                assert_eq!(daemon, PROTOCOL_VERSION);
                assert_eq!(client, 9);
                assert_eq!(daemon, 10);
            }
            other => panic!("expected VersionMismatch error, got {other:?}"),
        }
        assert!(server.registry.lock().await.conns.is_empty());
        assert_eq!(server.next_conn_id.load(Ordering::SeqCst), 1);
        shutdown.cancel();
        h.await.unwrap();
    }

    /// AC4: an in-flight turn completes + persists even if the client detaches
    /// mid-turn (the turn runs on the daemon-owned bus, not the socket).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn turn_survives_client_detach_midturn() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let chunks = vec![
            StreamChunk::Text {
                content: "persisted anyway".into(),
                parent_tool_use_id: None,
            },
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
        ];
        let (core, storage) = mock_core(ws, chunks);
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "detach-conv".into(),
            ..Default::default()
        }));
        let (bus, domain_rx) = EventBus::new(64);
        let socket = ws.join("detach.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = AttachServer::new(core, conversation, bus.domain_tx.clone());
        let shutdown = CancellationToken::new();
        let srv = server.clone();
        let sd = shutdown.clone();
        let h = tokio::spawn(async move { srv.run(listener, domain_rx, None, None, sd).await });

        let mut stream = UnixStream::connect(&socket).await.unwrap();
        let signer = test_signer(2);
        let _ = attach_with_proof(&mut stream, false, ConnectionTier::TrustedLocal, &signer).await; // AttachAck
        write_frame(
            &mut stream,
            &ClientFrame::UserMessage {
                text: "go".into(),
                images: vec![],
            },
        )
        .await
        .unwrap();
        // Immediately drop the client — detach mid-turn.
        drop(stream);

        // The daemon-owned turn still completes + persists.
        for _ in 0..100 {
            if let Ok(Some(conv)) = storage.load_conversation("detach-conv").await {
                if conv
                    .messages
                    .iter()
                    .any(|m| m.role == MessageRole::Assistant && m.content == "persisted anyway")
                {
                    shutdown.cancel();
                    h.abort();
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("turn did not survive client detach");
    }

    include!("approval_lifecycle_tests.rs");

    /// AC6: unattended (no writer) — a side-effecting (`Standard`) tool is
    /// denied-by-default and recorded; a read-only (`Safe`) tool auto-proceeds.
    /// No `PermissionMode` ever resolves to `Yolo`.
    #[tokio::test]
    async fn unattended_approval_denies_mutating_and_auto_allows_safe() {
        use crate::domain::models::tool_call::ApprovalSource;
        use crate::domain::models::{ApprovalOutcome, ToolRisk};

        let tmp = tempfile::tempdir().unwrap();
        let storage: Arc<dyn crate::domain::ports::StoragePort> =
            Arc::new(FileSystemStorage::with_workspace_root(
                crate::infrastructure::paths::sessions_dir(tmp.path()),
                tmp.path().to_path_buf(),
            ));
        let approval = ApprovalRuntime::new(64, Arc::new(NoOpApprovalPersistence));
        let registry = Arc::new(Mutex::new(ConnRegistry::default())); // no writer → unattended
        let blocked = Arc::new(AtomicUsize::new(0));
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "appr".into(),
            ..Default::default()
        }));

        let a = approval.clone();
        let r = registry.clone();
        let b = blocked.clone();
        let c = conversation.clone();
        let s = storage.clone();
        let events = a.subscribe();
        let owner = Arc::new(ApprovalGate::default());
        let replay = owner.replay.subscribe();
        let gate = tokio::spawn(async move {
            run_approval_gate(
                events,
                a,
                r,
                b,
                c,
                s,
                mpsc::unbounded_channel().0,
                Arc::new(Mutex::new(std::collections::HashMap::new())),
                owner,
                replay,
            )
            .await
        });

        let source = ApprovalSource::ForegroundTurn {
            conversation_id: "appr".into(),
        };

        // Mutating tool → deny-by-default (AC6 #3).
        let (_id, rx) = approval
            .request(
                source.clone(),
                "Write".into(),
                serde_json::json!({"file_path": "/etc/x"}),
                ToolRisk::Standard,
                None,
                None,
            )
            .await;
        let resolved = rx.await.unwrap();
        assert!(
            matches!(resolved.outcome, ApprovalOutcome::Reject { .. }),
            "mutating tool denied unattended, got {:?}",
            resolved.outcome
        );
        assert_eq!(
            blocked.load(Ordering::SeqCst),
            1,
            "blocked action counted (AC6 #5)"
        );

        // Safe tool → auto-proceed (read-only-auto).
        let (_id2, rx2) = approval
            .request(
                source,
                "Read".into(),
                serde_json::json!({"file_path": "/etc/x"}),
                ToolRisk::Safe,
                None,
                None,
            )
            .await;
        let resolved = rx2.await.unwrap();
        assert_eq!(
            resolved.resolution,
            crate::domain::models::PermissionResolution::Policy,
            "unattended Safe policy must not claim an operator answer"
        );
        assert!(
            matches!(resolved.outcome, ApprovalOutcome::Once),
            "Safe tool auto-proceeds unattended"
        );

        // AC6 #5: the denied action left a durable, visible transcript record.
        let conv = storage.load_conversation("appr").await.unwrap().unwrap();
        assert!(
            conv.messages.iter().any(|m| m.synthetic
                && m.content.contains("Skipped")
                && m.content.contains("Write")),
            "denied-while-unattended recorded a resumable transcript event"
        );
        gate.abort();
    }

    /// AC6.4 (Story 12.2c) — a SECOND attach is granted `ReadOnly` and the daemon
    /// REFUSES its write frames with `ProtocolError::ReadOnly`, end-to-end from the
    /// client side (re-asserting the 12.2b enforcement the rich client relies on).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn second_attach_is_readonly_and_writes_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let (core, _storage) = mock_core(ws, vec![]);
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "ro-conv".into(),
            ..Default::default()
        }));
        let (bus, domain_rx) = EventBus::new(64);
        let socket = ws.join("ro.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = AttachServer::new(core, conversation, bus.domain_tx.clone());
        let shutdown = CancellationToken::new();
        let srv = server.clone();
        let sd = shutdown.clone();
        let h = tokio::spawn(async move { srv.run(listener, domain_rx, None, None, sd).await });

        // Client 1 → writer.
        let mut c1 = UnixStream::connect(&socket).await.unwrap();
        let signer1 = test_signer(3);
        match attach_with_proof(&mut c1, false, ConnectionTier::TrustedLocal, &signer1).await {
            Some(DaemonFrame::AttachAck { granted_mode, .. }) => {
                assert_eq!(
                    granted_mode,
                    AttachMode::ReadWrite,
                    "first attach is writer"
                );
            }
            other => panic!("expected AttachAck, got {other:?}"),
        }

        // Client 2 → read-only (a writer already holds the slot).
        let mut c2 = UnixStream::connect(&socket).await.unwrap();
        let signer2 = test_signer(4);
        match attach_with_proof(&mut c2, false, ConnectionTier::TrustedLocal, &signer2).await {
            Some(DaemonFrame::AttachAck { granted_mode, .. }) => {
                assert_eq!(
                    granted_mode,
                    AttachMode::ReadOnly,
                    "second attach must be read-only"
                );
            }
            other => panic!("expected AttachAck(ReadOnly), got {other:?}"),
        }

        // The read-only client tries to write → daemon refuses with ReadOnly.
        write_frame(
            &mut c2,
            &ClientFrame::UserMessage {
                text: "i should be refused".into(),
                images: vec![],
            },
        )
        .await
        .unwrap();
        match read_frame::<_, DaemonFrame>(&mut c2).await.unwrap() {
            Some(DaemonFrame::Error(ProtocolError::ReadOnly)) => {}
            other => panic!("expected Error(ReadOnly), got {other:?}"),
        }

        shutdown.cancel();
        h.abort();
    }

    /// AC7 (Story 12.2c) — on writer-attach the daemon DRAINS the 12.1c purge
    /// notice and EMITS it as an Info `SystemNotice` event, plus a one-liner
    /// consolidation pointer. The purge notice is consumed (gone); the
    /// consolidation marker is NOT cleared (the durable hand-off to 12.2d).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn writer_attach_emits_and_drains_boundary_queues() {
        use crate::adapters::daemon::session_queue;
        use crate::domain::models::ConsolidationDueMarker;

        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();

        // Queue both 12.1c boundary records.
        session_queue::enqueue_purge_notice(ws, 5, vec!["a fact".into()], 1_000).unwrap();
        session_queue::enqueue_consolidation_due(
            ws,
            &ConsolidationDueMarker {
                boundary: "daily_reset".into(),
                queued_at_unix: 2_000,
                daily_log_ref: "2026-06-08".into(),
            },
        )
        .unwrap();

        let (core, _storage) = mock_core(ws, vec![]);
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "q-conv".into(),
            ..Default::default()
        }));
        let (bus, domain_rx) = EventBus::new(64);
        let socket = ws.join("q.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = AttachServer::new(core, conversation, bus.domain_tx.clone());
        let shutdown = CancellationToken::new();
        let srv = server.clone();
        let sd = shutdown.clone();
        let h = tokio::spawn(async move { srv.run(listener, domain_rx, None, None, sd).await });

        let mut c = UnixStream::connect(&socket).await.unwrap();
        let signer = test_signer(5);
        let _ = attach_with_proof(&mut c, false, ConnectionTier::TrustedLocal, &signer).await; // AttachAck

        // Collect emitted events. With NoOpMemory (empty recent), the
        // consolidation path emits "Nothing to consolidate yet" and clears
        // the marker (AC1 step 3 — empty-recent fast path).
        let mut saw_purge = false;
        let mut saw_consolidation_info = false;
        for _ in 0..4 {
            match tokio::time::timeout(
                std::time::Duration::from_millis(500),
                read_frame::<_, DaemonFrame>(&mut c),
            )
            .await
            {
                Ok(Ok(Some(DaemonFrame::Event(raw)))) => {
                    if let RawEventKind::SystemNotice { message, .. } = raw.kind {
                        if message.contains("facts removed from MEMORY.md") {
                            saw_purge = true;
                        }
                        if message.contains("Nothing to consolidate") {
                            saw_consolidation_info = true;
                        }
                    }
                }
                _ => break,
            }
            if saw_purge && saw_consolidation_info {
                break;
            }
        }
        assert!(saw_purge, "purge notice must be emitted on writer attach");
        assert!(
            saw_consolidation_info,
            "consolidation empty-recent info must be emitted on writer attach"
        );

        // Both markers cleared (purge by emit+drain; consolidation by empty-recent fast path).
        assert!(
            session_queue::read_purge_notice(ws).is_none(),
            "purge notice must be drained after emit"
        );
        assert!(
            session_queue::read_consolidation_due(ws).is_none(),
            "consolidation marker must be cleared (empty-recent fast path)"
        );

        shutdown.cancel();
        h.abort();
    }

    #[tokio::test]
    async fn purge_notice_stays_queued_when_attach_notice_delivery_fails() {
        use crate::adapters::daemon::session_queue;

        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        session_queue::enqueue_purge_notice(ws, 2, vec!["lost if removed".into()], 1_000).unwrap();

        let (core, _storage) = mock_core(ws, vec![]);
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "q-conv".into(),
            ..Default::default()
        }));
        let (bus, _domain_rx) = EventBus::new(64);
        let server = AttachServer::new(core, conversation, bus.domain_tx.clone());

        server.emit_session_queue_notices(404).await;

        assert!(
            session_queue::read_purge_notice(ws).is_some(),
            "failed delivery must not consume the durable purge notice"
        );
    }
    // ── Story 12.2d gauntlet tests (G1-G14, G16) ────────────────────────────

    /// Helper: set up an AttachServer + a fake writer conn + inject a retained proposal.
    async fn setup_consolidation_test(
        ws: &Path,
    ) -> (
        Arc<AttachServer>,
        mpsc::Receiver<DaemonFrame>,
        u64, /* conn_id of the writer */
    ) {
        setup_consolidation_test_with_memory(ws, Arc::new(NoOpMemory)).await
    }

    async fn setup_consolidation_test_with_memory(
        ws: &Path,
        memory: Arc<dyn crate::domain::ports::MemoryPort>,
    ) -> (
        Arc<AttachServer>,
        mpsc::Receiver<DaemonFrame>,
        u64, /* conn_id of the writer */
    ) {
        let (core, _storage) = mock_core_with_memory(ws, vec![], memory);
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "g-conv".into(),
            ..Default::default()
        }));
        let (bus, _domain_rx) = EventBus::new(64);
        let server = AttachServer::new(core, conversation, bus.domain_tx.clone());

        // Register a writer connection.
        let (tx, rx) = mpsc::channel(8);
        let conn_id = 99u64;
        {
            let mut reg = server.registry.lock().await;
            reg.conns.push(Conn {
                id: conn_id,
                tx,
                terminal_tx: watch::channel(None).0,
                mode: AttachMode::ReadWrite,
                tier: ConnectionTier::TrustedLocal,
                approval_ready: true,
            });
        }
        (server, rx, conn_id)
    }

    /// Story 18.4a code-review D3: `peer share` is a daemon-owned
    /// trusted-local mutation. The protocol frame must reach the daemon-side
    /// resolver (not silently fall through), and an unknown alias is refused
    /// before a peer transport could be used.
    #[tokio::test]
    async fn peer_share_frame_reaches_daemon_and_refuses_unknown_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let (server, mut rx, conn_id) = setup_consolidation_test(tmp.path()).await;

        server
            .handle_client_frame_tiered(
                ClientFrame::PeerShare {
                    alias: "missing".into(),
                    artifact: "artifact-1".into(),
                    topic: "architecture".into(),
                    summary: None,
                },
                AttachMode::ReadWrite,
                ConnectionTier::TrustedLocal,
                conn_id,
            )
            .await;
        match rx.recv().await {
            Some(DaemonFrame::PeerShareResult {
                outcome: crate::adapters::daemon::protocol::PeerShareOutcome::Refused { reason },
            }) => assert!(
                reason.contains("No peer named 'missing'"),
                "the daemon must resolve and refuse before any dial: {reason}"
            ),
            other => panic!("expected PeerShareResult::Refused, got {other:?}"),
        }

        server
            .handle_client_frame_tiered(
                ClientFrame::PeerShare {
                    alias: "missing".into(),
                    artifact: "artifact-1".into(),
                    topic: "architecture".into(),
                    summary: None,
                },
                AttachMode::ReadOnly,
                ConnectionTier::TrustedLocal,
                conn_id,
            )
            .await;
        assert!(matches!(
            rx.recv().await,
            Some(DaemonFrame::Error(ProtocolError::ReadOnly))
        ));
    }

    /// Wrap bare `MemoryFact`s into the wire `ProposedFact` shape (Story 12.2d Fork-C),
    /// minting per-item ids the same way the daemon generation task does (`.enumerate()`).
    fn proposed(
        facts: Vec<crate::domain::models::MemoryFact>,
    ) -> Vec<crate::adapters::daemon::protocol::ProposedFact> {
        use crate::adapters::daemon::protocol::{ProposalId, ProposedFact};
        facts
            .into_iter()
            .enumerate()
            .map(|(i, fact)| ProposedFact {
                id: ProposalId(i as u32),
                fact,
            })
            .collect()
    }

    /// G1 — daemon-authoritative resolution: resolve {accept:true} → daemon writes
    /// its OWN retained facts; client frame carries no fact payload.
    #[tokio::test]
    async fn g1_daemon_authoritative_resolution_writes_retained_facts() {
        use crate::domain::models::MemoryFact;

        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let (server, mut rx, conn_id) = setup_consolidation_test(ws).await;

        // Inject a retained consolidation.
        let facts = vec![MemoryFact {
            category: "Preference".into(),
            fact: "likes dark mode".into(),
            detail: Some("always".into()),
        }];
        {
            let mut map = server.retained_consolidations.lock().await;
            map.insert(
                2000,
                RetainedConsolidation {
                    token: ProposalToken(42),
                    proposals: proposed(facts.clone()),
                },
            );
        }

        // Set a consolidation marker so clear_consolidation_due has something to clear.
        session_queue::enqueue_consolidation_due(
            ws,
            &crate::domain::models::ConsolidationDueMarker {
                boundary: "daily_reset".into(),
                queued_at_unix: 2000,
                daily_log_ref: "2026-06-08".into(),
            },
        )
        .unwrap();

        // Resolve with accept=true.
        server
            .handle_client_frame(
                ClientFrame::ConsolidationResolve {
                    token: ProposalToken(42),
                    accept: true,
                },
                AttachMode::ReadWrite,
                conn_id,
            )
            .await;

        // Daemon should have sent a "Promoted" notice.
        let frame = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .unwrap()
            .unwrap();
        if let DaemonFrame::Event(raw) = frame {
            if let RawEventKind::SystemNotice { message, .. } = raw.kind {
                assert!(
                    message.contains("Promoted"),
                    "expected promote notice, got: {message}"
                );
            } else {
                panic!("expected SystemNotice, got {:?}", raw.kind);
            }
        } else {
            panic!("expected Event frame, got {frame:?}");
        }

        // Marker cleared.
        assert!(
            session_queue::read_consolidation_due(ws).is_none(),
            "marker must be cleared after accept"
        );
        // Retained entry evicted.
        let map = server.retained_consolidations.lock().await;
        assert!(
            map.is_empty(),
            "retained entry must be evicted after resolve"
        );
    }

    /// G3 — stale/unknown token rejected, no write.
    #[tokio::test]
    async fn g3_stale_resolve_rejected_no_write() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let (server, mut rx, conn_id) = setup_consolidation_test(ws).await;

        // No retained entries exist. Send resolve with a random token.
        server
            .handle_client_frame(
                ClientFrame::ConsolidationResolve {
                    token: ProposalToken(9999),
                    accept: true,
                },
                AttachMode::ReadWrite,
                conn_id,
            )
            .await;

        let frame = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .unwrap()
            .unwrap();

        // Must be an error, not a promote.
        match frame {
            DaemonFrame::Error(ProtocolError::Internal(msg)) => {
                assert!(
                    msg.contains("stale") || msg.contains("unknown"),
                    "expected stale/unknown token error, got: {msg}"
                );
            }
            other => panic!("expected Internal error for stale token, got: {other:?}"),
        }
    }

    /// G10 — secret-laden fact is skipped at promotion.
    #[tokio::test]
    async fn g10_secret_fact_skipped_on_apply() {
        use crate::domain::models::MemoryFact;

        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let (server, mut rx, conn_id) = setup_consolidation_test(ws).await;
        let secret_fact = MemoryFact {
            category: "Note".into(),
            fact: "my api key".into(),
            detail: Some("sk-proj-1234567890abcdef1234567890abcdef12".into()),
        };
        let safe_fact = MemoryFact {
            category: "Preference".into(),
            fact: "likes tea".into(),
            detail: None,
        };

        {
            let mut map = server.retained_consolidations.lock().await;
            map.insert(
                3000,
                RetainedConsolidation {
                    token: ProposalToken(10),
                    proposals: proposed(vec![secret_fact, safe_fact]),
                },
            );
        }

        session_queue::enqueue_consolidation_due(
            ws,
            &crate::domain::models::ConsolidationDueMarker {
                boundary: "daily_reset".into(),
                queued_at_unix: 3000,
                daily_log_ref: "2026-06-08".into(),
            },
        )
        .unwrap();

        server
            .handle_client_frame(
                ClientFrame::ConsolidationResolve {
                    token: ProposalToken(10),
                    accept: true,
                },
                AttachMode::ReadWrite,
                conn_id,
            )
            .await;

        let frame = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .unwrap()
            .unwrap();
        if let DaemonFrame::Event(raw) = frame {
            if let RawEventKind::SystemNotice { message, .. } = raw.kind {
                // 1 promoted, 1 skipped (the secret one).
                assert!(
                    message.contains("Promoted 1") && message.contains("1 skipped"),
                    "expected 'Promoted 1 facts (1 skipped)', got: {message}"
                );
            } else {
                panic!("expected SystemNotice");
            }
        }
    }

    /// G11 — read-only rejects the mutation frame.
    #[tokio::test]
    async fn g11_read_only_rejects_consolidation_resolve() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let (server, _rx, _conn_id) = setup_consolidation_test(ws).await;

        // Register a read-only connection.
        let (ro_tx, mut ro_rx) = mpsc::channel(8);
        {
            let mut reg = server.registry.lock().await;
            reg.conns.push(Conn {
                id: 100,
                tx: ro_tx,
                terminal_tx: watch::channel(None).0,
                mode: AttachMode::ReadOnly,
                tier: ConnectionTier::TrustedLocal,
                approval_ready: true,
            });
        }

        server
            .handle_client_frame(
                ClientFrame::ConsolidationResolve {
                    token: ProposalToken(1),
                    accept: true,
                },
                AttachMode::ReadOnly,
                100,
            )
            .await;

        let frame = tokio::time::timeout(std::time::Duration::from_millis(500), ro_rx.recv())
            .await
            .unwrap()
            .unwrap();

        assert!(
            matches!(frame, DaemonFrame::Error(ProtocolError::ReadOnly)),
            "read-only conn must get ReadOnly error, got: {frame:?}"
        );
    }

    /// G13 — decline clears marker, no writes.
    #[tokio::test]
    async fn g13_decline_clears_marker_no_write() {
        use crate::domain::models::MemoryFact;

        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let (server, mut rx, conn_id) = setup_consolidation_test(ws).await;

        {
            let mut map = server.retained_consolidations.lock().await;
            map.insert(
                4000,
                RetainedConsolidation {
                    token: ProposalToken(20),
                    proposals: proposed(vec![MemoryFact {
                        category: "Test".into(),
                        fact: "should not be written".into(),
                        detail: None,
                    }]),
                },
            );
        }

        session_queue::enqueue_consolidation_due(
            ws,
            &crate::domain::models::ConsolidationDueMarker {
                boundary: "daily_reset".into(),
                queued_at_unix: 4000,
                daily_log_ref: "2026-06-08".into(),
            },
        )
        .unwrap();

        server
            .handle_client_frame(
                ClientFrame::ConsolidationResolve {
                    token: ProposalToken(20),
                    accept: false,
                },
                AttachMode::ReadWrite,
                conn_id,
            )
            .await;

        let frame = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .unwrap()
            .unwrap();
        if let DaemonFrame::Event(raw) = frame {
            if let RawEventKind::SystemNotice { message, .. } = raw.kind {
                assert!(
                    message.contains("declined"),
                    "expected decline notice, got: {message}"
                );
            }
        }

        // Marker cleared.
        assert!(
            session_queue::read_consolidation_due(ws).is_none(),
            "marker must be cleared on decline"
        );
        // Retained entry evicted.
        let map = server.retained_consolidations.lock().await;
        assert!(map.is_empty(), "retained entry must be evicted on decline");
    }

    /// G2 — resolution token binds the set: only the matching token's proposals apply.
    #[tokio::test]
    async fn g2_token_binds_resolution_set() {
        use crate::domain::models::MemoryFact;

        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let (server, mut rx, conn_id) = setup_consolidation_test(ws).await;

        // Two retained entries with different tokens.
        {
            let mut map = server.retained_consolidations.lock().await;
            map.insert(
                5000,
                RetainedConsolidation {
                    token: ProposalToken(50),
                    proposals: proposed(vec![MemoryFact {
                        category: "A".into(),
                        fact: "token-50-fact".into(),
                        detail: None,
                    }]),
                },
            );
            map.insert(
                5001,
                RetainedConsolidation {
                    token: ProposalToken(51),
                    proposals: proposed(vec![MemoryFact {
                        category: "B".into(),
                        fact: "token-51-fact".into(),
                        detail: None,
                    }]),
                },
            );
        }

        session_queue::enqueue_consolidation_due(
            ws,
            &crate::domain::models::ConsolidationDueMarker {
                boundary: "daily_reset".into(),
                queued_at_unix: 5000,
                daily_log_ref: "2026-06-08".into(),
            },
        )
        .unwrap();

        // Resolve with token 51 — only that entry should be applied and evicted.
        server
            .handle_client_frame(
                ClientFrame::ConsolidationResolve {
                    token: ProposalToken(51),
                    accept: true,
                },
                AttachMode::ReadWrite,
                conn_id,
            )
            .await;

        let frame = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .unwrap()
            .unwrap();
        if let DaemonFrame::Event(raw) = frame {
            if let RawEventKind::SystemNotice { message, .. } = raw.kind {
                assert!(
                    message.contains("Promoted 1"),
                    "expected 1 promoted: {message}"
                );
            }
        }

        // Token 50's entry must still be retained.
        let map = server.retained_consolidations.lock().await;
        assert!(
            map.contains_key(&5000),
            "unresolved token's entry must remain"
        );
        assert!(
            !map.contains_key(&5001),
            "resolved token's entry must be evicted"
        );
    }

    /// D1 (Fork-C, Murat's required gate) — serde round-trip of `ConsolidationProposed`
    /// with TWO DISTINCT `ProposalId`s asserts the id↦fact PAIRING survives the wire
    /// (order-independent, by id). A vacuous round-trip that drops/reorders the id-fact
    /// association fails here — the non-vacuous proof the per-item handle is real.
    #[test]
    fn d1_consolidation_proposed_serde_preserves_id_fact_pairing() {
        use crate::adapters::daemon::protocol::{ProposalId, ProposalToken, ProposedFact};
        use crate::domain::models::MemoryFact;

        let frame = DaemonFrame::ConsolidationProposed {
            token: ProposalToken(7),
            proposals: vec![
                ProposedFact {
                    id: ProposalId(11),
                    fact: MemoryFact {
                        category: "A".into(),
                        fact: "alpha".into(),
                        detail: None,
                    },
                },
                ProposedFact {
                    id: ProposalId(22),
                    fact: MemoryFact {
                        category: "B".into(),
                        fact: "beta".into(),
                        detail: Some("d".into()),
                    },
                },
            ],
        };

        let bytes = serde_json::to_vec(&frame).unwrap();
        let decoded: DaemonFrame = serde_json::from_slice(&bytes).unwrap();
        let DaemonFrame::ConsolidationProposed { token, proposals } = decoded else {
            panic!("expected ConsolidationProposed, got {decoded:?}");
        };
        assert_eq!(token, ProposalToken(7));
        let by_id: std::collections::HashMap<u32, &ProposedFact> =
            proposals.iter().map(|p| (p.id.0, p)).collect();
        assert_eq!(by_id.len(), 2, "both distinct ids must survive the wire");
        assert_eq!(
            by_id[&11].fact.fact, "alpha",
            "id 11 must still pair with alpha"
        );
        assert_eq!(
            by_id[&22].fact.fact, "beta",
            "id 22 must still pair with beta"
        );
        assert_eq!(by_id[&22].fact.detail.as_deref(), Some("d"));
    }

    /// P1 (code review, HIGH) — a resolve clears ONLY the marker it was generated from.
    /// If a NEWER boundary marker landed between card-shown and resolve, it must SURVIVE.
    #[tokio::test]
    async fn p1_resolve_clears_only_the_resolved_marker() {
        use crate::domain::models::MemoryFact;

        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let (server, mut rx, conn_id) = setup_consolidation_test(ws).await;

        // Retained entry generated from marker @ queued_at = 2000.
        {
            let mut map = server.retained_consolidations.lock().await;
            map.insert(
                2000,
                RetainedConsolidation {
                    token: ProposalToken(70),
                    proposals: proposed(vec![MemoryFact {
                        category: "P".into(),
                        fact: "old".into(),
                        detail: None,
                    }]),
                },
            );
        }
        // A NEWER boundary marker (queued_at = 9999) is on disk by resolve time.
        session_queue::enqueue_consolidation_due(
            ws,
            &crate::domain::models::ConsolidationDueMarker {
                boundary: "daily_reset".into(),
                queued_at_unix: 9999,
                daily_log_ref: "2026-06-09".into(),
            },
        )
        .unwrap();

        server
            .handle_client_frame(
                ClientFrame::ConsolidationResolve {
                    token: ProposalToken(70),
                    accept: true,
                },
                AttachMode::ReadWrite,
                conn_id,
            )
            .await;
        let _ = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await;

        let m = session_queue::read_consolidation_due(ws);
        assert!(
            m.is_some(),
            "a newer marker must NOT be deleted by a stale resolve"
        );
        assert_eq!(
            m.unwrap().queued_at_unix,
            9999,
            "the surviving marker must be the newer one"
        );
    }

    /// P2 / G9 (code review, HIGH) — a transient `remember_fact` write error must PRESERVE
    /// the marker (re-propose on next attach, never silent-lose). The inverse of the
    /// silent-data-loss G9 guards: marker-cleared + nothing-written must be impossible.
    struct FailingMemory;
    #[async_trait::async_trait]
    impl crate::domain::ports::MemoryPort for FailingMemory {
        async fn remember_fact(
            &self,
            _fact: crate::domain::models::MemoryFact,
        ) -> Result<(), crate::domain::errors::MemoryError> {
            Err(crate::domain::errors::MemoryError::IoError(
                "disk full".into(),
            ))
        }
    }

    #[tokio::test]
    async fn p2_write_error_preserves_marker_for_retry() {
        use crate::domain::models::MemoryFact;

        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let (server, mut rx, conn_id) =
            setup_consolidation_test_with_memory(ws, Arc::new(FailingMemory)).await;

        {
            let mut map = server.retained_consolidations.lock().await;
            map.insert(
                2000,
                RetainedConsolidation {
                    token: ProposalToken(80),
                    proposals: proposed(vec![MemoryFact {
                        category: "X".into(),
                        fact: "keep me".into(),
                        detail: None,
                    }]),
                },
            );
        }
        session_queue::enqueue_consolidation_due(
            ws,
            &crate::domain::models::ConsolidationDueMarker {
                boundary: "daily_reset".into(),
                queued_at_unix: 2000,
                daily_log_ref: "2026-06-09".into(),
            },
        )
        .unwrap();

        server
            .handle_client_frame(
                ClientFrame::ConsolidationResolve {
                    token: ProposalToken(80),
                    accept: true,
                },
                AttachMode::ReadWrite,
                conn_id,
            )
            .await;
        let _ = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await;

        assert!(
            session_queue::read_consolidation_due(ws).is_some(),
            "marker must survive a remember_fact write error so the next attach retries"
        );
    }

    /// G7 (code review) — retain-reuse-no-respend: with a retained entry already present
    /// for the marker's `queued_at_unix`, `emit_session_queue_notices` re-emits the SAME
    /// token via the reuse branch (zero generation), even across repeated attaches.
    #[tokio::test]
    async fn g7_reuse_emits_same_token_no_respend() {
        use crate::domain::models::MemoryFact;

        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let (server, mut rx, conn_id) = setup_consolidation_test(ws).await;

        {
            let mut map = server.retained_consolidations.lock().await;
            map.insert(
                6000,
                RetainedConsolidation {
                    token: ProposalToken(60),
                    proposals: proposed(vec![MemoryFact {
                        category: "R".into(),
                        fact: "reused".into(),
                        detail: None,
                    }]),
                },
            );
        }
        session_queue::enqueue_consolidation_due(
            ws,
            &crate::domain::models::ConsolidationDueMarker {
                boundary: "daily_reset".into(),
                queued_at_unix: 6000,
                daily_log_ref: "2026-06-09".into(),
            },
        )
        .unwrap();

        // Two successive attaches → both must re-emit the SAME retained token, no respend.
        for _ in 0..2 {
            server.emit_session_queue_notices(conn_id).await;
            let frame = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
                .await
                .unwrap()
                .unwrap();
            match frame {
                DaemonFrame::ConsolidationProposed { token, proposals } => {
                    assert_eq!(
                        token,
                        ProposalToken(60),
                        "reuse must re-emit the same token"
                    );
                    assert_eq!(proposals.len(), 1);
                    assert_eq!(proposals[0].fact.fact, "reused");
                }
                other => panic!("expected reused ConsolidationProposed, got: {other:?}"),
            }
        }

        // Still exactly one retained entry — no second generation occurred.
        let map = server.retained_consolidations.lock().await;
        assert_eq!(
            map.len(),
            1,
            "reuse must not create a second retained entry"
        );
        assert!(map.contains_key(&6000));
    }

    /// CronCompletion received via `cron_completion_rx` is injected into the
    /// shared conversation with `ChannelKind::Cron` origin and `[cron: name]`
    /// prefix. The forwarder is the single writer, so no interleaving.
    #[cfg(feature = "cron")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cron_completion_injects_into_conversation() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        // No provider chunks needed — cron completion is injected directly.
        let (core, storage) = mock_core(ws, vec![]);
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "cron-daemon-conv".into(),
            ..Default::default()
        }));

        let (bus, domain_rx) = EventBus::new(64);
        let domain_tx = bus.domain_tx.clone();
        let (cron_tx, cron_rx) = mpsc::unbounded_channel::<CronCompletion>();
        let socket = ws.join("cron.sock");
        let listener = UnixListener::bind(&socket).unwrap();

        let server = AttachServer::new(core, conversation.clone(), domain_tx);
        let shutdown = CancellationToken::new();
        let srv = server.clone();
        let sd = shutdown.clone();
        let handle =
            tokio::spawn(
                async move { srv.run(listener, domain_rx, None, Some(cron_rx), sd).await },
            );

        // Send a cron completion.
        cron_tx
            .send(CronCompletion {
                job_name: "morning-briefing".into(),
                result_text: "3 commits yesterday".into(),
            })
            .unwrap();

        // Wait for the completion to be committed to the conversation.
        for _ in 0..50 {
            if let Ok(Some(conv)) = storage.load_conversation("cron-daemon-conv").await {
                if let Some(asst) = conv
                    .messages
                    .iter()
                    .find(|m| m.role == MessageRole::Assistant)
                {
                    assert!(
                        asst.content.contains("[cron: morning-briefing]"),
                        "assistant content must include cron prefix: {}",
                        asst.content
                    );
                    assert!(
                        asst.content.contains("3 commits yesterday"),
                        "assistant content must include result text: {}",
                        asst.content
                    );
                    assert_eq!(
                        asst.origin,
                        ChannelKind::Cron,
                        "cron completion must be tagged with Cron origin (AC5)"
                    );
                    shutdown.cancel();
                    handle.abort();
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        shutdown.cancel();
        handle.abort();
        panic!("cron completion was not injected into the conversation");
    }

    /// Multiple cron completions arrive as separate messages in the shared
    /// conversation — each with its own [cron: name] prefix and Cron origin.
    #[cfg(feature = "cron")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn multiple_cron_completions_are_sequential_messages() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let (core, storage) = mock_core(ws, vec![]);
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "cron-multi-conv".into(),
            ..Default::default()
        }));

        let (bus, domain_rx) = EventBus::new(64);
        let domain_tx = bus.domain_tx.clone();
        let (cron_tx, cron_rx) = mpsc::unbounded_channel::<CronCompletion>();
        let socket = ws.join("cron-multi.sock");
        let listener = UnixListener::bind(&socket).unwrap();

        let server = AttachServer::new(core, conversation.clone(), domain_tx);
        let shutdown = CancellationToken::new();
        let srv = server.clone();
        let sd = shutdown.clone();
        let handle =
            tokio::spawn(
                async move { srv.run(listener, domain_rx, None, Some(cron_rx), sd).await },
            );

        // Send two completions.
        cron_tx
            .send(CronCompletion {
                job_name: "job-a".into(),
                result_text: "result-a".into(),
            })
            .unwrap();
        cron_tx
            .send(CronCompletion {
                job_name: "job-b".into(),
                result_text: "result-b".into(),
            })
            .unwrap();

        // Wait for both to appear.
        for _ in 0..100 {
            if let Ok(Some(conv)) = storage.load_conversation("cron-multi-conv").await {
                let cron_msgs: Vec<_> = conv
                    .messages
                    .iter()
                    .filter(|m| m.origin == ChannelKind::Cron)
                    .collect();
                if cron_msgs.len() >= 2 {
                    assert!(
                        cron_msgs[0].content.contains("[cron: job-a]"),
                        "first cron message: {}",
                        cron_msgs[0].content
                    );
                    assert!(
                        cron_msgs[1].content.contains("[cron: job-b]"),
                        "second cron message: {}",
                        cron_msgs[1].content
                    );
                    shutdown.cancel();
                    handle.abort();
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        shutdown.cancel();
        handle.abort();
        panic!("two cron completions were not injected");
    }
    // ── Story 17.1a attach-proof acceptance ──────────────────────────────────

    /// AC: a valid TrustedLocal proof grants the writer slot.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn valid_trusted_local_proof_grants_writer() {
        let tmp = tempfile::tempdir().unwrap();
        let (_server, socket, shutdown, handle) =
            spawn_proof_server(tmp.path(), "tl.sock", None).await;

        let mut stream = UnixStream::connect(&socket).await.unwrap();
        let signer = test_signer(11);
        match attach_with_proof(&mut stream, false, ConnectionTier::TrustedLocal, &signer).await {
            Some(DaemonFrame::AttachAck { granted_mode, .. }) => {
                assert_eq!(granted_mode, AttachMode::ReadWrite);
            }
            other => panic!("valid TrustedLocal proof must be accepted, got {other:?}"),
        }
        shutdown.cancel();
        handle.abort();
    }

    /// AC: a valid Peer proof attaches (read-only) and a signed peer envelope is
    /// accepted with PeerAccepted.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn valid_peer_proof_accepts_signed_envelope() {
        let tmp = tempfile::tempdir().unwrap();
        let (_server, socket, shutdown, handle) =
            spawn_proof_server(tmp.path(), "peer.sock", None).await;

        let mut stream = UnixStream::connect(&socket).await.unwrap();
        let signer = test_signer(12);
        match attach_with_proof(&mut stream, false, ConnectionTier::Peer, &signer).await {
            Some(DaemonFrame::AttachAck { granted_mode, .. }) => {
                assert_eq!(granted_mode, AttachMode::ReadOnly, "peer tier is read-only");
            }
            other => panic!("valid Peer proof must be accepted, got {other:?}"),
        }

        // A properly signed envelope is verified and accepted.
        let env = peer_envelope(&signer, 1, i64::MAX);
        write_frame(&mut stream, &ClientFrame::PeerEnvelope(env))
            .await
            .unwrap();
        match read_control_frame(&mut stream).await {
            Some(DaemonFrame::PeerAccepted { sequence }) => assert_eq!(sequence, 1),
            other => panic!("expected PeerAccepted, got {other:?}"),
        }
        shutdown.cancel();
        handle.abort();
    }

    /// AC: an unsigned application frame sent BEFORE any proof is rejected — the
    /// server demands an Attach first. Local unsigned frames work ONLY after a
    /// valid proof (the positive direction is exercised by
    /// `attach_drives_a_turn_and_persists_origin_tagged_transcript`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unsigned_frame_rejected_before_proof() {
        let tmp = tempfile::tempdir().unwrap();
        let (_server, socket, shutdown, handle) =
            spawn_proof_server(tmp.path(), "pre.sock", None).await;

        let mut stream = UnixStream::connect(&socket).await.unwrap();
        // Consume the challenge, then send a raw unsigned UserMessage as the
        // first frame instead of an Attach.
        let _ = read_attach_challenge(&mut stream).await;
        write_frame(
            &mut stream,
            &ClientFrame::UserMessage {
                text: "no proof".into(),
                images: vec![],
            },
        )
        .await
        .unwrap();
        match read_frame::<_, DaemonFrame>(&mut stream).await.unwrap() {
            Some(DaemonFrame::Error(ProtocolError::Malformed(_))) => {}
            other => panic!("expected Malformed (first frame must be Attach), got {other:?}"),
        }
        shutdown.cancel();
        handle.abort();
    }

    /// AC: an absent (all-zero) proof is rejected before the connection is
    /// registered — proven by a subsequent valid attach still winning the writer
    /// slot (the rejected connection never registered).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn absent_proof_rejected_before_registration() {
        let tmp = tempfile::tempdir().unwrap();
        let (_server, socket, shutdown, handle) =
            spawn_proof_server(tmp.path(), "absent.sock", None).await;

        // Connection 1: absent proof.
        let mut bad = UnixStream::connect(&socket).await.unwrap();
        let nonce = read_attach_challenge(&mut bad).await;
        write_frame(
            &mut bad,
            &ClientFrame::Attach {
                protocol_version: PROTOCOL_VERSION,
                read_only_ok: false,
                tier: ConnectionTier::TrustedLocal,
                challenge_nonce: nonce,
                identity: test_signer(21).identity().clone(),
                proof: crate::domain::models::Ed25519Sig(vec![0u8; 64]),
            },
        )
        .await
        .unwrap();
        match read_frame::<_, DaemonFrame>(&mut bad).await.unwrap() {
            Some(DaemonFrame::Error(ProtocolError::AttachProof(_))) => {}
            other => panic!("expected AttachProof error, got {other:?}"),
        }
        drop(bad);

        // Connection 2: a valid proof must still get ReadWrite — conn1 never
        // registered, so it never claimed the writer slot.
        let mut good = UnixStream::connect(&socket).await.unwrap();
        match attach_with_proof(
            &mut good,
            false,
            ConnectionTier::TrustedLocal,
            &test_signer(22),
        )
        .await
        {
            Some(DaemonFrame::AttachAck { granted_mode, .. }) => {
                assert_eq!(
                    granted_mode,
                    AttachMode::ReadWrite,
                    "rejected attach must not have claimed the writer slot"
                );
            }
            other => panic!("valid attach after a rejection must succeed, got {other:?}"),
        }
        shutdown.cancel();
        handle.abort();
    }

    /// AC: a tampered proof (one flipped signature byte) is rejected.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tampered_proof_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let (_server, socket, shutdown, handle) =
            spawn_proof_server(tmp.path(), "tamper.sock", None).await;

        let mut stream = UnixStream::connect(&socket).await.unwrap();
        let signer = test_signer(23);
        let nonce = read_attach_challenge(&mut stream).await;
        let mut proof = signer.attach_proof(
            &nonce,
            PROTOCOL_VERSION,
            ConnectionTier::TrustedLocal.proof_tag(),
            false,
        );
        proof.0[0] ^= 0xff; // flip one signature byte
        write_frame(
            &mut stream,
            &ClientFrame::Attach {
                protocol_version: PROTOCOL_VERSION,
                read_only_ok: false,
                tier: ConnectionTier::TrustedLocal,
                challenge_nonce: nonce,
                identity: signer.identity().clone(),
                proof,
            },
        )
        .await
        .unwrap();
        match read_frame::<_, DaemonFrame>(&mut stream).await.unwrap() {
            Some(DaemonFrame::Error(ProtocolError::AttachProof(_))) => {}
            other => panic!("expected AttachProof error for tampered proof, got {other:?}"),
        }
        shutdown.cancel();
        handle.abort();
    }

    /// AC: a proof minted for the TrustedLocal tier but presented under a Peer
    /// claim is rejected — the transcript's tier tag is bound, so it cannot be
    /// silently swapped for another tier.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tier_mismatched_proof_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let (_server, socket, shutdown, handle) =
            spawn_proof_server(tmp.path(), "tier.sock", None).await;

        let mut stream = UnixStream::connect(&socket).await.unwrap();
        let signer = test_signer(24);
        let nonce = read_attach_challenge(&mut stream).await;
        // Sign the transcript for TrustedLocal, but declare tier = Peer.
        let proof = signer.attach_proof(
            &nonce,
            PROTOCOL_VERSION,
            ConnectionTier::TrustedLocal.proof_tag(),
            false,
        );
        write_frame(
            &mut stream,
            &ClientFrame::Attach {
                protocol_version: PROTOCOL_VERSION,
                read_only_ok: false,
                tier: ConnectionTier::Peer,
                challenge_nonce: nonce,
                identity: signer.identity().clone(),
                proof,
            },
        )
        .await
        .unwrap();
        match read_frame::<_, DaemonFrame>(&mut stream).await.unwrap() {
            Some(DaemonFrame::Error(ProtocolError::AttachProof(_))) => {}
            other => panic!("expected AttachProof error for tier-mismatched proof, got {other:?}"),
        }
        shutdown.cancel();
        handle.abort();
    }

    /// AC: a captured attach cannot be replayed on a new connection. Connection 1
    /// captures a (nonce, proof) pair; connection 2 receives a DIFFERENT nonce, so
    /// replaying the old pair fails the nonce-binding check before the signature
    /// is even examined.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn replayed_attach_proof_rejected_on_reconnect() {
        let tmp = tempfile::tempdir().unwrap();
        let (_server, socket, shutdown, handle) =
            spawn_proof_server(tmp.path(), "replay.sock", None).await;

        // Connection 1: capture the challenge nonce + build a proof for it.
        let mut c1 = UnixStream::connect(&socket).await.unwrap();
        let nonce1 = read_attach_challenge(&mut c1).await;
        let signer = test_signer(25);
        let proof1 = signer.attach_proof(
            &nonce1,
            PROTOCOL_VERSION,
            ConnectionTier::TrustedLocal.proof_tag(),
            false,
        );
        let captured = ClientFrame::Attach {
            protocol_version: PROTOCOL_VERSION,
            read_only_ok: false,
            tier: ConnectionTier::TrustedLocal,
            challenge_nonce: nonce1.clone(),
            identity: signer.identity().clone(),
            proof: proof1,
        };
        // (Don't complete conn1's handshake; the point is replaying the frame.)
        drop(c1);

        // Connection 2: a fresh challenge is issued.
        let mut c2 = UnixStream::connect(&socket).await.unwrap();
        let nonce2 = read_attach_challenge(&mut c2).await;
        assert_ne!(nonce2, nonce1, "each connection must get a distinct nonce");
        // Replay the captured frame verbatim — its nonce (nonce1) != nonce2.
        write_frame(&mut c2, &captured).await.unwrap();
        match read_frame::<_, DaemonFrame>(&mut c2).await.unwrap() {
            Some(DaemonFrame::Error(ProtocolError::AttachProof(m))) => {
                assert!(
                    m.contains("nonce"),
                    "rejection must cite the nonce mismatch: {m}"
                );
            }
            other => panic!("expected AttachProof(nonce mismatch) on replay, got {other:?}"),
        }
        shutdown.cancel();
        handle.abort();
    }

    /// AC: the SERVER-WIDE replay window rejects an envelope replayed on a
    /// reconnect (sequence <= highest seen on a prior connection).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn peer_envelope_replay_rejected_across_reconnect() {
        let tmp = tempfile::tempdir().unwrap();
        let (_server, socket, shutdown, handle) =
            spawn_proof_server(tmp.path(), "penv.sock", None).await;
        let signer = test_signer(26);

        // Connection 1: attach + accept envelope seq=1.
        let mut c1 = UnixStream::connect(&socket).await.unwrap();
        let _ = attach_with_proof(&mut c1, false, ConnectionTier::Peer, &signer).await;
        let env = peer_envelope(&signer, 1, i64::MAX);
        write_frame(&mut c1, &ClientFrame::PeerEnvelope(env))
            .await
            .unwrap();
        match read_control_frame(&mut c1).await {
            Some(DaemonFrame::PeerAccepted { sequence }) => assert_eq!(sequence, 1),
            other => panic!("first envelope must be accepted, got {other:?}"),
        }
        drop(c1);

        // Connection 2 (reconnect): the SAME envelope seq=1 is a replay.
        let mut c2 = UnixStream::connect(&socket).await.unwrap();
        let _ = attach_with_proof(&mut c2, false, ConnectionTier::Peer, &signer).await;
        let replayed = peer_envelope(&signer, 1, i64::MAX);
        write_frame(&mut c2, &ClientFrame::PeerEnvelope(replayed))
            .await
            .unwrap();
        match read_frame::<_, DaemonFrame>(&mut c2).await.unwrap() {
            Some(DaemonFrame::Error(ProtocolError::PeerVerification(_))) => {}
            other => panic!("replayed envelope must be rejected, got {other:?}"),
        }
        shutdown.cancel();
        handle.abort();
    }

    /// AC: the injectable wall clock drives peer-envelope TTL at the daemon verify
    /// seam. With MockClock pinned at wall=1000ms, an envelope with not_after=
    /// 60000 is accepted; advancing the clock to 120000 makes a fresh envelope
    /// (same not_after) expire — proving the daemon reads the injected clock, not
    /// the real wall time (which is ~1.7e12 ms and would reject both). The 60s
    /// margin dwarfs any test runtime.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mock_clock_drives_peer_ttl_at_verify_seam() {
        use crate::domain::clock::MockClock;
        let clock = Arc::new(MockClock::at_wall_ms(1_000));
        let dyn_clock: Arc<dyn Clock> = clock.clone();

        let tmp = tempfile::tempdir().unwrap();
        let (_server, socket, shutdown, handle) =
            spawn_proof_server(tmp.path(), "ttl.sock", Some(dyn_clock)).await;
        let signer = test_signer(27);

        let mut stream = UnixStream::connect(&socket).await.unwrap();
        let _ = attach_with_proof(&mut stream, false, ConnectionTier::Peer, &signer).await;

        // not_after=60000 is in the future relative to wall≈1000 → accepted.
        let fresh = peer_envelope(&signer, 1, 60_000);
        write_frame(&mut stream, &ClientFrame::PeerEnvelope(fresh))
            .await
            .unwrap();
        match read_control_frame(&mut stream).await {
            Some(DaemonFrame::PeerAccepted { .. }) => {}
            other => panic!("non-expired envelope must be accepted, got {other:?}"),
        }

        // Advance the injected clock past the TTL.
        clock.set_wall_anchor_ms(120_000);

        // A new envelope (seq=2, same not_after=60000) is now expired.
        let expired = peer_envelope(&signer, 2, 60_000);
        write_frame(&mut stream, &ClientFrame::PeerEnvelope(expired))
            .await
            .unwrap();
        loop {
            match read_frame::<_, DaemonFrame>(&mut stream).await.unwrap() {
                Some(DaemonFrame::Event(_)) => continue,
                Some(DaemonFrame::Error(ProtocolError::PeerVerification(_))) => break,
                other => {
                    panic!("expired envelope must be rejected after clock advance, got {other:?}")
                }
            }
        }
        shutdown.cancel();
        handle.abort();
    }

    /// AC: a Peer-tier connection (always read-only) cannot send unsigned
    /// mutation frames — a UserMessage is refused with ReadOnly. Peer accepts
    /// ONLY PeerEnvelope.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn peer_legacy_mutation_frame_stays_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let (_server, socket, shutdown, handle) =
            spawn_proof_server(tmp.path(), "mut.sock", None).await;

        let mut stream = UnixStream::connect(&socket).await.unwrap();
        let signer = test_signer(28);
        let _ = attach_with_proof(&mut stream, false, ConnectionTier::Peer, &signer).await;

        write_frame(
            &mut stream,
            &ClientFrame::UserMessage {
                text: "peer cannot mutate".into(),
                images: vec![],
            },
        )
        .await
        .unwrap();
        match read_frame::<_, DaemonFrame>(&mut stream).await.unwrap() {
            Some(DaemonFrame::Error(ProtocolError::ReadOnly)) => {}
            other => panic!("peer UserMessage must be refused ReadOnly, got {other:?}"),
        }
        shutdown.cancel();
        handle.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inbound_terminal_deregistration_frees_root_capacity() {
        let tmp = tempfile::tempdir().unwrap();
        let (server, _conversation, _journal, shutdown, handle) = spawn_inbound_forwarder(
            tmp.path(),
            vec![
                StreamChunk::Text {
                    content: "answer".to_owned(),
                    parent_tool_use_id: None,
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                },
            ],
        )
        .await;
        let peer_id = test_signer(80).identity().peer_id.clone();

        for _ in 0..11 {
            let mut task = inbound_task(peer_id.clone(), "repeat");
            task.response_policy.mode = crate::domain::models::ResponseMode::NotifyAndDraft;
            let node_id = task.node_id.clone();
            let mut status = server
                .start(task, CancellationToken::new())
                .await
                .expect("a terminal inbound task frees root capacity for the next task");
            // Draft mode parks for the operator; resolution is the terminal
            // transition that frees capacity.
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while *status.borrow_and_update() != NodeState::Waiting {
                    status.changed().await.expect("draft node parks");
                }
            })
            .await
            .expect("completed draft parks for operator resolution");
            server
                .resolve_peer_draft(
                    &node_id,
                    crate::adapters::daemon::response_modes::DraftResolution::Approve,
                )
                .await
                .expect("operator resolution settles the draft");
            let terminal = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                wait_for_terminal(&mut status),
            )
            .await
            .expect("resolved draft should reach a terminal state");
            assert_eq!(terminal, NodeState::Completed);
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                wait_for_deregistration(server.as_ref(), &node_id),
            )
            .await
            .expect("terminal node must be deregistered before the next task");
        }

        shutdown.cancel();
        handle.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn queued_inbound_cancellation_does_not_wait_for_turn_mutex() {
        let tmp = tempfile::tempdir().unwrap();
        let (core, _storage) = mock_core(tmp.path(), vec![]);
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "queued-cancel".to_owned(),
            ..Default::default()
        }));
        let (domain_tx, _domain_rx) = mpsc::unbounded_channel();
        let server = AttachServer::new(core, conversation, domain_tx);
        let blocked_turn = server.turn_serial.lock().await;
        let cancel = CancellationToken::new();
        let mut task = inbound_task(test_signer(81).identity().peer_id.clone(), "queued");
        task.response_policy.mode = crate::domain::models::ResponseMode::NotifyAndDraft;
        let node_id = task.node_id.clone();
        let mut status = server.start(task, cancel.clone()).await.unwrap();

        cancel.cancel();
        let terminal = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            wait_for_terminal(&mut status),
        )
        .await
        .expect("cancellation must win while the turn mutex is still blocked");
        assert_eq!(terminal, NodeState::Cancelled);
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            wait_for_deregistration(server.as_ref(), &node_id),
        )
        .await
        .expect("queued cancellation must deregister after publishing Cancelled");
        drop(blocked_turn);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_first_approval_gate_calls_install_exactly_one_receiver() {
        use crate::domain::models::ApprovalOutcome;
        use crate::domain::models::tool_call::ApprovalSource;

        let tmp = tempfile::tempdir().unwrap();
        let (core, _storage) = mock_core(tmp.path(), vec![]);
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "approval-gate".to_owned(),
            ..Default::default()
        }));
        let (domain_tx, _domain_rx) = mpsc::unbounded_channel();
        let server = AttachServer::new(core, conversation, domain_tx);
        let approval = ApprovalRuntime::new(64, Arc::new(NoOpApprovalPersistence));
        let (writer_tx, mut writer_rx) = mpsc::channel(4);
        server.registry.lock().await.conns.push(Conn {
            id: 1,
            tx: writer_tx,
            terminal_tx: watch::channel(None).0,
            mode: AttachMode::ReadWrite,
            tier: ConnectionTier::TrustedLocal,
            approval_ready: true,
        });

        const CALLERS: usize = 8;
        let barrier = Arc::new(tokio::sync::Barrier::new(CALLERS + 1));
        let mut callers = Vec::with_capacity(CALLERS);
        for _ in 0..CALLERS {
            let server = server.clone();
            let approval = approval.clone();
            let barrier = barrier.clone();
            callers.push(tokio::spawn(async move {
                barrier.wait().await;
                server.ensure_approval_gate(approval).await;
            }));
        }
        barrier.wait().await;
        for caller in callers {
            caller.await.unwrap();
        }
        assert_eq!(server.approval_gate.tasks.lock().await.len(), 1);

        let (request_id, decision) = approval
            .request(
                ApprovalSource::ForegroundTurn {
                    conversation_id: "approval-gate".to_owned(),
                },
                "gate-test".to_owned(),
                serde_json::json!({}),
                ToolRisk::Elevated,
                None,
                None,
            )
            .await;
        let request_id = request_id.expect("elevated request needs a gate decision");
        let received = tokio::time::timeout(std::time::Duration::from_secs(1), writer_rx.recv())
            .await
            .expect("the single gate forwards the request")
            .expect("writer stays connected");
        match received {
            DaemonFrame::ApprovalRequest {
                request_id: received_id,
                ..
            } => assert_eq!(received_id, request_id),
            other => panic!("expected approval request, got {other:?}"),
        }
        assert!(matches!(
            writer_rx.recv().await,
            Some(DaemonFrame::ApprovalInputChunk { sequence: 0, .. })
        ));
        assert!(matches!(
            writer_rx.recv().await,
            Some(DaemonFrame::ApprovalInputComplete { chunks: 1, .. })
        ));
        assert!(
            writer_rx.try_recv().is_err(),
            "only the winning gate sends a transfer"
        );
        approval
            .resolve(
                &request_id,
                ApprovalOutcome::Once,
                crate::domain::models::PermissionResolution::Operator,
            )
            .await;
        assert!(matches!(
            decision.await.unwrap().outcome,
            ApprovalOutcome::Once
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn aborted_inbound_turn_cannot_prefix_next_committed_answer() {
        let tmp = tempfile::tempdir().unwrap();
        let provider = Arc::new(AbortThenCompleteProvider {
            calls: AtomicUsize::new(0),
            partial_streamed: Arc::new(Notify::new()),
        });
        let partial_streamed = provider.partial_streamed.notified();
        tokio::pin!(partial_streamed);
        partial_streamed.as_mut().enable();

        let (core, _storage) = mock_core_with_provider(tmp.path(), provider.clone());
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "abort-reset".to_owned(),
            ..Default::default()
        }));
        let (bus, domain_rx) = EventBus::new(64);
        let listener = UnixListener::bind(tmp.path().join("abort-reset.sock")).unwrap();
        let journal = Arc::new(RecordingJournal::default());
        let server = journaled_server(core, conversation.clone(), bus.domain_tx.clone(), journal);
        let shutdown = CancellationToken::new();
        let server_for_run = server.clone();
        let shutdown_for_run = shutdown.clone();
        let handle = tokio::spawn(async move {
            server_for_run
                .run(listener, domain_rx, None, None, shutdown_for_run)
                .await;
        });

        let peer_id = test_signer(82).identity().peer_id.clone();
        let cancel = CancellationToken::new();
        let mut first_task = inbound_task(peer_id.clone(), "first");
        first_task.response_policy.mode = crate::domain::models::ResponseMode::NotifyAndDraft;
        let first_node_id = first_task.node_id.clone();
        let mut first_status = server.start(first_task, cancel.clone()).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), partial_streamed)
            .await
            .expect("the first turn must stream partial text before cancellation");

        cancel.cancel();
        assert_eq!(
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                wait_for_terminal(&mut first_status),
            )
            .await
            .expect("aborted inbound turn must become terminal"),
            NodeState::Cancelled
        );
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            wait_for_deregistration(server.as_ref(), &first_node_id),
        )
        .await
        .expect("aborted node must deregister after publishing Cancelled");

        let mut second_task = inbound_task(peer_id, "second");
        second_task.response_policy.mode = crate::domain::models::ResponseMode::NotifyAndDraft;
        let second_node_id = second_task.node_id.clone();
        let mut second_status = server
            .start(second_task, CancellationToken::new())
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while *second_status.borrow_and_update() != NodeState::Waiting {
                second_status.changed().await.expect("second draft parks");
            }
        })
        .await
        .expect("second inbound turn must park as a resolved-pending draft");
        server
            .resolve_peer_draft(
                &second_node_id,
                crate::adapters::daemon::response_modes::DraftResolution::Approve,
            )
            .await
            .expect("operator approves the second draft");
        assert_eq!(
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                wait_for_terminal(&mut second_status),
            )
            .await
            .expect("resolved second draft must complete"),
            NodeState::Completed
        );
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            wait_for_deregistration(server.as_ref(), &second_node_id),
        )
        .await
        .expect("second terminal node must deregister");

        let conversation = conversation.lock().await;
        let assistant_messages: Vec<_> = conversation
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::Assistant)
            .map(|message| message.content.as_str())
            .collect();
        assert_eq!(assistant_messages, vec!["fresh output"]);

        shutdown.cancel();
        handle.abort();
    }

    #[tokio::test]
    async fn transparency_room_event_fans_out_to_attached_client_queue() {
        let tmp = tempfile::tempdir().unwrap();
        let (core, _storage) = mock_core(tmp.path(), vec![]);
        let conversation = Arc::new(Mutex::new(Conversation::default()));
        let (domain_tx, _domain_rx) = mpsc::unbounded_channel();
        let server = AttachServer::new(core, conversation, domain_tx);
        let (tx, mut rx) = mpsc::channel(1);
        server.registry.lock().await.conns.push(Conn {
            id: 1,
            tx,
            terminal_tx: watch::channel(None).0,
            mode: AttachMode::ReadWrite,
            tier: ConnectionTier::TrustedLocal,
            approval_ready: true,
        });

        let mut assistant_buf = String::new();
        server
            .handle_bus_event(
                &AppEvent::DomainEvent(crate::domain::events::DomainEventPayload::Room(
                    crate::domain::models::RoomEvent::RemoteEnvelopeRejected {
                        peer: crate::domain::models::PeerId::from_public_key(&[9; 32])
                            .expect("valid test peer"),
                        reason: crate::domain::models::RejectReason::Policy {
                            detail: "policy rejection".to_owned(),
                        },
                        task: Some("remote-task-9".to_owned()),
                        direction: crate::domain::models::Direction::Inbound,
                    },
                )),
                &mut assistant_buf,
            )
            .await;

        let frame = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .expect("domain event reaches attached client queue")
            .expect("attached client queue remains open");
        match frame {
            DaemonFrame::Event(RawEvent {
                kind:
                    RawEventKind::DomainEvent(crate::domain::events::DomainEventPayload::Room(
                        crate::domain::models::RoomEvent::RemoteEnvelopeRejected {
                            task: Some(task),
                            ..
                        },
                    )),
                ..
            }) => assert_eq!(task, "remote-task-9"),
            other => panic!("expected forwarded transparency room event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancelled_terminal_discards_partial_forwarder_text_before_next_turn() {
        let tmp = tempfile::tempdir().unwrap();
        let (core, _storage) = mock_core(tmp.path(), vec![]);
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "forwarder-reset".to_owned(),
            ..Default::default()
        }));
        let (domain_tx, _domain_rx) = mpsc::unbounded_channel();
        let server = AttachServer::new(core, conversation.clone(), domain_tx);
        let mut assistant_buf = String::new();

        server
            .handle_bus_event(
                &AppEvent::ProviderChunk {
                    conversation_id: "forwarder-reset".to_owned(),
                    chunk: StreamChunk::Text {
                        content: "partial cancelled output".to_owned(),
                        parent_tool_use_id: None,
                    },
                },
                &mut assistant_buf,
            )
            .await;
        server
            .handle_bus_event(
                &AppEvent::ProviderChunk {
                    conversation_id: "forwarder-reset".to_owned(),
                    chunk: StreamChunk::TurnComplete {
                        stop_reason: StopReason::Cancelled,
                    },
                },
                &mut assistant_buf,
            )
            .await;
        assert!(assistant_buf.is_empty());

        server
            .handle_bus_event(
                &AppEvent::ProviderChunk {
                    conversation_id: "forwarder-reset".to_owned(),
                    chunk: StreamChunk::Text {
                        content: "fresh output".to_owned(),
                        parent_tool_use_id: None,
                    },
                },
                &mut assistant_buf,
            )
            .await;
        server
            .handle_bus_event(
                &AppEvent::ProviderChunk {
                    conversation_id: "forwarder-reset".to_owned(),
                    chunk: StreamChunk::TurnComplete {
                        stop_reason: StopReason::EndTurn,
                    },
                },
                &mut assistant_buf,
            )
            .await;

        let conversation = conversation.lock().await;
        let assistant_messages: Vec<_> = conversation
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::Assistant)
            .map(|message| message.content.as_str())
            .collect();
        assert_eq!(assistant_messages, vec!["fresh output"]);
    }

    #[tokio::test]
    async fn taking_inbound_result_removes_it_from_daemon_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let (core, _storage) = mock_core(tmp.path(), vec![]);
        let conversation = Arc::new(Mutex::new(Conversation::default()));
        let (domain_tx, _domain_rx) = mpsc::unbounded_channel();
        let server = AttachServer::new(core, conversation, domain_tx);
        let node_id = AgentId::new();
        server.inbound_results.lock().await.insert(
            node_id.clone(),
            crate::domain::ports::StagedPeerResult {
                text: "one-time answer".to_owned(),
                authorship: crate::domain::models::Authorship::AgentComposed,
                send_authorization: crate::domain::models::SendAuthorization::Automatic,
            },
        );

        assert_eq!(
            server
                .take_result(&node_id)
                .await
                .map(|result| result.text)
                .as_deref(),
            Some("one-time answer")
        );
        assert_eq!(server.take_result(&node_id).await, None);
    }

    #[tokio::test]
    async fn disclosure_fragments_exclude_short_empty_and_duplicate_prompt_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let long_line = "This host-sensitive system prompt line exceeds thirty-two characters.";
        let prompt = format!("\n  \nshort\n  {long_line}  \n{long_line}\n\t\n");
        let core = mock_core_with_persona(tmp.path(), &prompt);
        let conversation = Arc::new(Mutex::new(Conversation::default()));
        let (domain_tx, _domain_rx) = mpsc::unbounded_channel();
        let server = AttachServer::new(core, conversation, domain_tx);

        assert_eq!(
            server.disclosure_forbidden_fragments().await,
            vec![long_line.to_owned()]
        );

        let empty_core = mock_core_with_persona(tmp.path(), "\n short \n");
        let (empty_domain_tx, _empty_domain_rx) = mpsc::unbounded_channel();
        let empty_server = AttachServer::new(
            empty_core,
            Arc::new(Mutex::new(Conversation::default())),
            empty_domain_tx,
        );
        assert!(
            empty_server
                .disclosure_forbidden_fragments()
                .await
                .is_empty()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn story19_19_review_clear_cannot_be_resurrected_by_running_turn() {
        struct HeldCompletion(Arc<Notify>);
        #[async_trait::async_trait]
        impl StreamingProvider for HeldCompletion {
            async fn stream_completion(
                &self,
                _messages: Vec<Message>,
                _options: CompletionOptions,
            ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
                use futures::StreamExt;
                let release = self.0.clone();
                Ok(futures::stream::iter([StreamChunk::Text {
                    content: "pre-clear-answer".to_owned(),
                    parent_tool_use_id: None,
                }])
                .chain(futures::stream::once(async move {
                    release.notified().await;
                    StreamChunk::TurnComplete {
                        stop_reason: StopReason::EndTurn,
                    }
                }))
                .boxed())
            }
            async fn abort(&self) -> Result<(), ProviderError> {
                Ok(())
            }
            fn provider_id(&self) -> String {
                "held-completion".to_owned()
            }
            fn list_models(&self) -> Vec<ModelDescriptor> {
                vec![]
            }
            async fn health_check(&self) -> Result<(), ProviderError> {
                Ok(())
            }
            async fn connectivity_probe(
                &self,
            ) -> Result<crate::domain::ports::ProbeOutcome, ProviderError> {
                Ok(crate::domain::ports::ProbeOutcome {
                    latency: std::time::Duration::ZERO,
                })
            }
        }
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let workspace = tempfile::tempdir().unwrap();
            let release = Arc::new(Notify::new());
            let (core, storage) = mock_core_with_provider(
                workspace.path(),
                Arc::new(HeldCompletion(release.clone())),
            );
            let conversation = Arc::new(Mutex::new(Conversation {
                id: "review-clear".to_owned(),
                ..Default::default()
            }));
            let (bus, domain_rx) = EventBus::new(64);
            let socket = workspace.path().join("review-clear.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let server = AttachServer::new(core, conversation.clone(), bus.domain_tx);
            let shutdown = CancellationToken::new();
            let service = {
                let server = server.clone();
                let shutdown = shutdown.clone();
                tokio::spawn(async move {
                    server.run(listener, domain_rx, None, None, shutdown).await;
                })
            };
            let mut client = UnixStream::connect(&socket).await.unwrap();
            assert!(matches!(
                attach_with_proof(
                    &mut client,
                    false,
                    ConnectionTier::TrustedLocal,
                    &test_signer(93)
                )
                .await,
                Some(DaemonFrame::AttachAck {
                    granted_mode: AttachMode::ReadWrite,
                    ..
                })
            ));
            write_frame(
                &mut client,
                &ClientFrame::UserMessage {
                    text: "hold this ordinary turn".to_owned(),
                    images: vec![],
                },
            )
            .await
            .unwrap();
            loop {
                if matches!(
                    read_frame::<_, DaemonFrame>(&mut client).await.unwrap(),
                    Some(DaemonFrame::Event(RawEvent {
                        kind: RawEventKind::Provider(StreamChunk::Text { .. }),
                        ..
                    }))
                ) {
                    break;
                }
            }
            write_frame(
                &mut client,
                &ClientFrame::UserMessage {
                    text: "/clear".to_owned(),
                    images: vec![],
                },
            )
            .await
            .unwrap();
            write_frame(
                &mut client,
                &ClientFrame::DeferRecipientItem {
                    item_id: "ri_clear_reader_probe".to_owned(),
                },
            )
            .await
            .unwrap();
            let mut reset_observed = false;
            loop {
                match read_frame::<_, DaemonFrame>(&mut client).await.unwrap() {
                    Some(DaemonFrame::RecipientActionResult {
                        action: super::super::protocol::RecipientAction::Defer,
                        target: super::super::protocol::RecipientActionTarget::Item { item_id },
                        ..
                    }) if item_id == "ri_clear_reader_probe" => break,
                    Some(DaemonFrame::RecipientContextState {
                        show_inventory: false,
                        ..
                    }) => {
                        reset_observed = true;
                    }
                    _ => {}
                }
            }
            release.notify_one();
            if !reset_observed {
                loop {
                    if matches!(
                        read_frame::<_, DaemonFrame>(&mut client).await.unwrap(),
                        Some(DaemonFrame::RecipientContextState {
                            show_inventory: false,
                            ..
                        })
                    ) {
                        break;
                    }
                }
            }
            let turn_guard = server.turn_serial.lock().await;
            let remaining = conversation.lock().await.messages.clone();
            let persisted = storage
                .load_conversation("review-clear")
                .await
                .unwrap()
                .unwrap();
            drop(turn_guard);
            shutdown.cancel();
            drop(client);
            service.await.unwrap();
            assert!(
                remaining.is_empty() && persisted.messages.is_empty(),
                "a completed pre-clear turn resurrected the reset conversation: {remaining:?}"
            );
        })
        .await
        .expect("real reset/turn socket scenario completed");
    }
    async fn review_managed_clear_waits_for_tool_display() {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let workspace = tempfile::tempdir().unwrap();
            let provider = Arc::new(ToolThenTextProvider { calls: AtomicUsize::new(0) });
            let (core, storage) = mock_core_with_provider(workspace.path(), provider);
            let conversation = Arc::new(Mutex::new(Conversation {
                id: "managed-clear".to_owned(), ..Default::default()
            }));
            let (bus, mut domain_rx) = EventBus::new(64);
            let (forward_tx, forward_rx) = mpsc::unbounded_channel();
            let closed = Arc::new(Notify::new());
            let observed_close = closed.notified();
            tokio::pin!(observed_close);
            observed_close.as_mut().enable();
            let relay = {
                let closed = closed.clone();
                tokio::spawn(async move {
                    while let Some(event) = domain_rx.recv().await {
                        let last = matches!(event, AppEvent::PeerTurnClosed { .. });
                        if forward_tx.send(event).is_err() { break; }
                        if last { closed.notify_waiters(); }
                    }
                })
            };
            let socket = workspace.path().join("managed-clear.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let server = AttachServer::new(core, conversation.clone(), bus.domain_tx);
            let shutdown = CancellationToken::new();
            let service = {
                let server = server.clone();
                let shutdown = shutdown.clone();
                tokio::spawn(async move { server.run(listener, forward_rx, None, None, shutdown).await; })
            };
            let mut client = UnixStream::connect(&socket).await.unwrap();
            assert!(matches!(attach_with_proof(&mut client, false, ConnectionTier::TrustedLocal, &test_signer(94)).await,
                Some(DaemonFrame::AttachAck { granted_mode: AttachMode::ReadWrite, .. })));
            write_frame(&mut client, &ClientFrame::DeferRecipientItem { item_id: "ri_initial_probe".to_owned() }).await.unwrap();
            loop {
                if matches!(read_frame::<_, DaemonFrame>(&mut client).await.unwrap(),
                    Some(DaemonFrame::RecipientActionResult { target: super::super::protocol::RecipientActionTarget::Item { item_id }, .. }) if item_id == "ri_initial_probe") { break; }
            }
            // Gate the actual forwarder before any tool evidence can be folded.
            let display_gate = server.peer_assistant_buffers.lock().await;
            let peer = test_signer(95).identity().peer_id.clone();
            let mut task = inbound_task(peer, "request a tool");
            task.response_policy.mode = crate::domain::models::ResponseMode::NotifyAndDraft;
            server.start(task, CancellationToken::new()).await.unwrap();
            loop {
                if let Some(DaemonFrame::ApprovalRequest { request_id, .. }) =
                    read_frame::<_, DaemonFrame>(&mut client).await.unwrap() {
                    write_frame(&mut client, &ClientFrame::ApprovalResponse {
                        request_id, outcome: crate::domain::models::ApprovalOutcome::Once
                    }).await.unwrap();
                    break;
                }
            }
            observed_close.await;
            write_frame(&mut client, &ClientFrame::UserMessage { text: "/clear".to_owned(), images: vec![] }).await.unwrap();
            let early_reset = tokio::time::timeout(std::time::Duration::from_millis(200), async {
                loop {
                    let frame = read_frame::<_, DaemonFrame>(&mut client).await.unwrap();
                    if matches!(frame, Some(DaemonFrame::RecipientContextState { show_inventory: false, .. }))
                        && conversation.lock().await.messages.is_empty() { break; }
                }
            }).await.is_ok();
            drop(display_gate);
            if !early_reset {
                loop {
                    let frame = read_frame::<_, DaemonFrame>(&mut client).await.unwrap();
                    if matches!(frame, Some(DaemonFrame::RecipientContextState { show_inventory: false, .. }))
                        && conversation.lock().await.messages.is_empty() { break; }
                }
            }
            let turn_guard = server.turn_serial.lock().await;
            let remaining = conversation.lock().await.messages.clone();
            let persisted = storage.load_conversation("managed-clear").await.unwrap().unwrap();
            drop(turn_guard);
            shutdown.cancel();
            drop(client);
            service.await.unwrap();
            relay.abort();
            let _ = relay.await;
            assert!(!early_reset, "clear acknowledged before the queued managed tool display was drained");
            assert!(remaining.is_empty() && persisted.messages.is_empty(),
                "managed tool evidence resurrected the cleared conversation: {remaining:?}");
        }).await.expect("managed tool/reset socket scenario completed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn story19_19_review_a2a_clear_waits_for_managed_tool_display() {
        review_managed_clear_waits_for_tool_display().await;
    }

    #[tokio::test]
    async fn clearing_conversation_removes_uncontrolled_peer_display_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let (_core, storage) = mock_core(tmp.path(), vec![]);
        let peer_row = ChatMessage {
            id: "peer-transcript".to_owned(),
            content: "remote transcript".to_owned(),
            history_source: crate::domain::models::HistorySource::PeerDisplay,
            ..Default::default()
        };
        let local_row = ChatMessage {
            id: "local".to_owned(),
            content: "local text".to_owned(),
            ..Default::default()
        };
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "clear-peer-rows".to_owned(),
            messages: vec![local_row, peer_row],
            ..Default::default()
        }));
        let (writer_tx, mut writer_rx) = mpsc::channel(4);
        let registry = Mutex::new(ConnRegistry {
            conns: vec![Conn {
                id: 1,
                tx: writer_tx,
                terminal_tx: watch::channel(None).0,
                mode: AttachMode::ReadWrite,
                tier: ConnectionTier::TrustedLocal,
                approval_ready: true,
            }],
        });

        let pending_drafts =
            crate::adapters::daemon::response_modes::PendingDraftController::default();
        let row_serial = Mutex::new(());
        clear_conversation_rows(
            &pending_drafts,
            &row_serial,
            &conversation,
            &registry,
            storage.as_ref(),
        )
        .await
        .unwrap();

        let Some(DaemonFrame::PeerRowOperation(operation)) = writer_rx.recv().await else {
            panic!("peer display removal must be published");
        };
        assert!(matches!(
            operation.as_ref(),
            PeerRowOperation::Remove { row_id, revision: 1 } if row_id == "peer-transcript"
        ));
        assert!(conversation.lock().await.messages.is_empty());
    }

    #[tokio::test]
    async fn peer_stream_error_is_persisted_as_a_separate_display_notice() {
        let tmp = tempfile::tempdir().unwrap();
        let (core, storage) = mock_core(tmp.path(), vec![]);
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "peer-stream-error".to_owned(),
            ..Default::default()
        }));
        let (domain_tx, _domain_rx) = mpsc::unbounded_channel();
        let server = AttachServer::new(core, conversation.clone(), domain_tx);
        let node = Arc::new(AgentId::new());

        server
            .collect_peer_chunk(
                &node,
                &StreamChunk::Error {
                    content: "provider disconnected".to_owned(),
                },
            )
            .await;

        let rows = conversation.lock().await.messages.clone();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].role, MessageRole::System);
        assert!(rows[0].is_peer_display());
        assert_eq!(rows[0].content, "Peer turn failed: provider disconnected");
        let persisted = storage
            .load_conversation("peer-stream-error")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(persisted.messages.len(), rows.len());
        assert_eq!(persisted.messages[0].id, rows[0].id);
        assert_eq!(persisted.messages[0].content, rows[0].content);
    }

    #[tokio::test]
    async fn peer_turn_completion_does_not_wake_operator_completion_waiters() {
        let tmp = tempfile::tempdir().unwrap();
        let (core, _storage) = mock_core(tmp.path(), vec![]);
        let conversation = Arc::new(Mutex::new(Conversation::default()));
        let (domain_tx, _domain_rx) = mpsc::unbounded_channel();
        let server = AttachServer::new(core, conversation, domain_tx);
        let node = Arc::new(AgentId::new());
        let operator_complete = server.turn_complete.notified();
        tokio::pin!(operator_complete);
        operator_complete.as_mut().enable();

        server
            .collect_peer_chunk(
                &node,
                &StreamChunk::TurnComplete {
                    stop_reason: StopReason::Cancelled,
                },
            )
            .await;

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), operator_complete)
                .await
                .is_err()
        );
    }
    /// K02b is intentionally isolated: no Unix/TCP socket or paused live I/O.
    /// The existing coordinator gate is a counting front-door double; the
    /// explicit decision still enters the actual daemon frame dispatcher.
    #[cfg(feature = "a2a")]
    #[tokio::test(start_paused = true)]
    async fn proposal_24h_never_enters_send_frontdoor_then_human_send_once() {
        use crate::domain::ports::TeamProposalSink;
        let workspace = tempfile::tempdir().unwrap();
        let (core, _) = mock_core(workspace.path(), vec![]);
        let (domain_tx, _domain_rx) = mpsc::unbounded_channel();
        let tree = crate::infrastructure::subagent::NodeTree::new();
        let journal = Arc::new(RecordingJournal::default());
        let peer = crate::domain::models::A2aPeerSpec::new(
            "lena-po",
            crate::domain::models::RedactedUrl::from("http://127.0.0.1:9".to_owned()),
            crate::domain::models::A2aPeerSource::Workspace,
        );
        // Construct bindings without A2aEgress's boot card-fetch tasks: this
        // lifecycle test must never start a socket while time is paused.
        let client =
            Arc::new(crate::adapters::a2a::client::A2aClientAdapter::new(&peer, None).unwrap());
        let provider =
            crate::adapters::a2a::provider::A2aProvider::new(vec![(peer.clone(), client)]);
        let runtime = Arc::new(
            A2aDelegationRuntime::new(tree.clone(), journal.clone(), domain_tx.clone())
                .with_peer_bindings(provider.peer_bindings()),
        );
        let policy: Arc<dyn crate::domain::ports::DeliveryPolicy> =
            Arc::new(crate::domain::ports::RelationshipDeliveryPolicy);
        let bus = peer_bus_slot_with_policy(&tree, policy.clone());
        let proposals = Arc::new(super::super::team_proposal::TeamProposalRegistry::new());
        let hooks = TeamSendTestHooks::new(None, None, false).with_send_action_gate();
        let server = AttachServer::new_with_node_tree_bus_policy_journal_and_team(
            core,
            Arc::new(Mutex::new(Conversation {
                id: "isolated-timer".into(),
                ..Default::default()
            })),
            domain_tx,
            tree,
            bus,
            policy,
            journal.clone(),
            journal.clone(),
            None,
            Some(runtime),
            vec![peer],
            proposals.clone(),
            Some(hooks.clone()),
        );
        let (tx, mut rx) = mpsc::channel(16);
        let (terminal_tx, _terminal_rx) = watch::channel(None);
        server.registry.lock().await.conns.push(Conn {
            id: 1,
            tx,
            terminal_tx,
            mode: AttachMode::ReadWrite,
            tier: ConnectionTier::TrustedLocal,
            approval_ready: true,
        });
        server.last_writer_conn_id.store(1, Ordering::SeqCst);
        server.grant_team_proposal_writer(1).await;
        let proposal = Arc::new(crate::domain::models::TeamProposal {
            id: "tp_isolated_timer".into(),
            conversation_id: "isolated-timer".into(),
            recipients: vec!["lena-po".into()],
            message_type: crate::domain::models::SemanticMessageType::ArchitectureUpdate,
            body: "operator-reviewed exact body".into(),
        });
        assert_eq!(
            proposals.raise(proposal.clone()).await,
            crate::domain::models::ProposalRaiseOutcome::Shown
        );
        assert!(matches!(
            rx.recv().await,
            Some(DaemonFrame::TeamProposal { .. })
        ));
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(24 * 60 * 60)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            hooks.send_action_count(),
            0,
            "K02b elapsed 24h must call the send frontdoor zero times"
        );
        assert_eq!(
            proposals.pending().await,
            vec![proposal.clone()],
            "K02b elapsed 24h must call the send frontdoor zero times and retain the exact pending proposal"
        );
        server
            .handle_client_frame_tiered(
                ClientFrame::TeamProposalDecision {
                    proposal: proposal.id.clone(),
                    decision: crate::domain::models::TeamProposalDecision::Send,
                },
                AttachMode::ReadWrite,
                ConnectionTier::TrustedLocal,
                1,
            )
            .await;
        hooks.wait_for_send_action_count(1).await;
        assert_eq!(
            hooks.send_action_count(),
            1,
            "K02b explicit human Send must call the send frontdoor exactly once"
        );
        assert!(proposals.pending().await.is_empty());
        // The counting double never releases its coordinator gate. Cancel and
        // reap the owned task rather than opening a real network connection.
        server.team_send_shutdown.cancel();
        for task in server.team_send_tasks.lock().await.drain(..) {
            task.abort();
            let _ = task.await;
        }
        assert!(journal.events.lock().await.is_empty());
    }
    /// Story 19.21c: runtime absence refuses every new request before slot access,
    /// in both feature-independent and a2a-enabled clients.
    #[tokio::test]
    async fn attached_parity_no_runtime_is_request_keyed_after_authority_gates() {
        let tmp = tempfile::tempdir().unwrap();
        let (core, _storage) = mock_core(tmp.path(), vec![]);
        let (bus, domain_rx) = EventBus::new(32);
        let server = AttachServer::new(
            core,
            Arc::new(Mutex::new(Conversation::default())),
            bus.domain_tx.clone(),
        );
        let socket = tmp.path().join("parity.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let shutdown = CancellationToken::new();
        let running = tokio::spawn(server.clone().run(
            listener,
            domain_rx,
            None,
            None,
            shutdown.child_token(),
        ));
        let mut client = UnixStream::connect(&socket).await.unwrap();
        assert!(matches!(
            attach_with_proof(
                &mut client,
                false,
                ConnectionTier::TrustedLocal,
                &test_signer(73)
            )
            .await,
            Some(DaemonFrame::AttachAck {
                granted_mode: AttachMode::ReadWrite,
                ..
            })
        ));
        #[cfg(feature = "a2a")]
        {
            let id = server.registry.lock().await.conns[0].id;
            server.team_retracts.lock().await.insert(
                id,
                team::RetractSlot::Reading {
                    request: "decision".into(),
                    admission: 0,
                },
            );
        }
        for (request, action, frame) in [
            (
                "thread",
                RecipientAction::TeamThread,
                ClientFrame::TeamThread {
                    request: "thread".into(),
                    peer: "B".into(),
                    item_id: "ri_a".into(),
                },
            ),
            (
                "followup",
                RecipientAction::TeamSend,
                ClientFrame::TeamFollowup {
                    request: "followup".into(),
                    peer: "B".into(),
                    reply_to: "ri_a".into(),
                    text: "more".into(),
                },
            ),
            (
                "preview",
                RecipientAction::TeamRetract,
                ClientFrame::TeamRetractPreview {
                    request: "preview".into(),
                    peer: "B".into(),
                    item_id: "ri_a".into(),
                },
            ),
            (
                "decision",
                RecipientAction::TeamRetract,
                ClientFrame::TeamRetractDecision {
                    request: "decision".into(),
                    confirm: true,
                },
            ),
        ] {
            write_frame(&mut client, &frame).await.unwrap();
            let answer = tokio::time::timeout(
                std::time::Duration::from_secs(20),
                read_frame::<_, DaemonFrame>(&mut client),
            )
            .await
            .unwrap()
            .unwrap()
            .unwrap();
            assert!(matches!(answer, DaemonFrame::RecipientActionResult {
                action: found, target: RecipientActionTarget::Request { request: key },
                outcome: RecipientActionOutcome::Refused { error: ProtocolError::Internal(message) },
            } if found == action && key == request && message == "A2A is not enabled in this build"));
        }
        #[cfg(feature = "a2a")]
        assert_eq!(
            server.team_retracts.lock().await.len(),
            1,
            "runtime check precedes compare-and-take"
        );
        drop(client);
        shutdown.cancel();
        running.await.unwrap();
    }
}
