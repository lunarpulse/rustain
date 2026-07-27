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
//! - the **headless approval gate** (AC6): attached-writer → forward; timeout →
//!   conservative deny; unattended → deny-by-default (Safe auto-proceeds);
//!   denied-while-unattended is a durable, visible, resumable transcript record +
//!   a waiting-count.
//!
//! The turn is driven on the daemon-owned bus (not any socket), so an in-flight
//! turn **completes and persists even if the client detaches** (AC4).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use rand_core::{OsRng, RngCore};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Notify, mpsc};
use tokio_util::sync::CancellationToken;

#[cfg(feature = "cron")]
use crate::adapters::scheduler::cron::CronCompletion;
#[cfg(not(feature = "cron"))]
pub struct CronCompletion;
use crate::adapters::rap::{
    ReplayWindow, VerifyError, verify_attach_proof, verify_envelope_reserved,
};
use crate::domain::clock::{Clock, SystemClock};
use crate::domain::events::AppEvent;
use crate::domain::models::{
    AgentId, AgentMessage, ChannelKind, ChannelTurnRequest, ChatMessage, Conversation, MessageRole,
    NodeState, PeerId, StopReason, StreamChunk, ToolRisk, TurnOrigin, generate_message_id,
};
use crate::domain::services::approval_runtime::{ApprovalRuntime, ApprovalRuntimeEvent};
use crate::infrastructure::runtime::event_bus::{RawEvent, RawEventKind};

use super::protocol::{
    AttachMode, AttachSnapshot, ClientFrame, ConnectionTier, DaemonFrame, PROTOCOL_VERSION,
    ProposalToken, ProtocolError, read_frame, write_frame,
};
use super::runtime::DaemonCore;

/// Per-connection bounded writer queue depth. A connection that cannot keep up is
/// dropped (its queue fills) — the turn is never blocked (AC3).
const CONN_QUEUE_DEPTH: usize = 1024;

/// How long an attached-but-unresponsive writer has to answer an approval before
/// the daemon falls back to the conservative unattended path (deny) (AC6 #2).
const APPROVAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

const CHANNEL_TURN_FAILED_REPLY: &str =
    "Sorry, processing failed before the agent produced a response. Please try again.";

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

/// One attached connection's outbound handle + grant.
struct Conn {
    id: u64,
    tx: mpsc::Sender<DaemonFrame>,
    mode: AttachMode,
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

    /// Fan a frame out to every connection; queue-full / closed connections are
    /// dropped (returned for removal) so a slow reader never stalls the turn.
    fn fanout(&self, frame: &DaemonFrame) -> Vec<u64> {
        let mut dead = Vec::new();
        for c in &self.conns {
            match c.tx.try_send(frame.clone()) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_))
                | Err(mpsc::error::TrySendError::Closed(_)) => dead.push(c.id),
            }
        }
        dead
    }

    fn remove(&mut self, id: u64) {
        self.conns.retain(|c| c.id != id);
    }
}

/// The daemon attach server. Holds the lazily-built core, the per-process
/// conversation, the connection registry, and the daemon-owned event bus.
pub struct AttachServer {
    core: Arc<DaemonCore>,
    conversation: Arc<Mutex<Conversation>>,
    registry: Arc<Mutex<ConnRegistry>>,
    domain_tx: mpsc::UnboundedSender<AppEvent>,
    node_tree: crate::infrastructure::subagent::NodeTree,
    /// Tool actions denied while unattended, awaiting the user to resume (AC6 #5).
    blocked_waiting: Arc<AtomicUsize>,
    next_conn_id: AtomicU64,
    /// Guards single-spawn of the approval gate (started on first runtime build).
    approval_gate_started: Arc<std::sync::atomic::AtomicBool>,
    turn_serial: Arc<Mutex<()>>,
    turn_complete: Arc<Notify>,
    /// Story 12.3 — current turn origin read by `commit_assistant_turn`.
    /// tokio::sync — locks=4 held.
    active_channel_origin: Arc<Mutex<ChannelKind>>,
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
    /// Story 18.1b — the assistant answer produced by each inbound A2A peer
    /// node, captured at the moment its turn finished.
    ///
    /// Keyed by node id rather than read back off the shared conversation: the
    /// daemon has ONE conversation and several origins push into it, so "the
    /// last assistant message" is not reliably *this* task's answer. Capturing
    /// at completion is; guessing later is how a peer ends up reading another
    /// channel's reply.
    inbound_results: Arc<Mutex<std::collections::HashMap<AgentId, String>>>,
}

struct DaemonPeerConsumer {
    server: std::sync::Weak<AttachServer>,
}

#[async_trait::async_trait]
impl crate::adapters::rap::VerifiedPeerConsumer for DaemonPeerConsumer {
    async fn consent(
        &self,
        _recipient: &AgentId,
        _content: &AgentMessage,
        _peer_id: &PeerId,
    ) -> Result<crate::adapters::rap::VerifiedPeerConsent, String> {
        let server = self
            .server
            .upgrade()
            .ok_or_else(|| "daemon peer consumer is shutting down".to_string())?;
        server
            .core
            .ensure_runtime()
            .await
            .map_err(|error| error.to_string())?;
        Ok(crate::adapters::rap::VerifiedPeerConsent::Accept)
    }

    async fn ingest(
        &self,
        _recipient: &AgentId,
        content: AgentMessage,
        peer_id: &PeerId,
    ) -> Result<(), String> {
        let server = self
            .server
            .upgrade()
            .ok_or_else(|| "daemon peer consumer is shutting down".to_string())?;
        server
            .enqueue_verified_peer_turn(content.content, peer_id.clone())
            .await
    }
}

/// Subscribe and publish the approval gate as one ordered operation.
///
/// A broadcast receiver only receives requests emitted after it exists. Create
/// it before publishing the once-only flag so a concurrent caller can never
/// observe "started" while no receiver has been installed yet.
fn ensure_approval_gate_once(
    approval_gate_started: &std::sync::atomic::AtomicBool,
    approval: Arc<ApprovalRuntime>,
    registry: Arc<Mutex<ConnRegistry>>,
    blocked: Arc<AtomicUsize>,
    conversation: Arc<Mutex<Conversation>>,
    storage: Arc<dyn crate::domain::ports::StoragePort>,
) {
    let events = approval.subscribe();
    if approval_gate_started.swap(true, Ordering::SeqCst) {
        return;
    }
    tokio::spawn(async move {
        run_approval_gate(events, approval, registry, blocked, conversation, storage).await;
    });
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
    let bus = Arc::new(
        crate::infrastructure::agent_message_bus::LocalMessageBus::new(
            node_tree.clone(),
            Arc::new(crate::domain::ports::RelationshipDeliveryPolicy),
        ),
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
        let peer_bus = default_peer_bus_slot(&node_tree);
        Self::with_clock_and_node_tree(core, conversation, domain_tx, clock, node_tree, peer_bus)
    }

    pub fn new_with_node_tree(
        core: Arc<DaemonCore>,
        conversation: Arc<Mutex<Conversation>>,
        domain_tx: mpsc::UnboundedSender<AppEvent>,
        node_tree: crate::infrastructure::subagent::NodeTree,
    ) -> Arc<Self> {
        let peer_bus = default_peer_bus_slot(&node_tree);
        Self::new_with_node_tree_and_bus(core, conversation, domain_tx, node_tree, peer_bus)
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
        Self::with_clock_and_node_tree(
            core,
            conversation,
            domain_tx,
            Arc::new(SystemClock::default()),
            node_tree,
            peer_bus,
        )
    }

    fn with_clock_and_node_tree(
        core: Arc<DaemonCore>,
        conversation: Arc<Mutex<Conversation>>,
        domain_tx: mpsc::UnboundedSender<AppEvent>,
        clock: Arc<dyn Clock>,
        node_tree: crate::infrastructure::subagent::NodeTree,
        peer_bus: PeerBusSlot,
    ) -> Arc<Self> {
        Arc::new_cyclic(|_weak| Self {
            core,
            conversation,
            registry: Arc::new(Mutex::new(ConnRegistry::default())),
            domain_tx,
            node_tree,
            blocked_waiting: Arc::new(AtomicUsize::new(0)),
            next_conn_id: AtomicU64::new(1),
            approval_gate_started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            turn_serial: Arc::new(Mutex::new(())),
            turn_complete: Arc::new(Notify::new()),
            retained_consolidations: Arc::new(Mutex::new(std::collections::HashMap::new())),
            active_channel_origin: Arc::new(Mutex::new(ChannelKind::Terminal)),
            pending_channel_response_tx: Arc::new(Mutex::new(None)),
            next_proposal_token: Arc::new(AtomicU64::new(1)),
            generating_consolidations: Arc::new(Mutex::new(std::collections::HashSet::new())),
            replay: Arc::new(Mutex::new(ReplayWindow::default())),
            clock,
            peer_delivery: Arc::new(tokio::sync::RwLock::new(None)),
            peer_bus,
            inbound_results: Arc::new(Mutex::new(std::collections::HashMap::new())),
        })
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
    ) {
        let handler = Arc::new(crate::adapters::rap::VerifiedPeerFrameHandler::new(
            self.node_tree.clone(),
            self.peer_bus.clone(),
            self.domain_tx.clone(),
            Arc::new(DaemonPeerConsumer {
                server: Arc::downgrade(self),
            }),
            recorder,
        ));
        *self.peer_delivery.write().await = Some(handler);
    }

    /// Current count of connected channels for honest `status` reporting (AC4).
    pub fn channel_count(&self) -> usize {
        // best-effort, non-blocking read
        self.registry.try_lock().map(|r| r.conns.len()).unwrap_or(0)
    }
    pub async fn run(
        self: Arc<Self>,
        listener: UnixListener,
        mut domain_rx: mpsc::UnboundedReceiver<AppEvent>,
        channel_rx: Option<mpsc::UnboundedReceiver<ChannelTurnRequest>>,
        cron_completion_rx: Option<mpsc::UnboundedReceiver<CronCompletion>>,
        shutdown: CancellationToken,
    ) {
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
        forwarder.abort();
    }

    /// Fold an assistant turn into the conversation (persist on completion) and
    /// fan the projected event out to all attachments.
    async fn handle_bus_event(&self, event: &AppEvent, assistant_buf: &mut String) {
        if let AppEvent::ProviderChunk { chunk, .. } = event {
            match chunk {
                StreamChunk::Text { content, .. } => assistant_buf.push_str(content),
                StreamChunk::TurnComplete { stop_reason } => {
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
                }
                StreamChunk::ToolUse { id, name, .. } => {
                    assistant_buf.push_str(&format!("\n[tool use: {name} (id: {id})]\n"));
                }
                StreamChunk::ToolResult { content, .. } => {
                    assistant_buf.push_str(&format!("[tool result: {content}]\n"));
                }
                _ => {}
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

        // Register the connection atomically with mode selection: the first
        // ReadWrite-capable client is the writer; later clients are read-only.
        // Keeping grant+registration under one lock closes the two-attach writer
        // TOCTOU.
        let conn_id = self.next_conn_id.fetch_add(1, Ordering::SeqCst);
        let (tx, mut rx) = mpsc::channel::<DaemonFrame>(CONN_QUEUE_DEPTH);
        let granted_mode = {
            let mut reg = self.registry.lock().await;
            let granted = if tier == ConnectionTier::Peer || read_only_ok || reg.has_writer() {
                AttachMode::ReadOnly
            } else {
                AttachMode::ReadWrite
            };
            reg.conns.push(Conn {
                id: conn_id,
                tx,
                mode: granted,
            });
            granted
        };

        // Snapshot for immediate render (AC2).
        let snapshot = {
            let conv = self.conversation.lock().await;
            AttachSnapshot {
                conversation_id: conv.id.clone(),
                transcript: conv.messages.clone(),
                permission_mode: self.core.security.current_mode(),
                channels: vec![ChannelKind::Terminal],
                blocked_actions_waiting: self.blocked_waiting.load(Ordering::SeqCst),
            }
        };
        if write_frame(
            &mut write_half,
            &DaemonFrame::AttachAck {
                granted_mode,
                snapshot,
            },
        )
        .await
        .is_err()
        {
            let mut reg = self.registry.lock().await;
            reg.remove(conn_id);
            return;
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
        let writer = tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                if write_frame(&mut write_half, &frame).await.is_err() {
                    break;
                }
            }
        });

        // Story 12.2c AC7 — drain the 12.1c durable boundary queues to the freshly
        // attached WRITER and emit them onto the normal event stream (no new frame).
        // The purge notice is daemon-owned state, so the DAEMON drains it (a
        // read-only client reading/clearing it would violate the ownership boundary).
        if granted_mode == AttachMode::ReadWrite {
            self.emit_session_queue_notices(conn_id).await;
        }

        // Reader loop. Peer envelopes are verified against the SERVER-WIDE
        // replay window held on `self` (shared across connections).
        loop {
            match read_frame::<_, ClientFrame>(&mut read_half).await {
                Ok(Some(frame)) => {
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
        writer.abort();
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
                if mode != AttachMode::ReadWrite {
                    self.send_to(conn_id, DaemonFrame::Error(ProtocolError::ReadOnly))
                        .await;
                    return false;
                }
                if let Ok(rt) = self.core.ensure_runtime().await {
                    rt.approval.resolve(&request_id, outcome).await;
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
                    (conv.messages[start..end].to_vec(), start > 0)
                };
                self.send_to(conn_id, DaemonFrame::History { messages, has_more })
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
        }
        false
    }

    async fn enqueue_verified_peer_turn(
        self: Arc<Self>,
        text: String,
        peer_id: PeerId,
    ) -> Result<(), String> {
        let (ingested_tx, ingested_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _turn_guard = self.turn_serial.lock().await;
            *self.active_channel_origin.lock().await = ChannelKind::Terminal;
            let rt = match self.core.ensure_runtime().await {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = ingested_tx.send(Err(error.to_string()));
                    return;
                }
            };
            self.ensure_approval_gate(rt.approval.clone());

            let turn_complete = self.turn_complete.notified();
            let handle = {
                let mut conversation = self.conversation.lock().await;
                conversation.messages.push(ChatMessage {
                    id: generate_message_id(),
                    role: MessageRole::User,
                    content: text,
                    content_blocks: vec![],
                    tool_calls: vec![],
                    created_at: crate::domain::models::session_meta::now_unix(),
                    token_count: None,
                    stop_reason: None,
                    synthetic: false,
                    images: vec![],
                    origin: ChannelKind::Terminal,
                });
                rt.drive_preloaded_turn(
                    &mut conversation,
                    &self.domain_tx,
                    TurnOrigin::RemotePeer { peer_id },
                    CancellationToken::new(),
                )
            };
            let _ = ingested_tx.send(Ok(()));

            if let Err(error) = handle.await {
                tracing::warn!(error = ?error, "daemon verified-peer turn failed");
            } else if tokio::time::timeout(std::time::Duration::from_secs(5), turn_complete)
                .await
                .is_err()
            {
                tracing::warn!(
                    "verified-peer turn completed but assistant commit was not observed"
                );
            }
            *self.active_channel_origin.lock().await = ChannelKind::Terminal;
        });
        ingested_rx
            .await
            .map_err(|_| "verified-peer ingest task closed".to_string())?
    }

    /// Build the runtime (first activity), spawn the approval gate once, drive
    /// the turn. The turn emits to the daemon bus; the forwarder fans out + folds.
    async fn drive_user_turn(&self, text: String) {
        if text.trim() == "/clear" {
            if let Some(handler) = self.peer_delivery.read().await.clone() {
                handler.clear_all_taint().await;
            }
            self.conversation.lock().await.messages.clear();
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

        let rt = match self.core.ensure_runtime().await {
            Ok(rt) => rt,
            Err(e) => {
                tracing::error!(error = %e, "daemon: building turn runtime failed");
                self.resolve_pending_channel_response(CHANNEL_TURN_FAILED_REPLY)
                    .await;
                *self.active_channel_origin.lock().await = ChannelKind::Terminal;
                return Err(e.to_string());
            }
        };
        self.ensure_approval_gate(rt.approval.clone());

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
        };

        if let Err(e) = handle.await {
            tracing::warn!(error = ?e, "daemon turn task failed");
            self.resolve_pending_channel_response(CHANNEL_TURN_FAILED_REPLY)
                .await;
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
    /// broadcast into the void and its approval would hang until its own
    /// timeout. Story 18.1b made that reachable — an inbound A2A admission
    /// approval is raised immediately after the gate is armed, with no turn in
    /// between to absorb the gap.
    fn ensure_approval_gate(&self, approval: Arc<ApprovalRuntime>) {
        ensure_approval_gate_once(
            self.approval_gate_started.as_ref(),
            approval,
            self.registry.clone(),
            self.blocked_waiting.clone(),
            self.conversation.clone(),
            self.core.storage.clone(),
        );
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

/// The headless approval policy loop (AC6). Subscribes to the approval runtime
/// and resolves each request: forward to an attached writer (with a
/// timeout→deny), or — unattended — auto-proceed read-only/`Safe` tools and
/// **deny** anything mutating, recording a durable, resumable transcript record.
async fn run_approval_gate(
    mut rx: tokio::sync::broadcast::Receiver<ApprovalRuntimeEvent>,
    approval: Arc<ApprovalRuntime>,
    registry: Arc<Mutex<ConnRegistry>>,
    blocked: Arc<AtomicUsize>,
    conversation: Arc<Mutex<Conversation>>,
    storage: Arc<dyn crate::domain::ports::StoragePort>,
) {
    loop {
        match rx.recv().await {
            Ok(ApprovalRuntimeEvent::Requested {
                id,
                tool,
                input_preview,
                risk,
                ..
            }) => {
                let writer = { registry.lock().await.writer_tx() };
                match writer {
                    Some(tx) => {
                        // Forward to the writer; arm a timeout→deny so an
                        // unresponsive writer can never hang the turn (AC6 #2).
                        if let Err(e) = tx.try_send(DaemonFrame::ApprovalRequest {
                            request_id: id.clone(),
                            tool: tool.clone(),
                            input_preview,
                            risk,
                        }) {
                            tracing::warn!(error = ?e, "approval request dropped: writer queue full");
                        }
                        let approval2 = approval.clone();
                        let id2 = id.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(APPROVAL_TIMEOUT).await;
                            // No-op if the writer already resolved it.
                            approval2
                                .resolve(
                                    &id2,
                                    crate::domain::models::ApprovalOutcome::Reject {
                                        feedback: Some(
                                            "approval timed out — no response from the attached client".into(),
                                        ),
                                    },
                                )
                                .await;
                        });
                    }
                    None => {
                        // Unattended (AC6 #3): Safe tools auto-proceed; anything
                        // mutating is denied-by-default and recorded (AC6 #5).
                        if risk == ToolRisk::Safe {
                            approval
                                .resolve(&id, crate::domain::models::ApprovalOutcome::Once)
                                .await;
                        } else {
                            approval
                                .resolve(
                                    &id,
                                    crate::domain::models::ApprovalOutcome::Reject {
                                        feedback: Some(
                                            "denied: a side-effecting tool needs approval but no client is attached".into(),
                                        ),
                                    },
                                )
                                .await;
                            blocked.fetch_add(1, Ordering::SeqCst);
                            record_blocked_action(&conversation, &storage, &tool).await;
                        }
                    }
                }
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(
                    missed = n,
                    "approval gate lagged — missed requests may hang"
                );
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// Record a denied-while-unattended action as a durable, visible, resumable
/// transcript message (AC6 #5). The on-attach render + summary is 12.2c.
async fn record_blocked_action(
    conversation: &Arc<Mutex<Conversation>>,
    storage: &Arc<dyn crate::domain::ports::StoragePort>,
    tool: &str,
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
    domain_tx: mpsc::UnboundedSender<AppEvent>,
    node_tree: crate::infrastructure::subagent::NodeTree,
    /// The daemon has ONE conversation; turns must not interleave on it.
    turn_serial: Arc<Mutex<()>>,
    active_channel_origin: Arc<Mutex<ChannelKind>>,
    inbound_results: Arc<Mutex<std::collections::HashMap<AgentId, String>>>,
    registry: Arc<Mutex<ConnRegistry>>,
    blocked_waiting: Arc<AtomicUsize>,
    approval_gate_started: Arc<std::sync::atomic::AtomicBool>,
    turn_complete: Arc<Notify>,
}

impl InboundTurnContext {
    fn ensure_approval_gate(&self, approval: Arc<ApprovalRuntime>) {
        ensure_approval_gate_once(
            self.approval_gate_started.as_ref(),
            approval,
            self.registry.clone(),
            self.blocked_waiting.clone(),
            self.conversation.clone(),
            self.core.storage.clone(),
        );
    }

    /// Drive one inbound peer task to a terminal node state.
    ///
    /// This is the same origination primitive the Unix socket drives —
    /// `drive_preloaded_turn` with `TurnOrigin::RemotePeer` — not a parallel
    /// injection path. The only additions are the ones the front door owes the
    /// node: a terminal transition that distinguishes cancel from failure, and
    /// capture of the answer this task produced.
    async fn run(self, node_id: AgentId, peer_id: PeerId, text: String, cancel: CancellationToken) {
        let runtime = match self.core.ensure_runtime().await {
            Ok(runtime) => runtime,
            Err(error) => {
                tracing::error!(%error, node = %node_id, "A2A inbound turn: runtime unavailable");
                self.node_tree.set_state(&node_id, NodeState::Running).await;
                self.node_tree.set_state(&node_id, NodeState::Failed).await;
                self.node_tree.deregister(&node_id).await;
                return;
            }
        };
        self.ensure_approval_gate(runtime.approval.clone());

        // Serialize on the same mutex every other daemon turn uses. Taken AFTER
        // registration so the node — and therefore `tasks/get` — is live and
        // reports `working` while the task waits its turn, rather than the
        // submitter seeing nothing until the queue drains.
        let _turn_guard = tokio::select! {
            guard = self.turn_serial.lock() => guard,
            () = cancel.cancelled() => {
                self.node_tree.set_state(&node_id, NodeState::Running).await;
                self.node_tree
                    .set_state(&node_id, NodeState::Cancelled)
                    .await;
                self.node_tree.deregister(&node_id).await;
                return;
            }
        };
        if cancel.is_cancelled() {
            self.node_tree.set_state(&node_id, NodeState::Running).await;
            self.node_tree
                .set_state(&node_id, NodeState::Cancelled)
                .await;
            self.node_tree.deregister(&node_id).await;
            return;
        }
        *self.active_channel_origin.lock().await = ChannelKind::Terminal;

        self.node_tree.set_state(&node_id, NodeState::Running).await;
        // Register before driving: Notify is edge-triggered for a waiter
        // created after completion, so this must exist before the provider can
        // emit its terminal chunk.
        let turn_complete = self.turn_complete.notified();
        tokio::pin!(turn_complete);
        turn_complete.as_mut().enable();

        // Tee the turn's event stream. The assistant answer is committed to the
        // shared `conversation` by the attach forwarder, which only runs when a
        // client is attached — so reading the conversation afterwards would make
        // "did the remote peer get its answer?" depend on whether an operator
        // happened to be watching. Accumulating from the turn's own
        // `ProviderChunk` stream does not.
        //
        // Every event is forwarded on to the real `domain_tx`, so the daemon bus,
        // the forwarder, and every other consumer see exactly what they saw
        // before.
        let (tap_tx, mut tap_rx) = mpsc::unbounded_channel::<AppEvent>();
        let downstream = self.domain_tx.clone();
        let collector = tokio::spawn(async move {
            let mut answer = String::new();
            let mut errored = false;
            let mut completed = false;
            while let Some(event) = tap_rx.recv().await {
                // `run_turn`'s join handle resolves `Ok` whether the turn
                // succeeded or died, because failure is reported on the event
                // stream rather than by unwinding. This is therefore the ONLY
                // place "the turn finished" and "the turn worked" are different
                // questions — and there are three ways it can have not worked:
                match &event {
                    // (1) an in-stream error chunk (tool loop limit, stream
                    //     disconnect);
                    AppEvent::ProviderChunk {
                        chunk: StreamChunk::Error { .. },
                        ..
                    } => errored = true,
                    // (2) a provider call that never produced a stream at all —
                    //     `run_turn` emits an error notice and returns;
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
                let _ = downstream.send(event);
            }
            // (3) …and the catch-all: a turn that never reached a final
            // `TurnComplete` did not complete, whatever else it did or did not say.
            (answer, errored, completed)
        });

        let (mut handle, conversation_id) = {
            let mut conversation = self.conversation.lock().await;
            let conversation_id = conversation.id.clone();
            conversation.messages.push(ChatMessage {
                id: generate_message_id(),
                role: MessageRole::User,
                content: text,
                content_blocks: vec![],
                tool_calls: vec![],
                created_at: crate::domain::models::session_meta::now_unix(),
                token_count: None,
                stop_reason: None,
                synthetic: false,
                images: vec![],
                origin: ChannelKind::Terminal,
            });
            (
                runtime.drive_preloaded_turn(
                    &mut conversation,
                    &tap_tx,
                    TurnOrigin::RemotePeer { peer_id },
                    cancel.clone(),
                ),
                conversation_id,
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

        let terminal = match joined {
            None => {
                let (_, _, completed) = collector.await.unwrap_or_default();
                if !completed {
                    // The abort drops `run_turn` before it can emit a terminal
                    // chunk. Drain its tee first, then enqueue a cancelled
                    // terminal event after every partial chunk. The forwarder
                    // treats that event as a buffer reset, never an answer.
                    // The reset must travel the daemon's own forwarder channel in stream order;
                    // InboundTurnContext holds no event_bus handle, and every other chunk of this
                    // turn tees through the same tx.
                    let reset = AppEvent::ProviderChunk {
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
                let (answer, errored, completed) = match collector.await {
                    Ok(result) => result,
                    Err(error) => {
                        tracing::warn!(?error, node = %node_id, "A2A inbound turn collector failed");
                        (String::new(), true, false)
                    }
                };

                let terminal = if join_failed || errored || !completed {
                    // An unexpected join/runtime failure can leave streamed text
                    // without a terminal chunk too. Reset it through the same
                    // ordered path rather than allowing the next turn to own it.
                    if !completed {
                        // Same ordered-reset channel as the cancel path above; the forwarder
                        // consumes domain_tx.
                        let reset = AppEvent::ProviderChunk {
                            conversation_id,
                            chunk: StreamChunk::TurnComplete {
                                stop_reason: StopReason::Cancelled,
                            },
                        };
                        let _ = self.domain_tx.send(reset); // CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS: 18-1b AC6b — ordered cancel reset via the daemon forwarder channel
                    }
                    NodeState::Failed
                } else {
                    if !answer.trim().is_empty() {
                        self.inbound_results
                            .lock()
                            .await
                            .insert(node_id.clone(), answer);
                    }
                    NodeState::Completed
                };

                let folded =
                    tokio::time::timeout(std::time::Duration::from_secs(5), turn_complete).await;
                if folded.is_err() {
                    tracing::warn!(
                        "daemon inbound turn completed but assistant commit was not observed before timeout"
                    );
                }
                terminal
            }
        };
        self.node_tree.set_state(&node_id, terminal).await;
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
    async fn start(
        &self,
        task: crate::domain::ports::InboundPeerTask,
        cancel: CancellationToken,
    ) -> Result<tokio::sync::watch::Receiver<NodeState>, crate::domain::ports::InboundPeerError>
    {
        use crate::domain::models::{AgentMetrics, CapabilityTokenId, Op};
        use crate::domain::ports::InboundPeerError;
        use crate::infrastructure::subagent::{AgentHandle, MailboxBudget};

        // Story 18.3 (AC2) — named so the command loop below can settle a
        // delivery's reservation. It used to be constructed inline in the
        // `AgentHandle`, which is precisely why the loop had nothing to release.
        let mailbox_budget = MailboxBudget::new();
        let (command_tx, mut command_rx) = mpsc::channel(1);
        let (status_tx, _) = tokio::sync::watch::channel(NodeState::Created);
        let (_, metrics_rx) = tokio::sync::watch::channel(AgentMetrics::default());

        // The handle stays LOCAL. The remote submitter never receives a live
        // handle into our tree; its work runs as a node under OUR authority
        // (`OwnershipKind::Peer` / `NodeOrigin::Remote`), which is what makes
        // `relationship_disposition(Peer) == MayRefuse` apply to it.
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

        let context = InboundTurnContext {
            core: self.core.clone(),
            conversation: self.conversation.clone(),
            domain_tx: self.domain_tx.clone(),
            node_tree: self.node_tree.clone(),
            registry: self.registry.clone(),
            blocked_waiting: self.blocked_waiting.clone(),
            approval_gate_started: self.approval_gate_started.clone(),
            turn_serial: self.turn_serial.clone(),
            turn_complete: self.turn_complete.clone(),
            active_channel_origin: self.active_channel_origin.clone(),
            inbound_results: self.inbound_results.clone(),
        };
        tokio::spawn(context.run(
            task.node_id.clone(),
            task.peer_id.clone(),
            task.text,
            cancel,
        ));

        Ok(status)
    }

    async fn request_admission_approval(
        &self,
        peer_id: &PeerId,
        summary: &str,
    ) -> Result<crate::domain::ports::InboundApprovalTicket, crate::domain::ports::InboundPeerError>
    {
        use crate::domain::models::ApprovalOutcome;
        use crate::domain::ports::{InboundApprovalTicket, InboundPeerError};

        let rt = self
            .core
            .ensure_runtime()
            .await
            .map_err(|error| InboundPeerError::Unavailable(error.to_string()))?;
        self.ensure_approval_gate(rt.approval.clone());

        let conversation_id = self.conversation.lock().await.id.clone();
        // `ApprovalRuntime::request` RAISES and returns; it never awaits the
        // human. `request_id.is_some()` is precisely "a person must decide", and
        // that is the flag the caller turns into `auth-required`.
        let (request_id, resolved) = rt
            .approval
            .request(
                crate::domain::models::tool_call::ApprovalSource::RemotePeer {
                    conversation_id,
                    peer_id: peer_id.clone(),
                },
                "a2a/message.send".to_owned(),
                serde_json::json!({
                    "peer": peer_id.as_str(),
                    // A bounded preview: the prompt must be readable, and the
                    // whole instruction could be 64 KiB of remote text.
                    "instruction": summary.chars().take(400).collect::<String>(),
                }),
                ToolRisk::Elevated,
                None,
                None,
            )
            .await;

        let pending = request_id.is_some();
        let (decision_tx, decision) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let granted = matches!(
                resolved.await.map(|resolved| resolved.outcome),
                Ok(ApprovalOutcome::Once
                    | ApprovalOutcome::AlwaysTool { .. }
                    | ApprovalOutcome::AlwaysServer { .. }
                    | ApprovalOutcome::AlwaysAndSave { .. })
            );
            let _ = decision_tx.send(granted);
        });

        Ok(InboundApprovalTicket { pending, decision })
    }

    async fn take_result_text(&self, node_id: &AgentId) -> Option<String> {
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
    use crate::domain::ports::{
        InboundPeerRuntime, InboundPeerTask, PersonaPort, SecurityPort, StoragePort,
        StreamingProvider, ToolSetPort,
    };
    use crate::infrastructure::runtime::event_bus::EventBus;
    use arc_swap::ArcSwap;
    use futures::stream::BoxStream;
    use std::path::Path;
    use tokio::net::UnixStream;

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
            provider,
            app_config: Arc::new(ArcSwap::from_pointee(AppConfig::default())),
            security,
            tools,
            tool_scheduler,
            persona: Arc::new(NoOpPersona),
            context_assembler: Arc::new(ArcSwap::from_pointee(None)),
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

    fn mock_core(
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
    fn mock_core_with_storage(
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
            peer_id,
            text: text.into(),
            subagent_type: "a2a-test".to_owned(),
        }
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
        let server = AttachServer::new(core, conversation.clone(), bus.domain_tx.clone());
        let shutdown = CancellationToken::new();
        let srv = server.clone();
        let shutdown_for_run = shutdown.clone();
        let handle = tokio::spawn(async move {
            srv.run(listener, domain_rx, None, None, shutdown_for_run)
                .await;
        });
        (server, conversation, shutdown, handle)
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
    fn test_signer(seed: u8) -> AgentSigner {
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
    async fn attach_with_proof(
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
        let sender =
            AgentId::from_peer_path(&format!("{}/agent", signer.identity().peer_id.as_str()))
                .expect("peer-rooted sender");
        Box::new(
            signer
                .sign(
                    sender,
                    AgentId::parse("daemon").expect("valid recipient"),
                    CorrelationId::new("corr"),
                    MessageKind::PeerMessage,
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
            .configure_peer_recorder(Arc::new(AcceptingPeerRecorder))
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
    /// AC2: a protocol version mismatch is rejected with a clear Error frame.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn version_mismatch_is_rejected() {
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
        let signer = test_signer(1);
        let _ = read_attach_challenge(&mut stream).await;
        write_frame(
            &mut stream,
            &ClientFrame::Attach {
                protocol_version: PROTOCOL_VERSION + 99,
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
                assert_eq!(client, PROTOCOL_VERSION + 99);
            }
            other => panic!("expected VersionMismatch error, got {other:?}"),
        }
        shutdown.cancel();
        h.abort();
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

    /// AC6: unattended (no writer) — a side-effecting (`Standard`) tool is
    /// denied-by-default and recorded; a read-only (`Safe`) tool auto-proceeds.
    /// No `PermissionMode` ever resolves to `Yolo`.
    #[ignore = "flaky under CI parallelism; gated by nightly daemon L3"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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
        let gate = tokio::spawn(async move { run_approval_gate(events, a, r, b, c, s).await });

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
        assert!(
            matches!(rx2.await.unwrap().outcome, ApprovalOutcome::Once),
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
    /// AC6 #2: writer-connected approval forwards the request and the
    /// timeout-reject path works when the writer never responds.
    ///
    /// Uses a real `tokio::time::sleep` with a 10ms mock timeout (via direct
    /// `resolve`) since `tokio::time::pause` requires `current_thread` runtime
    /// but this crate's tests run on `multi_thread`.
    #[ignore = "flaky under CI parallelism; gated by nightly daemon L3"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn approval_timeout_deny_when_writer_silent() {
        use crate::domain::models::ApprovalOutcome;
        use crate::domain::models::tool_call::ApprovalSource;

        let approval = ApprovalRuntime::new(64, Arc::new(NoOpApprovalPersistence));
        let registry = Arc::new(Mutex::new(ConnRegistry::default()));
        let blocked = Arc::new(AtomicUsize::new(0));
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "timeout-conv".into(),
            ..Default::default()
        }));
        let storage: Arc<dyn crate::domain::ports::StoragePort> =
            Arc::new(FileSystemStorage::with_workspace_root(
                crate::infrastructure::paths::sessions_dir(tempfile::tempdir().unwrap().path()),
                tempfile::tempdir().unwrap().path().to_path_buf(),
            ));

        let a = approval.clone();
        let r = registry.clone();
        let b = blocked.clone();
        let c = conversation.clone();
        let s = storage.clone();
        let events = a.subscribe();
        let gate = tokio::spawn(async move { run_approval_gate(events, a, r, b, c, s).await });

        // Register a writer connection.
        let (tx, mut rx) = mpsc::channel::<DaemonFrame>(1);
        registry.lock().await.conns.push(Conn {
            id: 1,
            tx,
            mode: AttachMode::ReadWrite,
        });

        // Request approval — forwarded to writer.
        let source = ApprovalSource::ForegroundTurn {
            conversation_id: "timeout-conv".into(),
        };
        let (id, recv) = approval
            .request(
                source,
                "Write".into(),
                serde_json::json!({"file_path": "/tmp/x"}),
                ToolRisk::Standard,
                None,
                None,
            )
            .await;
        let request_id = id.expect("slow-path should return an id");

        // Verify ApprovalRequest was forwarded to the writer queue.
        match tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await {
            Ok(Some(DaemonFrame::ApprovalRequest {
                request_id: rid,
                tool,
                ..
            })) => {
                assert_eq!(rid, request_id);
                assert_eq!(tool, "Write");
            }
            other => panic!("expected ApprovalRequest on writer queue, got {other:?}"),
        }

        // Simulate the timeout path: the writer never responds, and the
        // timeout task fires `resolve(Reject)`. We replicate that here.
        approval
            .resolve(
                &request_id,
                ApprovalOutcome::Reject {
                    feedback: Some(
                        "approval timed out — no response from the attached client".into(),
                    ),
                },
            )
            .await;

        // The original caller receives the Reject.
        let result = tokio::time::timeout(std::time::Duration::from_millis(200), recv).await;
        match &result {
            Ok(Ok(resolved)) => assert!(
                matches!(resolved.outcome, ApprovalOutcome::Reject { .. }),
                "silent-writer timeout should resolve to Reject, got {:?}",
                resolved.outcome
            ),
            other => panic!("silent-writer timeout should resolve to Reject, got {other:?}"),
        }
        assert_eq!(
            blocked.load(Ordering::SeqCst),
            0,
            "blocked count unchanged (writer attached)"
        );

        gate.abort();
    }

    /// AC6 #1: approval forward-to-attached-writer round-trip.
    #[ignore = "flaky under CI parallelism; gated by nightly daemon L3"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn approval_forward_to_writer_roundtrip() {
        use crate::domain::models::ApprovalOutcome;
        use crate::domain::models::tool_call::{ApprovalSource, RequestId};

        let approval = ApprovalRuntime::new(64, Arc::new(NoOpApprovalPersistence));
        let registry = Arc::new(Mutex::new(ConnRegistry::default()));
        let blocked = Arc::new(AtomicUsize::new(0));
        let conversation = Arc::new(Mutex::new(Conversation {
            id: "rt-conv".into(),
            ..Default::default()
        }));
        let storage: Arc<dyn crate::domain::ports::StoragePort> =
            Arc::new(FileSystemStorage::with_workspace_root(
                crate::infrastructure::paths::sessions_dir(tempfile::tempdir().unwrap().path()),
                tempfile::tempdir().unwrap().path().to_path_buf(),
            ));

        let a = approval.clone();
        let r = registry.clone();
        let b = blocked.clone();
        let c = conversation.clone();
        let s = storage.clone();
        let events = a.subscribe();
        let gate = tokio::spawn(async move { run_approval_gate(events, a, r, b, c, s).await });

        // Ensure the spawned approval gate has subscribed before issuing the
        // request; otherwise the broadcast event can be missed under full-suite
        // scheduler contention.
        tokio::task::yield_now().await;

        // Register a writer connection.
        let (tx, mut rx) = mpsc::channel::<DaemonFrame>(1);
        registry.lock().await.conns.push(Conn {
            id: 1,
            tx,
            mode: AttachMode::ReadWrite,
        });

        // Request approval.
        let source = ApprovalSource::ForegroundTurn {
            conversation_id: "rt-conv".into(),
        };
        let (id, recv) = approval
            .request(
                source,
                "Write".into(),
                serde_json::json!({"file_path": "/tmp/y"}),
                ToolRisk::Standard,
                None,
                None,
            )
            .await;
        let request_id = id.expect("slow-path should return an id");

        // Writer receives ApprovalRequest.
        match tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await {
            Ok(Some(DaemonFrame::ApprovalRequest {
                request_id: rid,
                tool,
                ..
            })) => {
                assert_eq!(rid, request_id);
                assert_eq!(tool, "Write");
            }
            other => panic!("expected ApprovalRequest, got {other:?}"),
        }

        // Writer approves.
        approval.resolve(&request_id, ApprovalOutcome::Once).await;

        // Original caller receives the approval.
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), recv).await;
        match &outcome {
            Ok(Ok(resolved)) => assert_eq!(
                resolved.outcome,
                ApprovalOutcome::Once,
                "round-trip should deliver Once"
            ),
            other => panic!("round-trip should deliver Once, got {other:?}"),
        }
        assert_eq!(blocked.load(Ordering::SeqCst), 0, "no blocks");

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
                mode: AttachMode::ReadWrite,
            });
        }
        (server, rx, conn_id)
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
                mode: AttachMode::ReadOnly,
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
        let (server, _conversation, shutdown, handle) = spawn_inbound_forwarder(
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
            let task = inbound_task(peer_id.clone(), "repeat");
            let node_id = task.node_id.clone();
            let mut status = server
                .start(task, CancellationToken::new())
                .await
                .expect("a terminal inbound task frees root capacity for the next task");
            let terminal = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                wait_for_terminal(&mut status),
            )
            .await
            .expect("inbound turn should reach a terminal state");
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
        let task = inbound_task(test_signer(81).identity().peer_id.clone(), "queued");
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
            mode: AttachMode::ReadWrite,
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
                server.ensure_approval_gate(approval);
            }));
        }
        barrier.wait().await;
        for caller in callers {
            caller.await.unwrap();
        }
        assert!(server.approval_gate_started.load(Ordering::SeqCst));

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
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), writer_rx.recv())
                .await
                .is_err(),
            "only the winning gate may forward the request"
        );
        approval.resolve(&request_id, ApprovalOutcome::Once).await;
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
        let server = AttachServer::new(core, conversation.clone(), bus.domain_tx.clone());
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
        let first_task = inbound_task(peer_id.clone(), "first");
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

        let second_task = inbound_task(peer_id, "second");
        let second_node_id = second_task.node_id.clone();
        let mut second_status = server
            .start(second_task, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                wait_for_terminal(&mut second_status),
            )
            .await
            .expect("second inbound turn must complete"),
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
            mode: AttachMode::ReadWrite,
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
        server
            .inbound_results
            .lock()
            .await
            .insert(node_id.clone(), "one-time answer".to_owned());

        assert_eq!(
            server.take_result_text(&node_id).await.as_deref(),
            Some("one-time answer")
        );
        assert_eq!(server.take_result_text(&node_id).await, None);
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
}
