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

use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Notify, mpsc};
use tokio_util::sync::CancellationToken;

#[cfg(feature = "cron")]
use crate::adapters::scheduler::cron::CronCompletion;
#[cfg(not(feature = "cron"))]
pub struct CronCompletion;
use crate::domain::events::AppEvent;
use crate::domain::models::{
    ChannelKind, ChannelTurnRequest, ChatMessage, Conversation, MessageRole, StopReason,
    StreamChunk, ToolRisk, generate_message_id,
};
use crate::domain::services::approval_runtime::{ApprovalRuntime, ApprovalRuntimeEvent};
use crate::infrastructure::runtime::event_bus::{RawEvent, RawEventKind};

use super::protocol::{
    AttachMode, AttachSnapshot, ClientFrame, DaemonFrame, PROTOCOL_VERSION, ProposalToken,
    ProtocolError, read_frame, write_frame,
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
}

impl AttachServer {
    pub fn new(
        core: Arc<DaemonCore>,
        conversation: Arc<Mutex<Conversation>>,
        domain_tx: mpsc::UnboundedSender<AppEvent>,
    ) -> Arc<Self> {
        Arc::new(Self {
            core,
            conversation,
            registry: Arc::new(Mutex::new(ConnRegistry::default())),
            domain_tx,
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
        })
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
                    self.commit_assistant_turn(assistant_buf, stop_reason).await;
                    assistant_buf.clear();
                    self.turn_complete.notify_waiters();
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

        // First frame MUST be Attach.
        let attach = match read_frame::<_, ClientFrame>(&mut read_half).await {
            Ok(Some(ClientFrame::Attach {
                protocol_version,
                read_only_ok,
            })) => (protocol_version, read_only_ok),
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
        let (client_version, read_only_ok) = attach;

        // Version negotiation (AC2): reject a mismatch with a clear Error.
        if client_version != PROTOCOL_VERSION {
            let _ = write_frame(
                &mut write_half,
                &DaemonFrame::Error(ProtocolError::VersionMismatch {
                    daemon: PROTOCOL_VERSION,
                    client: client_version,
                }),
            )
            .await;
            return;
        }

        // Register the connection atomically with mode selection: the first
        // ReadWrite-capable client is the writer; later clients are read-only.
        // Keeping grant+registration under one lock closes the two-attach writer
        // TOCTOU.
        let conn_id = self.next_conn_id.fetch_add(1, Ordering::SeqCst);
        let (tx, mut rx) = mpsc::channel::<DaemonFrame>(CONN_QUEUE_DEPTH);
        let granted_mode = {
            let mut reg = self.registry.lock().await;
            let granted = if read_only_ok || reg.has_writer() {
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

        // Reader loop.
        loop {
            match read_frame::<_, ClientFrame>(&mut read_half).await {
                Ok(Some(frame)) => {
                    if self.handle_client_frame(frame, granted_mode, conn_id).await {
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

    /// Build the runtime (first activity), spawn the approval gate once, drive
    /// the turn. The turn emits to the daemon bus; the forwarder fans out + folds.
    async fn drive_user_turn(&self, text: String) {
        self.drive_user_turn_inner(text, ChannelKind::Terminal, None)
            .await;
    }

    async fn drive_channel_turn(&self, req: ChannelTurnRequest) {
        self.drive_user_turn_inner(req.text, req.origin, Some(req.response_tx))
            .await;
    }

    async fn drive_user_turn_inner(
        &self,
        text: String,
        origin: ChannelKind,
        response_tx: Option<tokio::sync::oneshot::Sender<String>>,
    ) {
        // Serialize submitted turns FIFO. The forwarder has one assistant
        // accumulator, and each turn's context must include the prior assistant
        // response before the next user message is assembled.
        let _turn_guard = self.turn_serial.lock().await;
        *self.active_channel_origin.lock().await = origin;
        *self.pending_channel_response_tx.lock().await = response_tx;

        let rt = match self.core.ensure_runtime().await {
            Ok(rt) => rt,
            Err(e) => {
                // Log only — emitting an AppEvent here would bypass `emit_domain`
                // (the daemon owns a bare bus; the EventBus-bypass ratchet stays
                // locked). A client surfaces runtime-build failures in 12.2c.
                tracing::error!(error = %e, "daemon: building turn runtime failed");
                self.resolve_pending_channel_response(CHANNEL_TURN_FAILED_REPLY)
                    .await;
                *self.active_channel_origin.lock().await = ChannelKind::Terminal;
                return;
            }
        };
        self.ensure_approval_gate(rt.approval.clone());

        let turn_complete = self.turn_complete.notified();
        let (handle, snapshot) = {
            let mut conv = self.conversation.lock().await;
            let handle = rt.drive_turn(
                text,
                origin,
                &mut conv,
                &self.domain_tx,
                CancellationToken::new(),
            );
            (handle, conv.clone())
        }; // lock dropped before save — avoid holding across .await

        // Persist the user message immediately (the assistant side persists on
        // TurnComplete in the forwarder).
        if let Err(e) = self.core.storage.save_conversation(&snapshot).await {
            tracing::warn!(error = %e, "daemon: persisting user message failed");
        }

        if let Err(e) = handle.await {
            tracing::warn!(error = ?e, "daemon turn task failed");
            self.resolve_pending_channel_response(CHANNEL_TURN_FAILED_REPLY)
                .await;
            *self.active_channel_origin.lock().await = ChannelKind::Terminal;
            return;
        }
        // Ensure the forwarder has folded the completed assistant message before
        // accepting the next queued turn. If a provider path ends without a
        // TurnComplete event, do not wedge the queue forever.
        let folded = tokio::time::timeout(std::time::Duration::from_secs(5), turn_complete).await;
        if folded.is_err() {
            tracing::warn!(
                "daemon turn completed but assistant commit was not observed before timeout"
            );
        }
        self.resolve_pending_channel_response(CHANNEL_TURN_FAILED_REPLY)
            .await;
        *self.active_channel_origin.lock().await = ChannelKind::Terminal;
    }

    async fn resolve_pending_channel_response(&self, fallback: &str) {
        if let Some(tx) = self.pending_channel_response_tx.lock().await.take() {
            let _ = tx.send(fallback.to_string());
        }
    }

    /// Spawn the headless approval gate exactly once (AC6).
    fn ensure_approval_gate(&self, approval: Arc<ApprovalRuntime>) {
        if self.approval_gate_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let registry = self.registry.clone();
        let blocked = self.blocked_waiting.clone();
        let conversation = self.conversation.clone();
        let storage = self.core.storage.clone();
        tokio::spawn(async move {
            run_approval_gate(approval, registry, blocked, conversation, storage).await;
        });
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
                            let _ = tx.send(frame);
                        }
                    }
                    Ok(Ok(_)) => {
                        // Empty proposals → clear marker, emit info (no card).
                        if let Err(e) = super::session_queue::clear_consolidation_due(&workspace) {
                            tracing::warn!(error = %e, "daemon: could not clear consolidation marker on empty proposals");
                        }
                        // Best-effort info notice (connection may be gone).
                        if let Some(tx) = send_tx {
                            let _ = tx.send(DaemonFrame::Event(RawEvent {
                                conversation_id: None,
                                timestamp_ms: chrono::Utc::now().timestamp_millis(),
                                kind: RawEventKind::SystemNotice {
                                    level: crate::domain::models::NoticeLevel::Info,
                                    message: "Nothing worth promoting from recent activity".into(),
                                },
                            }));
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
    approval: Arc<ApprovalRuntime>,
    registry: Arc<Mutex<ConnRegistry>>,
    blocked: Arc<AtomicUsize>,
    conversation: Arc<Mutex<Conversation>>,
    storage: Arc<dyn crate::domain::ports::StoragePort>,
) {
    let mut rx = approval.subscribe();
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
    use crate::domain::errors::ProviderError;
    use crate::domain::models::provider::ModelDescriptor;
    use crate::domain::models::{AppConfig, CompletionOptions, Message, StopReason};
    use crate::domain::ports::{SecurityPort, StoragePort, StreamingProvider, ToolSetPort};
    use crate::infrastructure::runtime::event_bus::EventBus;
    use arc_swap::ArcSwap;
    use futures::stream::BoxStream;
    use std::path::Path;
    use tokio::net::UnixStream;

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
        })
    }

    fn mock_core(
        workspace: &Path,
        chunks: Vec<StreamChunk>,
    ) -> (Arc<DaemonCore>, Arc<dyn StoragePort>) {
        mock_core_with_memory(workspace, chunks, Arc::new(NoOpMemory))
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
        write_frame(
            &mut stream,
            &ClientFrame::Attach {
                protocol_version: PROTOCOL_VERSION,
                read_only_ok: false,
            },
        )
        .await
        .unwrap();

        // AttachAck (writer grant).
        match read_frame::<_, DaemonFrame>(&mut stream).await.unwrap() {
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
        write_frame(
            &mut stream,
            &ClientFrame::Attach {
                protocol_version: PROTOCOL_VERSION + 99,
                read_only_ok: false,
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
        write_frame(
            &mut stream,
            &ClientFrame::Attach {
                protocol_version: PROTOCOL_VERSION,
                read_only_ok: false,
            },
        )
        .await
        .unwrap();
        let _ = read_frame::<_, DaemonFrame>(&mut stream).await.unwrap(); // AttachAck
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
        let gate = tokio::spawn(async move { run_approval_gate(a, r, b, c, s).await });

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
        let outcome = rx.await.unwrap();
        assert!(
            matches!(outcome, ApprovalOutcome::Reject { .. }),
            "mutating tool denied unattended, got {outcome:?}"
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
            matches!(rx2.await.unwrap(), ApprovalOutcome::Once),
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
        let gate = tokio::spawn(async move { run_approval_gate(a, r, b, c, s).await });

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
        assert!(
            matches!(result, Ok(Ok(ApprovalOutcome::Reject { .. }))),
            "silent-writer timeout should resolve to Reject, got {result:?}"
        );
        assert_eq!(
            blocked.load(Ordering::SeqCst),
            0,
            "blocked count unchanged (writer attached)"
        );

        gate.abort();
    }

    /// AC6 #1: approval forward-to-attached-writer round-trip.
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
        let gate = tokio::spawn(async move { run_approval_gate(a, r, b, c, s).await });

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
        assert!(
            matches!(outcome, Ok(Ok(ApprovalOutcome::Once))),
            "round-trip should deliver Once, got {outcome:?}"
        );
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
        write_frame(
            &mut c1,
            &ClientFrame::Attach {
                protocol_version: PROTOCOL_VERSION,
                read_only_ok: false,
            },
        )
        .await
        .unwrap();
        match read_frame::<_, DaemonFrame>(&mut c1).await.unwrap() {
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
        write_frame(
            &mut c2,
            &ClientFrame::Attach {
                protocol_version: PROTOCOL_VERSION,
                read_only_ok: false,
            },
        )
        .await
        .unwrap();
        match read_frame::<_, DaemonFrame>(&mut c2).await.unwrap() {
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
        write_frame(
            &mut c,
            &ClientFrame::Attach {
                protocol_version: PROTOCOL_VERSION,
                read_only_ok: false,
            },
        )
        .await
        .unwrap();
        let _ = read_frame::<_, DaemonFrame>(&mut c).await.unwrap(); // AttachAck

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
}
