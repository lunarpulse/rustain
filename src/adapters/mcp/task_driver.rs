//! MCP task driver — materializes a long-running MCP task as a first-class
//! durable node and drives its lifecycle (17.5a: AC1, AC4, AC5, AC6).
//!
//! Shape mirrors `A2aDelegationRuntime` (`a2a/driver.rs`) — `NodeHandle::Local`
//! + a driver task owning the transport — but reaches lifecycle ONLY through
//! domain ports (`TaskNodes`, `SupervisedNodes`, `RoomJournal`), never by
//! importing `infrastructure/` (ADR-17-5-01 D2; the a2a driver's
//! `driver.rs:37-38` imports are a known, unremediated violation this story
//! does not propagate).
//!
//! Lifecycle rules that bind here:
//! - `try_set_state` exclusively; every error propagates loudly (R-6).
//! - Cancel cascades on the `tasks/cancel` ACK, never on an observed
//!   `cancelled` status (R-15), routed through `SupervisedNodes::cascade_kill`
//!   for external kills.
//! - `isError: true` maps to `Completed`, never `Failed` (R-14).
//! - `input_required` drives a real `Running → Waiting` transition (17.5b /
//!   AC1); the node parks durably and resumes on a submitted answer (R-5).
//! - The poll loop never inherits `call_tool`'s 60s bound (R-16.4).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::domain::clock::Clock;
use crate::domain::models::orchestration::WaitReason;
use crate::domain::models::{
    AgentId, ArtifactId, NodeState, Op, RoomEvent, TicketResolution, ToolResult,
};
use crate::domain::ports::{ArtifactSink, RoomJournal, SupervisedNodes, TaskNodes, TaskNodesError};

use super::error::McpError;
use super::task_transport::McpTaskTransport;
use super::tasks::{CreateTaskReply, InputRequest, InputResponse, TaskStatus};

/// Poll cadence for the driver loop. Mirrors the `A2aTaskTransport` precedent
/// shape (`a2a/lifecycle.rs`); redeclared here because adapter-to-adapter
/// imports are forbidden.
#[derive(Debug, Clone)]
pub struct PollConfig {
    /// Fallback poll interval when the server sends no `pollIntervalMs`.
    pub interval: Duration,
    /// Total bound on the driver's lifetime; a task that outlives it is
    /// failed loudly rather than stranded.
    pub deadline: Duration,
    /// Bound on a single `tasks/get` round-trip. Independent of
    /// `call_tool`'s 60s bound, which applies only to the initial
    /// `tools/call` (R-16.4).
    pub request_timeout: Duration,
    /// Cap on observed status TRANSITIONS (not polls), mirroring
    /// `MAILBOX_CAP`. Polls themselves are bounded by `deadline`; a
    /// long-running task in a single state must never trip this cap.
    pub max_status_updates: usize,
}

impl Default for PollConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(1),
            deadline: Duration::from_secs(3600),
            request_timeout: Duration::from_secs(30),
            max_status_updates: 64,
        }
    }
}

impl PollConfig {
    #[cfg(test)]
    pub fn fast_config() -> Self {
        Self {
            interval: Duration::from_millis(5),
            deadline: Duration::from_secs(10),
            request_timeout: Duration::from_secs(2),
            max_status_updates: 64,
        }
    }
}

/// The operator's answer to one or more outstanding input requests on a
/// `Waiting` MCP task (17.5b / AC1). Arrives via the daemon `ClientFrame`
/// surface (ADR-17-5-02 D2) and is routed to the driver's answer channel by
/// [`McpTaskRuntime::submit_answer`].
#[derive(Debug, Clone)]
pub struct InputAnswer {
    /// Responses keyed by the outstanding input-request key (R-6). The driver
    /// refuses any key not currently outstanding before forwarding.
    pub responses: std::collections::BTreeMap<String, InputResponse>,
}

/// Why a submitted answer did not reach a live `Waiting` driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerRoutingError {
    /// No driver is registered for this node (unknown or already terminal).
    UnknownNode,
    /// The driver has exited (channel closed) — treat as terminal.
    DriverGone,
    /// A driver exists, but the node has not entered its current `Waiting`
    /// epoch. Refuse rather than buffer an answer for a predictable future key.
    NotWaiting,
}

impl std::fmt::Display for AnswerRoutingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownNode => write!(f, "no live MCP task driver for this node"),
            Self::DriverGone => write!(f, "MCP task driver has exited"),
            Self::NotWaiting => write!(f, "MCP task is not waiting for an answer"),
        }
    }
}

impl std::error::Error for AnswerRoutingError {}

/// Everything a live MCP task node needs, injected at the composition root.
/// One runtime per `McpClientAdapter`; shared by every task the server
/// creates.
pub struct McpTaskRuntime {
    nodes: Arc<dyn TaskNodes>,
    supervised: Arc<dyn SupervisedNodes>,
    room: Arc<dyn RoomJournal>,
    clock: Arc<dyn Clock>,
    /// 17.5b — the sink for input-request artifacts + tickets (AC3). Optional
    /// only because the artifact store is built AFTER the runtime at some
    /// composition roots; wired via [`Self::set_artifact_sink`] before the
    /// runtime accepts tasks. Absence at a `Waiting` transition is a loud
    /// error (AC3 is load-bearing on the artifact), never a silent no-op.
    /// `std::sync::Mutex` (not tokio) so SYNC composition-root closures can
    /// set it; the guard is never held across an await.
    artifact: std::sync::Mutex<Option<Arc<dyn ArtifactSink>>>, // CONFORMANCE_EXCEPTION_STD_SYNC_LOCK: sync composition-root closures (composition/mod.rs, acp/run.rs) set the sink without an async context; tokio::sync::Mutex would require `.await`. Guard is held only for a brief clone, never across `.await`. ADR-17-5-02 D2.

    /// 17.5b — per-node answer channel. A driver registers on start and the
    /// operator's `ClientFrame` answer is routed here by [`Self::submit_answer`].
    answers: Mutex<HashMap<AgentId, tokio::sync::mpsc::Sender<InputAnswer>>>,
    /// Live task nodes owned by this client, for session teardown (AC5).
    /// Each node carries a runtime-owned cancel signal — the checkpoint-free
    /// teardown trigger, distinct from the tree's handle token — plus a
    /// `tearing_down` gate that closes the admission window during
    /// `kill_all_tasks` (P2 / D1).
    live: Mutex<LiveState>,
    poll: PollConfig,
}

#[derive(Default)]
struct LiveState {
    tasks: HashMap<AgentId, CancellationToken>,
    tearing_down: bool,
}

impl McpTaskRuntime {
    pub fn new(
        nodes: Arc<dyn TaskNodes>,
        supervised: Arc<dyn SupervisedNodes>,
        room: Arc<dyn RoomJournal>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            nodes,
            supervised,
            room,
            clock,
            artifact: std::sync::Mutex::new(None), // CONFORMANCE_EXCEPTION_STD_SYNC_LOCK: ADR-17-5-02 D2 (sync composition-root setter; never held across await).
            answers: Mutex::new(HashMap::new()),
            live: Mutex::new(LiveState::default()),
            poll: PollConfig::default(),
        }
    }

    /// 17.5b — wire the input-request artifact sink. Called at every
    pub fn set_artifact_sink(self: &Arc<Self>, sink: Arc<dyn ArtifactSink>) {
        *self.artifact.lock().expect("artifact sink lock poisoned") = Some(sink);
    }

    /// 17.5b — route the operator's answer to the driver of `node_id`. The
    /// driver correlates by key (R-6) and validates against `requestedSchema`
    /// (D4) before forwarding `tasks/update`; a refused answer leaves the node
    /// `Waiting` and returns `Ok` (the refusal is observable, not an error).
    pub async fn submit_answer(
        &self,
        node_id: &AgentId,
        answer: InputAnswer,
    ) -> Result<(), AnswerRoutingError> {
        let sender = {
            let answers = self.answers.lock().await;
            answers.get(node_id).cloned()
        };
        let Some(sender) = sender else {
            let live = self.live.lock().await.tasks.contains_key(node_id);
            return Err(if live {
                AnswerRoutingError::NotWaiting
            } else {
                AnswerRoutingError::UnknownNode
            });
        };
        sender
            .send(answer)
            .await
            .map_err(|_| AnswerRoutingError::DriverGone)
    }

    pub fn with_poll_config(mut self, poll: PollConfig) -> Self {
        self.poll = poll;
        self
    }

    /// Live task node ids (test observability + teardown).
    pub async fn live_tasks(&self) -> Vec<AgentId> {
        self.live.lock().await.tasks.keys().cloned().collect()
    }

    /// Register the task as a durable node and spawn its driver. Called from
    /// `McpClientAdapter::call_tool` when a `tools/call` reply carries
    /// `resultType: "task"`. Returns the `ToolResult` the LLM sees: the task
    /// is running; its state is observable via the room projection.
    pub async fn start_task(
        self: &Arc<Self>,
        server_id: &str,
        reply: CreateTaskReply,
        transport: Arc<dyn McpTaskTransport>,
        owner_cancel: CancellationToken,
    ) -> Result<ToolResult, McpError> {
        let task_id = reply.task.task_id.clone();
        // P7: an empty server taskId mints an unrecoverable (`t-`) and colliding
        // node id; refuse it and best-effort cancel the remote task.
        if task_id.is_empty() {
            let _ = transport.tasks_cancel("").await;
            return Err(McpError::TaskProtocol(
                "server returned a task with an empty taskId".into(),
            ));
        }
        let node_id = mint_mcp_node_id(server_id, &task_id);

        // Runtime-owned, checkpoint-free cancel signal for this node. The driver
        // watches it alongside the owner/handle tokens; teardown fires it to
        // reach the driver's ack-gated cancel WITHOUT cascade_kill's
        // pre-checkpoint (D1).
        let node_cancel = CancellationToken::new();

        // P2: close the admission window under the same lock `kill_all_tasks`
        // snapshots — refuse a task that arrives once teardown has begun (or the
        // owner token is already cancelled), best-effort cancel it, and return a
        // text result rather than register a node nothing will reap.
        {
            let live = self.live.lock().await;
            if live.tearing_down || owner_cancel.is_cancelled() {
                drop(live);
                let _ = transport.tasks_cancel(&task_id).await;
                let seq = self.clock.wall_now_ms();
                return Ok(ToolResult {
                    tool_use_id: format!("mcp-task-closing-{seq}"),
                    content: format!(
                        "MCP server '{server_id}' created task {task_id} while this client \
                         was disconnecting; a cancellation was requested and no durable node \
                         was materialized."
                    ),
                    is_error: false,
                });
            }
        }

        let handle = match self.nodes.register_task_node(&node_id, "mcp-task").await {
            Ok(handle) => handle,
            Err(e) => {
                // P6: the remote task already exists — do not orphan it.
                let _ = transport.tasks_cancel(&task_id).await;
                return Err(McpError::TaskProtocol(format!("node registration: {e}")));
            }
        };

        self.live
            .lock()
            .await
            .tasks
            .insert(node_id.clone(), node_cancel.clone());

        // The sender is exposed through `answers` only while the driver is in
        // its current Waiting epoch. Pre-wait and between-wait submissions are
        // rejected instead of buffered for a predictable future key.
        let (answer_tx, answer_rx) = tokio::sync::mpsc::channel::<InputAnswer>(8);

        let runtime = Arc::clone(self);
        let driver_node = node_id.clone();
        let driver_server = server_id.to_string();
        let driver_task = task_id.clone();
        tokio::spawn(async move {
            run_driver(
                runtime.clone(),
                driver_node.clone(),
                driver_server,
                driver_task,
                transport,
                owner_cancel,
                node_cancel,
                handle,
                reply,
                answer_tx,
                answer_rx,
            )
            .await;
            // Deregister the answer route once the driver is gone.
            runtime.answers.lock().await.remove(&driver_node);
        });

        let seq = self.clock.wall_now_ms();
        Ok(ToolResult {
            tool_use_id: format!("mcp-task-{seq}"),
            content: format!(
                "MCP task started on server '{server_id}' (taskId: {task_id}). \
                 The task runs asynchronously; its state is a durable node \
                 ({node_id}) visible in the orchestration room. Poll `tasks/get` \
                 or inspect the room for progress."
            ),
            is_error: false,
        })
    }

    /// Session teardown (AC4/AC5), ack-gated (D1): (1) flip `tearing_down` to
    /// close admission and snapshot the live nodes, (2) fire each node's cancel
    /// so its driver issues a real `tasks/cancel` and terminalizes ON THE ACK,
    /// (3) await self-termination (bounded), (4) supervised-cascade every
    /// snapshot node to reap/deregister — a no-op checkpoint on the already
    /// terminal ones (`checkpoint_cancelled_batch` skips terminal), and a
    /// bounded FORCE path for any straggler the server never acked.
    pub async fn kill_all_tasks(&self) {
        let snapshot: Vec<(AgentId, CancellationToken)> = {
            let mut live = self.live.lock().await;
            live.tearing_down = true;
            live.tasks
                .iter()
                .map(|(node, token)| (node.clone(), token.clone()))
                .collect()
        };

        // Phase 1: signal the ack-gated cancel path in every driver.
        for (_node, token) in &snapshot {
            token.cancel();
        }

        // Phase 2: await each driver terminalizing itself (it removes itself
        // from `live` on exit), bounded so a wedged server cannot hang teardown.
        let deadline =
            tokio::time::Instant::now() + self.poll.request_timeout + Duration::from_secs(2);
        loop {
            let outstanding = self.live.lock().await.tasks.len();
            if outstanding == 0 || tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // Phase 3: reap/deregister. On an already-terminal node this only
        // deregisters; on a straggler it is the explicit bounded FORCE path.
        for (node_id, _token) in &snapshot {
            match self
                .supervised
                .cascade_kill(node_id, Duration::from_secs(5))
                .await
            {
                Ok(_) | Err(crate::domain::ports::SupervisedNodesError::NotFound(_)) => {}
                Err(error) => {
                    tracing::warn!(%node_id, %error, "MCP task node cascade failed during teardown");
                }
            }
        }

        // Re-open admission for a subsequent reconnect on the same runtime.
        self.live.lock().await.tearing_down = false;
    }

    async fn unmark_live(&self, node_id: &AgentId) {
        self.live.lock().await.tasks.remove(node_id);
    }
}

/// Mint a node id that durably and reversibly encodes `(server_id, task_id)`
/// as safe `AgentId` path segments. Both values cross a trust boundary (the
/// task id is server-chosen and untrusted), so neither may be interpolated
/// into an `AgentId` before base64 encoding — the 17.4b HIGH finding
/// (`mint_node_id` panic on untrusted ids) applies verbatim (R-16.2).
pub fn mint_mcp_node_id(server_id: &str, task_id: &str) -> AgentId {
    let server = URL_SAFE_NO_PAD.encode(server_id.as_bytes());
    let task = URL_SAFE_NO_PAD.encode(task_id.as_bytes());
    AgentId::from_validated(format!("mcp/s-{server}/t-{task}"))
}

/// Recover `(server_id, task_id)` from a node id minted by
/// [`mint_mcp_node_id`]. Returns `None` for malformed, empty, or non-MCP
/// node ids.
pub fn parse_mcp_node_id(node_id: &AgentId) -> Option<(String, String)> {
    let rest = node_id.as_str().strip_prefix("mcp/")?;
    let server = rest.strip_prefix("s-")?;
    let (server_b64, task_b64) = server.split_once("/t-")?;
    let server_bytes = URL_SAFE_NO_PAD.decode(server_b64).ok()?;
    let task_bytes = URL_SAFE_NO_PAD.decode(task_b64).ok()?;
    let server_id = String::from_utf8(server_bytes).ok()?;
    let task_id = String::from_utf8(task_bytes).ok()?;
    if server_id.is_empty() || task_id.is_empty() {
        return None;
    }
    Some((server_id, task_id))
}

/// Wire status → node state. `Completed`/`Failed`/`Cancelled` are terminal;
/// `Working` is non-terminal. `InputRequired` is a DISTINCT non-terminal
/// state — the poll loop drives `Running → Waiting` on it (17.5b / R-5).
fn terminal_node_state_for(status: &TaskStatus) -> Option<NodeState> {
    match status {
        TaskStatus::Completed => Some(NodeState::Completed),
        TaskStatus::Failed => Some(NodeState::Failed),
        TaskStatus::Cancelled => Some(NodeState::Cancelled),
        TaskStatus::Working => None,
        TaskStatus::InputRequired => None,
    }
}

/// The driver task: owns the transport for one MCP task node until terminal.
///
/// Cancellation wiring (17.4b's two HIGH findings closed here by
/// construction): the node's own `CancellationToken`, an `Op::Kill` arriving
/// through the command channel, and the owner (adapter session) token all
/// converge on one cooperative cancel path that issues a real `tasks/cancel`
/// and drives the local cascade on the ack.
async fn run_driver(
    runtime: Arc<McpTaskRuntime>,
    node_id: AgentId,
    server_id: String,
    task_id: String,
    transport: Arc<dyn McpTaskTransport>,
    owner_cancel: CancellationToken,
    node_cancel: CancellationToken,
    mut handle: crate::domain::ports::TaskNodeHandle,
    initial: CreateTaskReply,
    answer_tx: tokio::sync::mpsc::Sender<InputAnswer>,
    mut answer_rx: tokio::sync::mpsc::Receiver<InputAnswer>,
) {
    let outcome = run_driver_inner(
        &runtime,
        &node_id,
        &server_id,
        &task_id,
        &*transport,
        &owner_cancel,
        &node_cancel,
        &mut handle,
        &initial,
        &answer_tx,
        &mut answer_rx,
    )
    .await;
    runtime.answers.lock().await.remove(&node_id);
    match outcome {
        Ok(()) => runtime.unmark_live(&node_id).await,
        Err(error) => {
            tracing::warn!(%node_id, %task_id, %error, "MCP task driver ended with error");
            // P3: never strand a durable node non-terminal. `Waiting → Failed`
            // is illegal, so the finalizer uses the sanctioned
            // `Waiting → Running → Failed` bridge.
            match finalize_failed(&runtime, &node_id).await {
                Ok(()) => runtime.unmark_live(&node_id).await,
                Err(finalize_error) => {
                    tracing::error!(
                        %node_id, %finalize_error,
                        "MCP task node left non-terminal after driver error; retained for teardown reap"
                    );
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_driver_inner(
    runtime: &Arc<McpTaskRuntime>,
    node_id: &AgentId,
    server_id: &str,
    task_id: &str,
    transport: &dyn McpTaskTransport,
    owner_cancel: &CancellationToken,
    node_cancel: &CancellationToken,
    handle: &mut crate::domain::ports::TaskNodeHandle,
    initial: &CreateTaskReply,
    answer_tx: &tokio::sync::mpsc::Sender<InputAnswer>,
    answer_rx: &mut tokio::sync::mpsc::Receiver<InputAnswer>,
) -> Result<(), McpError> {
    // 1. Materialize: Created -> Running, then journal the task binding.
    drive_state(runtime, node_id, NodeState::Running).await?;
    if let Err(error) = runtime
        .room
        .record_event(RoomEvent::McpTaskBound {
            node: node_id.clone(),
            server: server_id.to_string(),
            task: task_id.to_string(),
        })
        .await
    {
        tracing::error!(%node_id, %error, "failed to journal McpTaskBound; continuing with durable node only");
    }
    if let Some(terminal) = terminal_node_state_for(&initial.task.status) {
        drive_state(runtime, node_id, terminal).await?;
        return Ok(());
    }

    // 2. Poll loop. Tickets are keyed by the server's durable input-request
    // key; this preserves the exact correlation data in the artifact and lets
    // re-issued requests reuse their already-journaled ticket.
    let interval = initial
        .task
        .poll_interval_ms
        .map(Duration::from_millis)
        .filter(|d| !d.is_zero())
        .unwrap_or(runtime.poll.interval);
    let deadline = tokio::time::Instant::now() + runtime.poll.deadline;
    let mut status_updates = 0usize;
    let mut last_status = initial.task.status.clone();
    let mut currently_waiting = false;
    let mut outstanding_requests = BTreeMap::<String, InputRequest>::new();
    let mut active_tickets = BTreeMap::<String, ArtifactId>::new();
    let mut pending_answered = BTreeSet::<String>::new();

    loop {
        // A remote update ack is truth, but ticket resolution is durable local
        // state. Retry that local write before leaving Waiting so a transient
        // journal failure cannot create a Running node with an open answered
        // ticket.
        if !pending_answered.is_empty() {
            match resolve_ticket_keys(
                runtime,
                node_id,
                &mut active_tickets,
                &pending_answered,
                TicketResolution::Answered,
            )
            .await
            {
                Ok(()) => {
                    runtime.answers.lock().await.remove(node_id);
                    drive_state(runtime, node_id, NodeState::Running).await?;
                    currently_waiting = false;
                    outstanding_requests.clear();
                    pending_answered.clear();
                }
                Err(error) => {
                    tracing::warn!(
                        %node_id, %task_id, %error,
                        "tasks/update was acknowledged but ticket resolution is not durable; retrying"
                    );
                }
            }
        }

        // P5: the total deadline is independent of an untrusted poll interval.
        let now = tokio::time::Instant::now();
        if now >= deadline {
            let error = McpError::TaskFailed(format!(
                "task {task_id} exceeded the poll deadline ({:?})",
                runtime.poll.deadline
            ));
            fail_live_task(
                runtime,
                node_id,
                &mut active_tickets,
                currently_waiting,
                TicketResolution::Failed {
                    reason: error.to_string(),
                },
            )
            .await?;
            return Err(error);
        }
        let sleep_for = interval.min(deadline - now);

        tokio::select! {
            biased;
            _ = owner_cancel.cancelled() => {
                return finish_cancel(
                    runtime, node_id, task_id, transport, "owner cancelled",
                    &mut active_tickets, currently_waiting,
                ).await;
            }
            _ = node_cancel.cancelled() => {
                return finish_cancel(
                    runtime, node_id, task_id, transport, "teardown",
                    &mut active_tickets, currently_waiting,
                ).await;
            }
            _ = handle.cancel_token.cancelled() => {
                return finish_cancel(
                    runtime, node_id, task_id, transport, "node kill",
                    &mut active_tickets, currently_waiting,
                ).await;
            }
            op = handle.command_rx.recv() => {
                match op {
                    Some(Op::Kill) => {
                        return finish_cancel(
                            runtime, node_id, task_id, transport, "Op::Kill",
                            &mut active_tickets, currently_waiting,
                        ).await;
                    }
                    Some(_) => continue,
                    None => {
                        return finish_cancel(
                            runtime, node_id, task_id, transport, "command channel closed",
                            &mut active_tickets, currently_waiting,
                        ).await;
                    }
                }
            }
            answer = answer_rx.recv(), if currently_waiting && pending_answered.is_empty() => {
                if let Some(answer) = answer {
                    match handle_answer(
                        runtime,
                        task_id,
                        transport,
                        &outstanding_requests,
                        answer,
                    )
                    .await
                    {
                        Ok(AnswerOutcome::Accepted { keys }) => {
                            pending_answered.extend(keys);
                        }
                        Ok(AnswerOutcome::Refused { reason }) => {
                            tracing::warn!(%node_id, %task_id, %reason, "input answer refused; node stays Waiting");
                        }
                        Err(error) => {
                            tracing::warn!(%node_id, %task_id, %error, "tasks/update failed; node stays Waiting");
                        }
                    }
                }
            }
            _ = tokio::time::sleep(sleep_for) => {
                let outcome = match poll_once(transport, task_id, runtime.poll.request_timeout).await {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        tracing::warn!(%node_id, %task_id, %error, "MCP task poll failed; driving node Failed");
                        fail_live_task(
                            runtime,
                            node_id,
                            &mut active_tickets,
                            currently_waiting,
                            TicketResolution::Failed {
                                reason: error.to_string(),
                            },
                        )
                        .await?;
                        return Err(error);
                    }
                };
                match outcome {
                    PollOutcome::Continue { status, input_requests } => {
                        if status == TaskStatus::InputRequired {
                            // ⚠️ DO NOT REUSE THE APPROVAL GATE
                            // (ADR-17-5-02 D1). Persist every ticket before the
                            // visible Waiting transition: no ticket, no wait.
                            let withdrawn: BTreeSet<String> = active_tickets
                                .keys()
                                .filter(|key| !input_requests.contains_key(*key))
                                .cloned()
                                .collect();
                            if !withdrawn.is_empty() {
                                resolve_ticket_keys(
                                    runtime,
                                    node_id,
                                    &mut active_tickets,
                                    &withdrawn,
                                    TicketResolution::Failed {
                                        reason: "server withdrew the input request before it was answered".into(),
                                    },
                                )
                                .await?;
                            }
                            outstanding_requests = input_requests;
                            if let Err(error) = ensure_input_request_artifacts(
                                runtime,
                                node_id,
                                &outstanding_requests,
                                &mut active_tickets,
                            )
                            .await
                            {
                                let reason = error.to_string();
                                fail_live_task(
                                    runtime,
                                    node_id,
                                    &mut active_tickets,
                                    currently_waiting,
                                    TicketResolution::Failed {
                                        reason: reason.clone(),
                                    },
                                )
                                .await?;
                                return Err(error);
                            }
                            if !currently_waiting {
                                runtime
                                    .answers
                                    .lock()
                                    .await
                                    .insert(node_id.clone(), answer_tx.clone());
                                drive_state(runtime, node_id, NodeState::Waiting).await?;
                                if let Err(error) = runtime
                                    .nodes
                                    .stamp_wait_reason(node_id, Some(WaitReason::AwaitingHumanInput))
                                    .await
                                {
                                    let error = McpError::TaskProtocol(format!(
                                        "durable wait_reason stamp: {error}"
                                    ));
                                    fail_live_task(
                                        runtime,
                                        node_id,
                                        &mut active_tickets,
                                        true,
                                        TicketResolution::Failed {
                                            reason: error.to_string(),
                                        },
                                    )
                                    .await?;
                                    return Err(error);
                                }
                                currently_waiting = true;
                            }
                        } else if currently_waiting {
                            // The server resumed itself without an acknowledged
                            // human answer. Close the stale tickets explicitly.
                            runtime.answers.lock().await.remove(node_id);
                            resolve_all_tickets(
                                runtime,
                                node_id,
                                &mut active_tickets,
                                TicketResolution::Failed {
                                    reason: "server left input_required without an acknowledged answer".into(),
                                },
                            )
                            .await?;
                            drive_state(runtime, node_id, NodeState::Running).await?;
                            currently_waiting = false;
                            outstanding_requests.clear();
                        }
                        if status != last_status {
                            status_updates += 1;
                            if status_updates > runtime.poll.max_status_updates {
                                let error = McpError::TaskFailed(format!(
                                    "task {task_id} exceeded the status-update cap"
                                ));
                                fail_live_task(
                                    runtime,
                                    node_id,
                                    &mut active_tickets,
                                    currently_waiting,
                                    TicketResolution::Failed {
                                        reason: error.to_string(),
                                    },
                                )
                                .await?;
                                return Err(error);
                            }
                            last_status = status;
                        }
                    }
                    PollOutcome::Terminal {
                        mut state,
                        resolution,
                    } => {
                        runtime.answers.lock().await.remove(node_id);
                        let resolution = if state == NodeState::Completed
                            && !active_tickets.is_empty()
                        {
                            state = NodeState::Failed;
                            TicketResolution::Failed {
                                reason: "remote task completed with unanswered input requests".into(),
                            }
                        } else {
                            resolution
                        };
                        resolve_all_tickets(
                            runtime,
                            node_id,
                            &mut active_tickets,
                            resolution,
                        )
                        .await?;
                        if currently_waiting && state != NodeState::Cancelled {
                            drive_state(runtime, node_id, NodeState::Running).await?;
                        }
                        drive_state(runtime, node_id, state).await?;
                        return Ok(());
                    }
                }
            }
        }
    }
}

enum PollOutcome {
    Continue {
        status: TaskStatus,
        input_requests: BTreeMap<String, InputRequest>,
    },
    Terminal {
        state: NodeState,
        resolution: TicketResolution,
    },
}

/// One `tasks/get` round-trip with the driver's own request bound.
async fn poll_once(
    transport: &dyn McpTaskTransport,
    task_id: &str,
    request_timeout: Duration,
) -> Result<PollOutcome, McpError> {
    let reply = match tokio::time::timeout(request_timeout, transport.tasks_get(task_id)).await {
        Ok(Ok(reply)) => reply,
        Ok(Err(error)) => return Err(error),
        Err(_) => {
            return Err(McpError::TaskProtocol(format!(
                "tasks/get for {task_id} exceeded the request timeout ({request_timeout:?})"
            )));
        }
    };
    if reply.task.task_id != task_id {
        return Err(McpError::TaskProtocol(format!(
            "tasks/get for {task_id} returned a mismatched taskId {:?}",
            reply.task.task_id
        )));
    }
    match terminal_node_state_for(&reply.task.status) {
        Some(NodeState::Completed) => {
            let is_error = reply
                .result
                .as_ref()
                .and_then(|r| r.get("isError"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            if is_error {
                tracing::info!(%task_id, "MCP task completed with isError:true (tool-level failure)");
            }
            Ok(PollOutcome::Terminal {
                state: NodeState::Completed,
                resolution: TicketResolution::Failed {
                    reason: "remote task completed while a ticket was still open".into(),
                },
            })
        }
        Some(state) => {
            // R-11: do not calculate or enforce ttlMs locally. Expiry is
            // classified only from the server's terminal reply.
            let reason = reply
                .task
                .status_message
                .clone()
                .or_else(|| reply.error.as_ref().map(|error| error.message.clone()))
                .unwrap_or_else(|| format!("remote task entered {state:?}"));
            let expired = reason.to_ascii_lowercase().contains("expir")
                || reply
                    .error
                    .as_ref()
                    .and_then(|error| error.data.as_ref())
                    .and_then(|data| data.get("reason"))
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|reason| reason == "expired");
            Ok(PollOutcome::Terminal {
                state,
                resolution: if expired {
                    TicketResolution::ExpiredUnanswered { reason }
                } else {
                    TicketResolution::Failed { reason }
                },
            })
        }
        None => {
            let input_requests = reply.input_requests.unwrap_or_default();
            if reply.task.status == TaskStatus::InputRequired && input_requests.is_empty() {
                return Err(McpError::TaskProtocol(format!(
                    "tasks/get for {task_id} returned input_required without inputRequests"
                )));
            }
            Ok(PollOutcome::Continue {
                status: reply.task.status,
                input_requests,
            })
        }
    }
}

/// Cooperative cancel (R-15): issue a real `tasks/cancel`, drive the local
/// node cascade ON THE ACK, never on an observed `cancelled` status. A
/// server that acks and never transitions still terminates here.
async fn cooperative_cancel(
    runtime: &Arc<McpTaskRuntime>,
    node_id: &AgentId,
    task_id: &str,
    transport: &dyn McpTaskTransport,
    reason: &str,
) -> Result<(), McpError> {
    tracing::info!(%node_id, %task_id, %reason, "cancelling MCP task");
    match tokio::time::timeout(
        runtime.poll.request_timeout,
        transport.tasks_cancel(task_id),
    )
    .await
    {
        // AC5 / D6: only the real ack may gate `Cancelled`.
        Ok(Ok(_ack)) => Ok(()),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(McpError::TaskProtocol(format!(
            "tasks/cancel for {task_id} exceeded the request timeout ({:?})",
            runtime.poll.request_timeout
        ))),
    }
}

async fn finish_cancel(
    runtime: &Arc<McpTaskRuntime>,
    node_id: &AgentId,
    task_id: &str,
    transport: &dyn McpTaskTransport,
    reason: &str,
    active_tickets: &mut BTreeMap<String, ArtifactId>,
    currently_waiting: bool,
) -> Result<(), McpError> {
    runtime.answers.lock().await.remove(node_id);
    match cooperative_cancel(runtime, node_id, task_id, transport, reason).await {
        Ok(()) => {
            resolve_all_tickets(
                runtime,
                node_id,
                active_tickets,
                TicketResolution::Cancelled,
            )
            .await?;
            drive_state(runtime, node_id, NodeState::Cancelled).await
        }
        Err(error) => {
            let resolution_result = resolve_all_tickets(
                runtime,
                node_id,
                active_tickets,
                TicketResolution::CancelUnconfirmed {
                    reason: error.to_string(),
                },
            )
            .await;
            if currently_waiting {
                drive_state(runtime, node_id, NodeState::Running).await?;
            }
            drive_state(runtime, node_id, NodeState::Failed).await?;
            resolution_result?;
            Err(error)
        }
    }
}

async fn fail_live_task(
    runtime: &Arc<McpTaskRuntime>,
    node_id: &AgentId,
    active_tickets: &mut BTreeMap<String, ArtifactId>,
    currently_waiting: bool,
    resolution: TicketResolution,
) -> Result<(), McpError> {
    runtime.answers.lock().await.remove(node_id);
    let resolution_result = resolve_all_tickets(runtime, node_id, active_tickets, resolution).await;
    if currently_waiting {
        drive_state(runtime, node_id, NodeState::Running).await?;
    }
    drive_state(runtime, node_id, NodeState::Failed).await?;
    resolution_result
}

async fn finalize_failed(runtime: &Arc<McpTaskRuntime>, node_id: &AgentId) -> Result<(), McpError> {
    match runtime
        .nodes
        .try_set_state(node_id, NodeState::Failed)
        .await
    {
        Ok(()) | Err(TaskNodesError::NotFound(_)) => Ok(()),
        Err(TaskNodesError::InvalidTransition {
            from: NodeState::Waiting,
            ..
        }) => {
            drive_state(runtime, node_id, NodeState::Running).await?;
            drive_state(runtime, node_id, NodeState::Failed).await
        }
        Err(TaskNodesError::InvalidTransition { from, .. }) if from.is_terminal() => Ok(()),
        Err(error) => Err(McpError::TaskProtocol(format!(
            "node {node_id} failed-state finalization: {error}"
        ))),
    }
}

/// Loud state transition through the domain seam (R-6). Idempotent terminal
/// re-drives are tolerated as `Ok` — a kill racing a natural completion must
/// not turn into a spurious error; every other failure propagates.
async fn drive_state(
    runtime: &Arc<McpTaskRuntime>,
    node_id: &AgentId,
    target: NodeState,
) -> Result<(), McpError> {
    match runtime.nodes.try_set_state(node_id, target).await {
        Ok(()) => Ok(()),
        Err(TaskNodesError::InvalidTransition { from, to }) if from == to => Ok(()),
        Err(error) => Err(McpError::TaskProtocol(format!(
            "node {node_id} state drive to {target:?}: {error}"
        ))),
    }
}

/// Outcome of applying a submitted answer.
#[derive(Debug)]
enum AnswerOutcome {
    /// The answer correlated, validated, and received a real tasks/update ack.
    /// Ticket resolution and the Waiting→Running transition follow durably in
    /// the driver loop.
    Accepted { keys: BTreeSet<String> },
    /// The answer was refused before forwarding. The node stays Waiting.
    Refused { reason: String },
}

/// Correlate + validate + forward an operator's answer (R-6, D4, D6).
///
/// - **Correlate (R-6):** every key in `answer.responses` must be currently
///   outstanding. Refuse stale/unknown/mismatched keys — never forward them.
/// - **Validate (D4):** each response's `content` must structurally satisfy the
///   request's `requestedSchema` (D5 minimum set). Refuse on mismatch.
/// - **Forward (D6):** `tasks/update` carries the answer verbatim; the node
///   resumes to `Running` and the next poll observes the server's terminal.
async fn handle_answer(
    runtime: &Arc<McpTaskRuntime>,
    task_id: &str,
    transport: &dyn McpTaskTransport,
    outstanding: &BTreeMap<String, InputRequest>,
    answer: InputAnswer,
) -> Result<AnswerOutcome, McpError> {
    if answer.responses.is_empty() {
        return Ok(AnswerOutcome::Refused {
            reason: "inputResponses must contain at least one outstanding key".into(),
        });
    }
    for key in answer.responses.keys() {
        if !outstanding.contains_key(key) {
            return Ok(AnswerOutcome::Refused {
                reason: format!("answer key {key:?} is not currently outstanding"),
            });
        }
    }
    for (key, response) in &answer.responses {
        let Some(request) = outstanding.get(key) else {
            continue;
        };
        if let Some(schema) = request.params.get("requestedSchema") {
            if response.action == "accept"
                && response.content.is_none()
                && schema_requires_content(schema)
            {
                return Ok(AnswerOutcome::Refused {
                    reason: format!(
                        "answer for key {key:?} omits content required by requestedSchema"
                    ),
                });
            }
            if let Some(content) = response.content.as_ref()
                && !validate_against_schema(content, schema)
            {
                return Ok(AnswerOutcome::Refused {
                    reason: format!("answer for key {key:?} does not satisfy requestedSchema"),
                });
            }
        }
    }

    let keys = answer.responses.keys().cloned().collect();
    match tokio::time::timeout(
        runtime.poll.request_timeout,
        transport.tasks_update(task_id, answer.responses),
    )
    .await
    {
        Ok(Ok(_ack)) => Ok(AnswerOutcome::Accepted { keys }),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(McpError::TaskProtocol(format!(
            "tasks/update for {task_id} exceeded the request timeout ({:?})",
            runtime.poll.request_timeout
        ))),
    }
}

/// Whether omission of `content` would necessarily violate this schema.
fn schema_requires_content(schema: &serde_json::Value) -> bool {
    schema
        .get("required")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|required| !required.is_empty())
}

/// Minimal structural validator for the D5 renderable set
/// (boolean/string/number/integer/enum). Conservative for unsupported shapes,
/// strict for constraints we claim to render locally.
fn validate_against_schema(content: &serde_json::Value, schema: &serde_json::Value) -> bool {
    let Some(schema_obj) = schema.as_object() else {
        return true;
    };
    // Only object-topped schemas are validated structurally (the captured
    // common case). A non-object content for an object schema is a mismatch.
    let is_object_schema = schema_obj
        .get("type")
        .and_then(serde_json::Value::as_str)
        .map(|t| t == "object")
        .unwrap_or(true);
    if !is_object_schema {
        return true;
    }
    let Some(content_obj) = content.as_object() else {
        return false;
    };
    let Some(properties) = schema_obj.get("properties").and_then(|p| p.as_object()) else {
        return true;
    };
    // Check every required field is present and type-matches its property.
    if let Some(required_arr) = schema_obj.get("required").and_then(|r| r.as_array()) {
        for req in required_arr {
            let Some(field) = req.as_str() else { continue };
            let Some(value) = content_obj.get(field) else {
                return false;
            };
            if let Some(prop_schema) = properties.get(field)
                && !value_matches_type(value, prop_schema)
            {
                return false;
            }
        }
    }
    // Check every present field type-matches (catch a boolean field sent as string).
    for (field, value) in content_obj {
        if let Some(prop_schema) = properties.get(field)
            && !value_matches_type(value, prop_schema)
        {
            return false;
        }
    }
    true
}

/// D5 type check for a single property value against its property schema.
fn value_matches_type(value: &serde_json::Value, prop_schema: &serde_json::Value) -> bool {
    let Some(prop_obj) = prop_schema.as_object() else {
        return true;
    };
    if let Some(allowed) = prop_obj.get("enum").and_then(serde_json::Value::as_array)
        && !allowed.contains(value)
    {
        return false;
    }
    match prop_obj.get("type").and_then(serde_json::Value::as_str) {
        Some("boolean") => value.is_boolean(),
        Some("string") => value.is_string(),
        Some("integer") => value.is_i64() || value.is_u64(),
        Some("number") => value.is_number(),
        Some(_) | None => true,
    }
}

/// Write one `InputRequest` artifact + `TicketAssigned` event per outstanding
/// request (AC3). The artifact body is the raw elicitation envelope so the
/// human sees exactly what was asked. A missing sink is a LOUD error (AC3 is
/// load-bearing on the artifact); the node still reaches `Waiting`.
async fn ensure_input_request_artifacts(
    runtime: &Arc<McpTaskRuntime>,
    node_id: &AgentId,
    outstanding: &BTreeMap<String, InputRequest>,
    active_tickets: &mut BTreeMap<String, ArtifactId>,
) -> Result<(), McpError> {
    let sink = runtime
        .artifact
        .lock()
        .expect("artifact sink lock poisoned")
        .clone()
        .ok_or_else(|| {
            McpError::TaskProtocol(
                "ArtifactSink not wired; refusing to park an MCP task without a durable ticket"
                    .into(),
            )
        })?;

    for (key, request) in outstanding {
        if active_tickets.contains_key(key) {
            continue;
        }
        // The keyed envelope is the durable answer contract. Persisting only
        // `request` would discard the correlation key required by
        // `inputResponses`, making a replayed/headless ticket unanswerable.
        let body = serde_json::json!({
            "key": key,
            "request": request,
        });
        let id = sink
            .write_input_request(node_id, node_id, body)
            .await
            .map_err(|error| McpError::TaskProtocol(error.to_string()))?;
        tracing::info!(%node_id, request_key = %key, artifact = %id, "input-request ticket journaled");
        active_tickets.insert(key.clone(), id);
    }
    Ok(())
}

async fn resolve_ticket_keys(
    runtime: &Arc<McpTaskRuntime>,
    node_id: &AgentId,
    active_tickets: &mut BTreeMap<String, ArtifactId>,
    keys: &BTreeSet<String>,
    outcome: TicketResolution,
) -> Result<(), McpError> {
    let tickets: Vec<(String, ArtifactId)> = keys
        .iter()
        .filter_map(|key| {
            active_tickets
                .get(key)
                .cloned()
                .map(|artifact| (key.clone(), artifact))
        })
        .collect();
    for (key, artifact) in tickets {
        runtime
            .room
            .record_event(RoomEvent::TicketResolved {
                node: node_id.clone(),
                artifact,
                outcome: outcome.clone(),
            })
            .await
            .map_err(|error| {
                McpError::TaskProtocol(format!("ticket resolution journal failed: {error}"))
            })?;
        active_tickets.remove(&key);
    }
    Ok(())
}

async fn resolve_all_tickets(
    runtime: &Arc<McpTaskRuntime>,
    node_id: &AgentId,
    active_tickets: &mut BTreeMap<String, ArtifactId>,
    outcome: TicketResolution,
) -> Result<(), McpError> {
    let keys = active_tickets.keys().cloned().collect();
    resolve_ticket_keys(runtime, node_id, active_tickets, &keys, outcome).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_node_id_round_trips_untrusted_server_and_task_values() {
        for (server, task) in [
            ("postgres", "task-1"),
            ("org/server", "task/with/slashes"),
            ("srv", ".."),
            ("srv", "task\u{0}nul"),
            ("srv", &"x".repeat(4096)),
        ] {
            let id = mint_mcp_node_id(server, task);
            assert_eq!(
                parse_mcp_node_id(&id)
                    .as_ref()
                    .map(|(server, task)| (server.as_str(), task.as_str())),
                Some((server, task)),
                "round-trip failed for ({server:?}, {task:?})"
            );
        }
        // No panics, no embedded separators in the minted id's raw segments.
        let id = mint_mcp_node_id("org/server", "task/with/slashes");
        let s = id.as_str();
        assert!(s.starts_with("mcp/s-"));
        assert_eq!(
            s.matches('/').count(),
            2,
            "exactly the structural separators"
        );
    }

    #[test]
    fn parse_rejects_non_mcp_and_malformed_ids() {
        assert!(parse_mcp_node_id(&AgentId::from_validated("subagent/x")).is_none());
        assert!(parse_mcp_node_id(&AgentId::from_validated("a2a/p-abc/t-def")).is_none());
        assert!(parse_mcp_node_id(&AgentId::from_validated("mcp/s-!!!/t-def")).is_none());
    }

    #[test]
    fn input_required_is_non_terminal_the_poll_loop_owns_the_waiting_edge() {
        assert_eq!(terminal_node_state_for(&TaskStatus::InputRequired), None);
        assert_eq!(terminal_node_state_for(&TaskStatus::Working), None);
        assert_eq!(
            terminal_node_state_for(&TaskStatus::Completed),
            Some(NodeState::Completed)
        );
        assert_eq!(
            terminal_node_state_for(&TaskStatus::Failed),
            Some(NodeState::Failed)
        );
        assert_eq!(
            terminal_node_state_for(&TaskStatus::Cancelled),
            Some(NodeState::Cancelled)
        );
    }

    // ---- Driver keystones: scripted transport doubles + a REAL NodeTree
    // (mirrors a2a/driver.rs tests — no mocked domain logic) ----

    use parking_lot::Mutex as StdMutex;
    use std::collections::VecDeque;

    use crate::domain::events::AppEvent;
    use crate::domain::models::{JournalRecord, OwnershipKind};
    use crate::infrastructure::subagent::{NodeJournal, NodeRoomJournal, NodeTree};

    use super::super::tasks::{McpTask, TaskAck, TaskGetReply};

    /// Scripted `McpTaskTransport`: replays a queue of `tasks/get` replies and
    /// records every `tasks/cancel`. The ack is ALWAYS `{"resultType":
    /// "complete"}` — the ack-then-never-cancel mutant is the default script
    /// (the server never reports `cancelled`), so the cancel keystones prove
    /// R-15 (cascade on the ack).
    struct ScriptedTransport {
        replies: StdMutex<VecDeque<Result<TaskGetReply, McpError>>>,
        cancels: StdMutex<Vec<String>>,
        // 17.5b: recorded `tasks/update` calls — (task_id, sorted "key=action" pairs)
        // so keystones can assert which keys were forwarded to the server.
        updates: StdMutex<Vec<(String, Vec<(String, String)>)>>,
        cancel_ack: bool,
    }

    impl ScriptedTransport {
        fn new(statuses: &[&str]) -> Arc<Self> {
            let replies = statuses
                .iter()
                .map(|s| Ok(get_reply(s)))
                .collect::<VecDeque<_>>();
            Arc::new(Self {
                replies: StdMutex::new(replies),
                cancels: StdMutex::new(Vec::new()),
                updates: StdMutex::new(Vec::new()),
                cancel_ack: true,
            })
        }
        fn repeat_forever(status: &str) -> Arc<Self> {
            // A long deque stands in for "forever" at fast poll intervals.
            let replies = std::iter::repeat_with(|| Ok(get_reply(status)))
                .take(10_000)
                .collect();
            Arc::new(Self {
                replies: StdMutex::new(replies),
                cancels: StdMutex::new(Vec::new()),
                updates: StdMutex::new(Vec::new()),
                cancel_ack: true,
            })
        }
    }

    #[async_trait::async_trait]
    impl McpTaskTransport for ScriptedTransport {
        async fn tasks_get(&self, _task_id: &str) -> Result<TaskGetReply, McpError> {
            self.replies.lock().pop_front().expect("script exhausted")
        }
        async fn tasks_cancel(&self, task_id: &str) -> Result<TaskAck, McpError> {
            self.cancels.lock().push(task_id.to_owned());
            if !self.cancel_ack {
                return Err(McpError::TaskProtocol(
                    "scripted tasks/cancel rejection".into(),
                ));
            }
            Ok(TaskAck {
                result_type: Some("complete".into()),
            })
        }
        async fn tasks_update(
            &self,
            task_id: &str,
            responses: std::collections::BTreeMap<
                String,
                crate::adapters::mcp::tasks::InputResponse,
            >,
        ) -> Result<TaskAck, McpError> {
            let forwarded: Vec<(String, String)> =
                responses.into_iter().map(|(k, v)| (k, v.action)).collect();
            self.updates.lock().push((task_id.to_owned(), forwarded));
            Ok(TaskAck {
                result_type: Some("complete".into()),
            })
        }
    }

    struct HangingUpdateTransport;

    #[async_trait::async_trait]
    impl McpTaskTransport for HangingUpdateTransport {
        async fn tasks_get(&self, _task_id: &str) -> Result<TaskGetReply, McpError> {
            unreachable!("timeout keystone calls only tasks_update")
        }

        async fn tasks_cancel(&self, _task_id: &str) -> Result<TaskAck, McpError> {
            unreachable!("timeout keystone calls only tasks_update")
        }

        async fn tasks_update(
            &self,
            _task_id: &str,
            _responses: BTreeMap<String, InputResponse>,
        ) -> Result<TaskAck, McpError> {
            std::future::pending().await
        }
    }

    fn get_reply(status: &str) -> TaskGetReply {
        let input_required = status == "input_required";
        let (status, extra_result, extra_error) = match status {
            "working" => (TaskStatus::Working, None, None),
            "input_required" => (TaskStatus::InputRequired, None, None),
            "completed" => (
                TaskStatus::Completed,
                Some(serde_json::json!({"content": [{"type": "text", "text": "done"}]})),
                None,
            ),
            "completed_iserror" => (
                TaskStatus::Completed,
                Some(serde_json::json!({
                    "content": [{"type": "text", "text": "tool failed"}],
                    "isError": true
                })),
                None,
            ),
            "failed" => (
                TaskStatus::Failed,
                None,
                Some(super::super::tasks::TaskError {
                    code: -32603,
                    message: "job failed".into(),
                    data: None,
                }),
            ),
            "cancelled" => (TaskStatus::Cancelled, None, None),
            other => panic!("unknown scripted status {other}"),
        };
        TaskGetReply {
            result_type: Some("complete".into()),
            task: McpTask {
                task_id: "task-1".into(),
                status,
                status_message: None,
                created_at: "2026-07-19T00:00:00Z".into(),
                last_updated_at: "2026-07-19T00:00:01Z".into(),
                ttl_ms: Some(300_000),
                poll_interval_ms: Some(5),
            },
            result: extra_result,
            error: extra_error,
            input_requests: input_required.then(|| {
                BTreeMap::from([(
                    "confirm".into(),
                    InputRequest {
                        method: "elicitation/create".into(),
                        params: serde_json::json!({
                            "requestedSchema": {
                                "type": "object",
                                "properties": {
                                    "confirm": { "type": "boolean" }
                                },
                                "required": ["confirm"]
                            }
                        }),
                    },
                )])
            }),
        }
    }

    fn input_reply_with_keys(keys: &[&str]) -> TaskGetReply {
        let mut reply = get_reply("input_required");
        reply.input_requests = Some(
            keys.iter()
                .map(|key| {
                    (
                        (*key).to_string(),
                        InputRequest {
                            method: "elicitation/create".into(),
                            params: serde_json::json!({}),
                        },
                    )
                })
                .collect(),
        );
        reply
    }

    fn create_reply() -> CreateTaskReply {
        CreateTaskReply {
            result_type: "task".into(),
            task: get_reply("working").task,
        }
    }

    struct Fixture {
        runtime: Arc<McpTaskRuntime>,
        tree: Arc<NodeTree>,
        journal: Arc<NodeJournal>,
        rx: mpsc::UnboundedReceiver<AppEvent>,
        _dir: tempfile::TempDir,
    }

    struct FailingArtifactSink;

    #[async_trait::async_trait]
    impl ArtifactSink for FailingArtifactSink {
        async fn write_input_request(
            &self,
            _producer: &AgentId,
            _node: &AgentId,
            _body: serde_json::Value,
        ) -> Result<ArtifactId, crate::domain::ports::ArtifactSinkError> {
            Err(crate::domain::ports::ArtifactSinkError::Write(
                "injected artifact persistence failure".into(),
            ))
        }
    }

    async fn fixture() -> Fixture {
        fixture_with_poll(PollConfig::fast_config()).await
    }

    async fn fixture_with_poll(poll: PollConfig) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(
            NodeJournal::open_workspace(dir.path())
                .await
                .expect("journal opens"),
        );
        let (tx, rx) = mpsc::unbounded_channel();
        let tree = Arc::new(
            NodeTree::with_event_tx(tx.clone(), std::sync::Arc::new(|| 0i64))
                .with_journal(journal.clone()),
        );
        let room = Arc::new(NodeRoomJournal::new(journal.clone(), Some(tx)));
        let clock: Arc<dyn Clock> = Arc::new(crate::domain::clock::SystemClock::default());
        let runtime = Arc::new(
            McpTaskRuntime::new(tree.clone(), tree.clone(), room.clone(), clock)
                .with_poll_config(poll),
        );
        let store: Arc<dyn crate::domain::ports::ArtifactStore> = Arc::new(
            crate::adapters::artifact::FileSystemArtifactStore::new(dir.path()),
        );
        let authority = crate::domain::models::CapabilityToken::r1_root(AgentId::root());
        runtime.set_artifact_sink(Arc::new(
            crate::infrastructure::subagent::JournalArtifactSink::new(
                store,
                room,
                authority.id,
                crate::domain::models::HostBinding::new(
                    "local",
                    format!("workspace:{}", dir.path().display()),
                ),
            ),
        ));
        Fixture {
            runtime,
            tree,
            journal,
            rx,
            _dir: dir,
        }
    }

    async fn wait_node_state(tree: &NodeTree, node_id: &AgentId, want: NodeState) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(entry) = tree
                    .list()
                    .await
                    .into_iter()
                    .find(|e| &e.agent_id == node_id)
                    && entry.current_status == want
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("node {node_id} reached {want:?}"));
    }

    fn node_id() -> AgentId {
        mint_mcp_node_id("srv", "task-1")
    }

    /// AC1 keystone: a task materializes as a durable first-class node and
    /// reaches Completed; every state change is journaled (AC6) and the
    /// binding event lands in the room journal.
    #[tokio::test]
    async fn task_materializes_as_durable_node_and_completes() {
        let mut fx = fixture().await;
        let transport = ScriptedTransport::new(&["working", "working", "completed"]);
        let result = fx
            .runtime
            .start_task("srv", create_reply(), transport, CancellationToken::new())
            .await
            .expect("start_task");
        assert!(!result.is_error);
        assert!(result.content.contains("task-1"));

        let id = node_id();
        wait_node_state(&fx.tree, &id, NodeState::Completed).await;
        let entry = fx
            .tree
            .list()
            .await
            .into_iter()
            .find(|e| e.agent_id == id)
            .expect("node present");
        assert_eq!(entry.ownership, OwnershipKind::Peer);
        assert_eq!(entry.subagent_type, "mcp-task");

        // AC6: journal holds the registration, the binding, and the terminal
        // checkpoint; the bus saw the room events (durable-first).
        let records = fx.journal.load().await.unwrap();
        let saw_bound = records.iter().any(|e| {
            matches!(
                &e.record,
                JournalRecord::Room(RoomEvent::McpTaskBound { server, task, .. })
                    if server == "srv" && task == "task-1"
            )
        });
        assert!(saw_bound, "McpTaskBound must be journaled");
        let saw_completed_checkpoint = records.iter().any(|e| {
            matches!(
                &e.record,
                JournalRecord::Checkpoint(cp) if cp.id == id && cp.state == NodeState::Completed
            )
        });
        assert!(
            saw_completed_checkpoint,
            "terminal checkpoint must be journaled"
        );
        let mut saw_state_changed = false;
        while let Ok(event) = fx.rx.try_recv() {
            if let AppEvent::DomainEvent(payload) = event
                && format!("{payload:?}").contains("NodeStateChanged")
            {
                saw_state_changed = true;
            }
        }
        assert!(saw_state_changed, "NodeStateChanged must reach the bus");
    }

    /// R-14 keystone: `isError:true` completes — never Failed.
    #[tokio::test]
    async fn iserror_true_maps_to_completed_not_failed() {
        let fx = fixture().await;
        let transport = ScriptedTransport::new(&["working", "completed_iserror"]);
        fx.runtime
            .start_task("srv", create_reply(), transport, CancellationToken::new())
            .await
            .unwrap();
        wait_node_state(&fx.tree, &node_id(), NodeState::Completed).await;
    }

    /// R-14 keystone (other half): a protocol-level failure maps to Failed.
    #[tokio::test]
    async fn wire_failed_maps_to_node_failed() {
        let fx = fixture().await;
        let transport = ScriptedTransport::new(&["working", "failed"]);
        fx.runtime
            .start_task("srv", create_reply(), transport, CancellationToken::new())
            .await
            .unwrap();
        wait_node_state(&fx.tree, &node_id(), NodeState::Failed).await;
    }

    /// AC4 keystone: owner cancel issues a real `tasks/cancel` and drives the
    /// local cascade ON THE ACK — the scripted server NEVER reports
    /// `cancelled`, yet the node reaches `Cancelled` (R-15).
    #[tokio::test]
    async fn cancel_drives_cascade_on_ack_not_observed_status() {
        let fx = fixture().await;
        let transport = ScriptedTransport::repeat_forever("working");
        let owner = CancellationToken::new();
        fx.runtime
            .start_task("srv", create_reply(), transport.clone(), owner.clone())
            .await
            .unwrap();
        wait_node_state(&fx.tree, &node_id(), NodeState::Running).await;

        owner.cancel();
        wait_node_state(&fx.tree, &node_id(), NodeState::Cancelled).await;
        assert_eq!(transport.cancels.lock().as_slice(), ["task-1"]);

        // The Cancelled checkpoint is durable (AC6).
        let records = fx.journal.load().await.unwrap();
        assert!(records.iter().any(|e| matches!(
            &e.record,
            JournalRecord::Checkpoint(cp) if cp.id == node_id() && cp.state == NodeState::Cancelled
        )));
    }

    /// AC4 mutant (b): an external `cascade_kill` must reach the driver
    /// through `Op::Kill` and issue a real `tasks/cancel`. Disconnecting the
    /// kill signal strands the node and turns this RED.
    #[tokio::test]
    async fn cascade_kill_reaches_driver_and_cancels_remote_task() {
        let fx = fixture().await;
        let transport = ScriptedTransport::repeat_forever("working");
        fx.runtime
            .start_task(
                "srv",
                create_reply(),
                transport.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let id = node_id();
        wait_node_state(&fx.tree, &id, NodeState::Running).await;

        let killed = fx
            .tree
            .cascade_kill(&id, Duration::from_secs(2))
            .await
            .expect("cooperative kill succeeds");
        assert!(killed.contains(&id));
        assert_eq!(transport.cancels.lock().as_slice(), ["task-1"]);
    }

    /// AC5 keystone: session teardown (`kill_all_tasks`, as `disconnect`
    /// calls it) drives an in-flight task node terminal through the
    /// supervised cascade.
    #[tokio::test]
    async fn session_teardown_terminalizes_inflight_node() {
        let fx = fixture().await;
        let transport = ScriptedTransport::repeat_forever("working");
        fx.runtime
            .start_task(
                "srv",
                create_reply(),
                transport.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let id = node_id();
        wait_node_state(&fx.tree, &id, NodeState::Running).await;

        fx.runtime.kill_all_tasks().await;
        assert!(fx.runtime.live_tasks().await.is_empty());
        assert_eq!(transport.cancels.lock().as_slice(), ["task-1"]);
    }

    /// AC1 (17.5b): `input_required` now drives a REAL `Running → Waiting`
    /// transition (17.5a's "decode and log, never transition" is inverted).
    /// The node parks in `Waiting` — the durable, recoverable, hazard-eligible
    /// state — instead of silently continuing to poll.
    #[tokio::test]
    async fn input_required_drives_a_real_waiting_node() {
        let fx = fixture().await;
        let mut replies: std::collections::VecDeque<Result<TaskGetReply, McpError>> =
            std::collections::VecDeque::new();
        replies.push_back(Ok(get_reply("working")));
        // Dwell on input_required so the Waiting transition is observable.
        for _ in 0..200 {
            replies.push_back(Ok(get_reply("input_required")));
        }
        let transport = Arc::new(ScriptedTransport {
            replies: StdMutex::new(replies),
            cancels: StdMutex::new(Vec::new()),
            updates: StdMutex::new(Vec::new()),
            cancel_ack: true,
        });
        fx.runtime
            .start_task("srv", create_reply(), transport, CancellationToken::new())
            .await
            .unwrap();
        wait_node_state(&fx.tree, &node_id(), NodeState::Waiting).await;
    }

    #[tokio::test]
    async fn pre_wait_answer_is_refused_not_buffered() {
        let fx = fixture().await;
        let transport = ScriptedTransport::repeat_forever("working");
        let owner = CancellationToken::new();
        fx.runtime
            .start_task("srv", create_reply(), transport, owner.clone())
            .await
            .unwrap();
        let error = fx
            .runtime
            .submit_answer(
                &node_id(),
                InputAnswer {
                    responses: BTreeMap::new(),
                },
            )
            .await
            .expect_err("answers before Waiting must be refused");
        assert_eq!(error, AnswerRoutingError::NotWaiting);
        owner.cancel();
        wait_node_state(&fx.tree, &node_id(), NodeState::Cancelled).await;
    }

    #[tokio::test]
    async fn artifact_failure_fails_closed_before_waiting() {
        let fx = fixture().await;
        fx.runtime.set_artifact_sink(Arc::new(FailingArtifactSink));
        let transport = ScriptedTransport::repeat_forever("input_required");
        fx.runtime
            .start_task("srv", create_reply(), transport, CancellationToken::new())
            .await
            .unwrap();
        wait_node_state(&fx.tree, &node_id(), NodeState::Failed).await;
        assert!(
            !fx.journal
                .load()
                .await
                .unwrap()
                .iter()
                .any(|entry| matches!(
                    &entry.record,
                    JournalRecord::Checkpoint(cp)
                        if cp.id == node_id() && cp.state == NodeState::Waiting
                ))
        );
    }

    #[tokio::test]
    async fn input_required_without_requests_fails_without_parking() {
        let fx = fixture().await;
        let mut malformed = get_reply("input_required");
        malformed.input_requests = None;
        let transport = Arc::new(ScriptedTransport {
            replies: StdMutex::new(VecDeque::from([Ok(malformed)])),
            cancels: StdMutex::new(Vec::new()),
            updates: StdMutex::new(Vec::new()),
            cancel_ack: true,
        });
        fx.runtime
            .start_task("srv", create_reply(), transport, CancellationToken::new())
            .await
            .unwrap();
        wait_node_state(&fx.tree, &node_id(), NodeState::Failed).await;
        assert!(
            !fx.journal
                .load()
                .await
                .unwrap()
                .iter()
                .any(|entry| matches!(
                    &entry.record,
                    JournalRecord::Checkpoint(cp)
                        if cp.id == node_id() && cp.state == NodeState::Waiting
                ))
        );
    }

    #[tokio::test]
    async fn refreshed_input_request_key_gets_its_own_ticket() {
        let fx = fixture().await;
        let mut replies = VecDeque::from([Ok(input_reply_with_keys(&["confirm"]))]);
        replies.extend(
            std::iter::repeat_with(|| Ok(input_reply_with_keys(&["confirm", "reason"]))).take(500),
        );
        let transport = Arc::new(ScriptedTransport {
            replies: StdMutex::new(replies),
            cancels: StdMutex::new(Vec::new()),
            updates: StdMutex::new(Vec::new()),
            cancel_ack: true,
        });
        fx.runtime
            .start_task("srv", create_reply(), transport, CancellationToken::new())
            .await
            .unwrap();
        wait_node_state(&fx.tree, &node_id(), NodeState::Waiting).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let room = fx.journal.project_room("test-host").await.unwrap();
                if room
                    .nodes()
                    .get(&node_id())
                    .is_some_and(|view| view.open_tickets.len() == 2)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the refreshed request key received a durable ticket");
        fx.runtime.kill_all_tasks().await;
    }

    #[tokio::test]
    async fn unacknowledged_cancel_fails_without_forging_cancelled() {
        let fx = fixture().await;
        let replies = std::iter::repeat_with(|| Ok(get_reply("input_required")))
            .take(10_000)
            .collect();
        let transport = Arc::new(ScriptedTransport {
            replies: StdMutex::new(replies),
            cancels: StdMutex::new(Vec::new()),
            updates: StdMutex::new(Vec::new()),
            cancel_ack: false,
        });
        let owner = CancellationToken::new();
        fx.runtime
            .start_task("srv", create_reply(), transport, owner.clone())
            .await
            .unwrap();
        wait_node_state(&fx.tree, &node_id(), NodeState::Waiting).await;
        owner.cancel();
        wait_node_state(&fx.tree, &node_id(), NodeState::Failed).await;
        let records = fx.journal.load().await.unwrap();
        assert!(!records.iter().any(|entry| matches!(
            &entry.record,
            JournalRecord::Checkpoint(cp)
                if cp.id == node_id() && cp.state == NodeState::Cancelled
        )));
        assert!(records.iter().any(|entry| matches!(
            &entry.record,
            JournalRecord::Room(RoomEvent::TicketResolved {
                outcome: TicketResolution::CancelUnconfirmed { .. },
                ..
            })
        )));
    }

    #[tokio::test]
    async fn deadline_while_waiting_bridges_to_failed() {
        let fx = fixture_with_poll(PollConfig {
            interval: Duration::from_millis(1),
            deadline: Duration::from_millis(40),
            request_timeout: Duration::from_millis(10),
            max_status_updates: 64,
        })
        .await;
        let transport = ScriptedTransport::repeat_forever("input_required");
        fx.runtime
            .start_task("srv", create_reply(), transport, CancellationToken::new())
            .await
            .unwrap();
        wait_node_state(&fx.tree, &node_id(), NodeState::Waiting).await;
        wait_node_state(&fx.tree, &node_id(), NodeState::Failed).await;
    }

    #[tokio::test]
    async fn invalid_answer_shapes_are_refused_before_tasks_update() {
        let fx = fixture().await;
        let transport = ScriptedTransport::new(&[]);
        let request = InputRequest {
            method: "elicitation/create".into(),
            params: serde_json::json!({
                "requestedSchema": {
                    "type": "object",
                    "properties": {
                        "choice": {
                            "type": "string",
                            "enum": ["yes", "no"]
                        }
                    },
                    "required": ["choice"]
                }
            }),
        };
        let outstanding = BTreeMap::from([("prompt".into(), request)]);

        assert!(matches!(
            handle_answer(
                &fx.runtime,
                "task-1",
                transport.as_ref(),
                &outstanding,
                InputAnswer {
                    responses: BTreeMap::new()
                },
            )
            .await
            .unwrap(),
            AnswerOutcome::Refused { .. }
        ));

        for response in [
            InputResponse {
                action: "accept".into(),
                content: None,
            },
            InputResponse {
                action: "accept".into(),
                content: Some(serde_json::json!({"choice": "maybe"})),
            },
        ] {
            assert!(matches!(
                handle_answer(
                    &fx.runtime,
                    "task-1",
                    transport.as_ref(),
                    &outstanding,
                    InputAnswer {
                        responses: BTreeMap::from([("prompt".into(), response)])
                    },
                )
                .await
                .unwrap(),
                AnswerOutcome::Refused { .. }
            ));
        }
        assert!(transport.updates.lock().is_empty());
    }

    #[tokio::test]
    async fn tasks_update_timeout_is_bounded_and_leaves_the_answer_unaccepted() {
        let fx = fixture_with_poll(PollConfig {
            interval: Duration::from_millis(1),
            deadline: Duration::from_secs(1),
            request_timeout: Duration::from_millis(10),
            max_status_updates: 64,
        })
        .await;
        let outstanding = BTreeMap::from([(
            "confirm".into(),
            InputRequest {
                method: "elicitation/create".into(),
                params: serde_json::json!({
                    "requestedSchema": {
                        "type": "object",
                        "properties": { "confirm": { "type": "boolean" } },
                        "required": ["confirm"]
                    }
                }),
            },
        )]);
        let error = handle_answer(
            &fx.runtime,
            "task-1",
            &HangingUpdateTransport,
            &outstanding,
            InputAnswer {
                responses: BTreeMap::from([(
                    "confirm".into(),
                    InputResponse {
                        action: "accept".into(),
                        content: Some(serde_json::json!({"confirm": true})),
                    },
                )]),
            },
        )
        .await
        .expect_err("a silent tasks/update must time out");
        assert!(error.to_string().contains("tasks/update"));
        assert!(error.to_string().contains("timeout"));
    }

    /// AC5 (crash half): a mid-poll transport death drives the node to
    /// `Failed` instead of stranding it in `Running`.
    #[tokio::test]
    async fn poll_transport_death_drives_failed_not_stranded() {
        let fx = fixture().await;
        let mut replies = std::collections::VecDeque::new();
        replies.push_back(Ok(get_reply("working")));
        replies.push_back(Err(McpError::TransportClosed("child exited".into())));
        let transport = Arc::new(ScriptedTransport {
            replies: StdMutex::new(replies),
            cancels: StdMutex::new(Vec::new()),
            updates: StdMutex::new(Vec::new()),
            cancel_ack: true,
        });
        fx.runtime
            .start_task("srv", create_reply(), transport, CancellationToken::new())
            .await
            .unwrap();
        wait_node_state(&fx.tree, &node_id(), NodeState::Failed).await;
    }

    /// AC6 restart-read seed: while a task runs, the journal alone projects a
    /// room that shows the live node AND its MCP task binding — the durable
    /// surface a restart reads. (The full kill-host recovery keystone runs
    /// against the real fake in tests/integration_mcp_tasks.rs.)
    #[tokio::test]
    async fn journal_projection_reports_live_task_binding() {
        let fx = fixture().await;
        let transport = ScriptedTransport::repeat_forever("working");
        fx.runtime
            .start_task("srv", create_reply(), transport, CancellationToken::new())
            .await
            .unwrap();
        let id = node_id();
        wait_node_state(&fx.tree, &id, NodeState::Running).await;

        let room = fx
            .journal
            .project_room("test-host")
            .await
            .expect("project_room");
        let view = room.nodes().get(&id).expect("node projected from journal");
        assert_eq!(view.state, NodeState::Running);
        assert_eq!(
            view.mcp_task,
            Some(("srv".to_string(), "task-1".to_string()))
        );
        // Identity is also recoverable from the node id itself.
        assert_eq!(
            parse_mcp_node_id(&id)
                .as_ref()
                .map(|(server, task)| (server.as_str(), task.as_str())),
            Some(("srv", "task-1"))
        );
    }

    /// AC2 (c) mutant — illegal direct edge: `Waiting → Completed` is illegal
    /// (R-5); a direct `try_set_state` returns `Err`, never silently swallowed.
    /// The driver routes `Waiting → Running → Completed` explicitly instead.
    #[tokio::test]
    async fn waiting_to_completed_direct_edge_is_illegal() {
        let fx = fixture().await;
        // Manually drive a peer node through Created -> Running -> Waiting.
        let id = node_id();
        fx.runtime
            .nodes
            .register_task_node(&id, "mcp-task")
            .await
            .unwrap();
        fx.runtime
            .nodes
            .try_set_state(&id, NodeState::Running)
            .await
            .unwrap();
        fx.runtime
            .nodes
            .try_set_state(&id, NodeState::Waiting)
            .await
            .unwrap();
        let Err(error) = fx
            .runtime
            .nodes
            .try_set_state(&id, NodeState::Completed)
            .await
        else {
            panic!("Waiting -> Completed must be a loud Err, never silently swallowed");
        };
        assert!(
            matches!(
                error,
                crate::domain::ports::TaskNodesError::InvalidTransition {
                    from: NodeState::Waiting,
                    to: NodeState::Completed
                }
            ),
            "expected an InvalidTransition for Waiting -> Completed, got {error:?}"
        );
    }

    /// AC6 (R-2) keystone: a `Waiting` node stamped `AwaitingHumanInput`
    /// escalates past the threshold; the mutant — a NON-escalating reason
    /// (`BudgetPaused`) — must NOT. Proves `escalates()` is read on the hazard
    /// path, not merely stored.
    #[tokio::test]
    async fn waiting_hazard_reads_the_stamped_reason() {
        use crate::domain::clock::MockClock;
        use crate::domain::models::WAITING_HAZARD_THRESHOLD_MS;
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(
            NodeJournal::open_workspace(dir.path())
                .await
                .expect("journal opens"),
        );
        let (tx, _rx) = mpsc::unbounded_channel();
        let clock = Arc::new(MockClock::at_wall_ms(0));
        let now_fn = {
            let c = clock.clone();
            std::sync::Arc::new(move || c.wall_now_ms())
        };
        let tree = Arc::new(NodeTree::with_event_tx(tx, now_fn).with_journal(journal.clone()));

        let escalating = AgentId::from_validated("mcp/waiting-escalates");
        let non_escalating = AgentId::from_validated("mcp/waiting-paused");
        for id in &[escalating.clone(), non_escalating.clone()] {
            tree.register_task_node(id, "mcp-task").await.unwrap();
            tree.try_set_state(id, NodeState::Running).await.unwrap();
            tree.try_set_state(id, NodeState::Waiting).await.unwrap();
        }
        tree.stamp_wait_reason(&escalating, Some(WaitReason::AwaitingHumanInput))
            .await
            .unwrap();
        tree.stamp_wait_reason(&non_escalating, Some(WaitReason::BudgetPaused))
            .await
            .unwrap();

        // Advance the clock past the 60s threshold.
        clock.advance(Duration::from_millis(
            (WAITING_HAZARD_THRESHOLD_MS + 1) as u64,
        ));
        let hazards = tree.raise_due_hazards(WAITING_HAZARD_THRESHOLD_MS).await;
        assert!(
            hazards.contains(&escalating),
            "AwaitingHumanInput must escalate past the threshold"
        );
        assert!(
            !hazards.contains(&non_escalating),
            "BudgetPaused (non-escalating) must NOT raise a hazard — the mutant"
        );
    }
}
