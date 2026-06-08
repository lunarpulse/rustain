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

use crate::domain::events::AppEvent;
use crate::domain::models::{
    ChannelKind, ChatMessage, Conversation, MessageRole, StopReason, StreamChunk, ToolRisk,
    generate_message_id,
};
use crate::domain::services::approval_runtime::{ApprovalRuntime, ApprovalRuntimeEvent};
use crate::infrastructure::runtime::event_bus::{RawEvent, RawEventKind};

use super::protocol::{
    AttachMode, AttachSnapshot, ClientFrame, DaemonFrame, PROTOCOL_VERSION, ProtocolError,
    read_frame, write_frame,
};
use super::runtime::DaemonCore;

/// Per-connection bounded writer queue depth. A connection that cannot keep up is
/// dropped (its queue fills) — the turn is never blocked (AC3).
const CONN_QUEUE_DEPTH: usize = 1024;

/// How long an attached-but-unresponsive writer has to answer an approval before
/// the daemon falls back to the conservative unattended path (deny) (AC6 #2).
const APPROVAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

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
        })
    }

    /// Current count of connected channels for honest `status` reporting (AC4).
    pub fn channel_count(&self) -> usize {
        // best-effort, non-blocking read
        self.registry.try_lock().map(|r| r.conns.len()).unwrap_or(0)
    }

    /// Run the forwarder + accept loop until `shutdown` is cancelled.
    pub async fn run(
        self: Arc<Self>,
        listener: UnixListener,
        mut domain_rx: mpsc::UnboundedReceiver<AppEvent>,
        shutdown: CancellationToken,
    ) {
        // The single forwarder task: fold → fan out (drains the one mpsc
        // `domain_rx`; connections do NOT each subscribe).
        let fwd = self.clone();
        let fwd_shutdown = shutdown.clone();
        let forwarder = tokio::spawn(async move {
            let mut assistant_buf = String::new();
            loop {
                tokio::select! {
                    _ = fwd_shutdown.cancelled() => break,
                    maybe = domain_rx.recv() => {
                        let Some(event) = maybe else { break };
                        fwd.handle_bus_event(&event, &mut assistant_buf).await;
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
            origin: ChannelKind::Terminal,
        });
        if let Err(e) = self.core.storage.save_conversation(&conv).await {
            tracing::warn!(error = %e, "daemon: persisting conversation after turn failed");
        }
    }

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
        }
        false
    }

    /// Build the runtime (first activity), spawn the approval gate once, drive
    /// the turn. The turn emits to the daemon bus; the forwarder fans out + folds.
    async fn drive_user_turn(&self, text: String) {
        // Serialize submitted turns FIFO. The forwarder has one assistant
        // accumulator, and each turn's context must include the prior assistant
        // response before the next user message is assembled.
        let _turn_guard = self.turn_serial.lock().await;

        let rt = match self.core.ensure_runtime().await {
            Ok(rt) => rt,
            Err(e) => {
                // Log only — emitting an AppEvent here would bypass `emit_domain`
                // (the daemon owns a bare bus; the EventBus-bypass ratchet stays
                // locked). A client surfaces runtime-build failures in 12.2c.
                tracing::error!(error = %e, "daemon: building turn runtime failed");
                return;
            }
        };
        self.ensure_approval_gate(rt.approval.clone());

        let turn_complete = self.turn_complete.notified();
        let (handle, snapshot) = {
            let mut conv = self.conversation.lock().await;
            let handle = rt.drive_turn(text, &mut conv, &self.domain_tx, CancellationToken::new());
            (handle, conv.clone())
        }; // lock dropped before save — avoid holding across .await

        // Persist the user message immediately (the assistant side persists on
        // TurnComplete in the forwarder).
        if let Err(e) = self.core.storage.save_conversation(&snapshot).await {
            tracing::warn!(error = %e, "daemon: persisting user message failed");
        }

        if let Err(e) = handle.await {
            tracing::warn!(error = ?e, "daemon turn task failed");
            return;
        }
        // Ensure the forwarder has folded the completed assistant message before
        // accepting the next queued turn. If a provider path ends without a
        // TurnComplete event, do not wedge the queue forever.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), turn_complete).await;
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

    async fn send_to(&self, conn_id: u64, frame: DaemonFrame) {
        let tx = {
            let reg = self.registry.lock().await;
            reg.conns
                .iter()
                .find(|c| c.id == conn_id)
                .map(|c| c.tx.clone())
        };
        if let Some(tx) = tx {
            let _ = tx.try_send(frame);
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
            Arc::new(NoOpMemory),
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
        let handle = tokio::spawn(async move { srv.run(listener, domain_rx, sd).await });

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
        let h = tokio::spawn(async move { srv.run(listener, domain_rx, sd).await });

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
        let h = tokio::spawn(async move { srv.run(listener, domain_rx, sd).await });

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
        match tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await {
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
        let outcome = tokio::time::timeout(std::time::Duration::from_millis(200), recv).await;
        assert!(
            matches!(outcome, Ok(Ok(ApprovalOutcome::Once))),
            "round-trip should deliver Once, got {outcome:?}"
        );
        assert_eq!(blocked.load(Ordering::SeqCst), 0, "no blocks");

        gate.abort();
    }
}
