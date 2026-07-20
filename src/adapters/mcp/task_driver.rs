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
//! - `input_required` is decoded and logged, never transitioned — 17.5a has
//!   no `Waiting` edge (that arc is 17.5b's, R-5).
//! - The poll loop never inherits `call_tool`'s 60s bound (R-16.4).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::domain::clock::Clock;
use crate::domain::models::{AgentId, NodeState, Op, RoomEvent, ToolResult};
use crate::domain::ports::{RoomJournal, SupervisedNodes, TaskNodes, TaskNodesError};

use super::error::McpError;
use super::task_transport::McpTaskTransport;
use super::tasks::{CreateTaskReply, TaskStatus};

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

/// Everything a live MCP task node needs, injected at the composition root.
/// One runtime per `McpClientAdapter`; shared by every task the server
/// creates.
pub struct McpTaskRuntime {
    nodes: Arc<dyn TaskNodes>,
    supervised: Arc<dyn SupervisedNodes>,
    room: Arc<dyn RoomJournal>,
    clock: Arc<dyn Clock>,
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
            live: Mutex::new(LiveState::default()),
            poll: PollConfig::default(),
        }
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

        let runtime = Arc::clone(self);
        let driver_node = node_id.clone();
        let driver_server = server_id.to_string();
        let driver_task = task_id.clone();
        tokio::spawn(async move {
            run_driver(
                runtime,
                driver_node,
                driver_server,
                driver_task,
                transport,
                owner_cancel,
                node_cancel,
                handle,
                reply,
            )
            .await;
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

/// Wire status → node state, for the statuses 17.5a acts on.
/// `InputRequired` is deliberately NOT mapped here — the poll loop decodes
/// and logs it without a transition (R-5's `Waiting` edges are 17.5b's).
fn terminal_node_state_for(status: &TaskStatus) -> Option<NodeState> {
    match status {
        TaskStatus::Completed => Some(NodeState::Completed),
        TaskStatus::Failed => Some(NodeState::Failed),
        TaskStatus::Cancelled => Some(NodeState::Cancelled),
        TaskStatus::Working | TaskStatus::InputRequired => None,
    }
}

/// The driver task: owns the transport for one MCP task node until terminal.
///
/// Cancellation wiring (17.4b's two HIGH findings closed here by
/// construction): the node's own `CancellationToken`, an `Op::Kill` arriving
/// through the command channel, and the owner (adapter session) token all
/// converge on one cooperative cancel path that issues a real `tasks/cancel`
/// and drives the local cascade on the ack.
#[allow(clippy::too_many_arguments)]
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
    )
    .await;
    match outcome {
        Ok(()) => runtime.unmark_live(&node_id).await,
        Err(error) => {
            tracing::warn!(%node_id, %task_id, %error, "MCP task driver ended with error");
            // P3: never strand a durable node non-terminal. Force it to
            // `Failed`; stop tracking it ONLY once it is actually terminal,
            // else leave it in `live` so session teardown can reap it.
            match runtime
                .nodes
                .try_set_state(&node_id, NodeState::Failed)
                .await
            {
                Ok(()) | Err(TaskNodesError::NotFound(_)) => {
                    runtime.unmark_live(&node_id).await;
                }
                Err(TaskNodesError::InvalidTransition { from, .. }) if from.is_terminal() => {
                    runtime.unmark_live(&node_id).await;
                }
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
    // P13: a task born terminal is routed Created -> Running -> <terminal>
    // (the FSM has no Created -> terminal edge) and never enters the poll
    // loop, so a stray later poll cannot flip an already-settled task.
    if let Some(terminal) = terminal_node_state_for(&initial.task.status) {
        drive_state(runtime, node_id, terminal).await?;
        return Ok(());
    }

    // 2. Poll loop. The interval honors the server's pollIntervalMs when
    //    present; the loop never inherits call_tool's 60s bound (R-16.4).
    let interval = initial
        .task
        .poll_interval_ms
        .map(Duration::from_millis)
        .filter(|d| !d.is_zero())
        .unwrap_or(runtime.poll.interval);
    let deadline = tokio::time::Instant::now() + runtime.poll.deadline;
    let mut status_updates = 0usize;
    let mut last_status = initial.task.status.clone();

    loop {
        // P5: enforce the total deadline INDEPENDENTLY of the (untrusted,
        // unbounded) server pollIntervalMs — clamp each sleep to the time
        // remaining so a huge pollIntervalMs cannot defer the deadline.
        let now = tokio::time::Instant::now();
        if now >= deadline {
            drive_state(runtime, node_id, NodeState::Failed).await?;
            return Err(McpError::TaskFailed(format!(
                "task {task_id} exceeded the poll deadline ({:?})",
                runtime.poll.deadline
            )));
        }
        let sleep_for = interval.min(deadline - now);

        tokio::select! {
            biased;
            _ = owner_cancel.cancelled() => {
                return cooperative_cancel(runtime, node_id, task_id, transport, "owner cancelled").await;
            }
            _ = node_cancel.cancelled() => {
                return cooperative_cancel(runtime, node_id, task_id, transport, "teardown").await;
            }
            _ = handle.cancel_token.cancelled() => {
                return cooperative_cancel(runtime, node_id, task_id, transport, "node kill").await;
            }
            op = handle.command_rx.recv() => {
                match op {
                    Some(Op::Kill) => {
                        return cooperative_cancel(runtime, node_id, task_id, transport, "Op::Kill").await;
                    }
                    Some(_) => continue, // Pause/Resume/… are not meaningful for MCP tasks
                    None => return cooperative_cancel(runtime, node_id, task_id, transport, "command channel closed").await,
                }
            }
            _ = tokio::time::sleep(sleep_for) => {
                let poll = poll_once(transport, task_id, runtime.poll.request_timeout).await;
                let outcome = match poll {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        // Transport death (child exit, wedge) mid-task: the
                        // task is unreachable — drive Failed rather than
                        // stranding the node in `Running` (AC5).
                        tracing::warn!(%node_id, %task_id, %error, "MCP task poll failed; driving node Failed");
                        drive_state(runtime, node_id, NodeState::Failed).await?;
                        return Err(error);
                    }
                };
                match outcome {
                    PollOutcome::Continue(status) => {
                        if status == TaskStatus::InputRequired {
                            // Decoded and logged, never transitioned (17.5b's arc).
                            tracing::info!(%node_id, %task_id, "MCP task is input_required; 17.5a does not resume");
                        }
                        if status != last_status {
                            status_updates += 1;
                            if status_updates > runtime.poll.max_status_updates {
                                drive_state(runtime, node_id, NodeState::Failed).await?;
                                return Err(McpError::TaskFailed(format!(
                                    "task {task_id} exceeded the status-update cap"
                                )));
                            }
                            last_status = status;
                        }
                    }
                    PollOutcome::Terminal(state) => {
                        drive_state(runtime, node_id, state).await?;
                        // Terminal nodes STAY in the tree (17.4b taint
                        // semantics); `cascade_kill` deregisters on kill paths.
                        return Ok(());
                    }
                }
            }
        }
    }
}

enum PollOutcome {
    Continue(TaskStatus),
    Terminal(NodeState),
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
    // P8: a reply for a different task must never drive THIS node's state.
    if reply.task.task_id != task_id {
        return Err(McpError::TaskProtocol(format!(
            "tasks/get for {task_id} returned a mismatched taskId {:?}",
            reply.task.task_id
        )));
    }
    match terminal_node_state_for(&reply.task.status) {
        Some(NodeState::Completed) => {
            // R-14: isError:true stays Completed — the error rides the
            // inlined ToolResult payload, exactly as a non-task erroring
            // tool call surfaces it.
            let is_error = reply
                .result
                .as_ref()
                .and_then(|r| r.get("isError"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            if is_error {
                tracing::info!(%task_id, "MCP task completed with isError:true (tool-level failure)");
            }
            Ok(PollOutcome::Terminal(NodeState::Completed))
        }
        Some(state) => Ok(PollOutcome::Terminal(state)),
        None => Ok(PollOutcome::Continue(reply.task.status)),
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
    // P4: bound the cancel round-trip exactly like a poll so a connected-
    // but-silent server cannot wedge the driver (and its `live` entry).
    let cancel = tokio::time::timeout(
        runtime.poll.request_timeout,
        transport.tasks_cancel(task_id),
    )
    .await;
    match cancel {
        // R-15: the ack (never an observed status) gates the local Cancelled.
        Ok(Ok(_ack)) => {}
        // Session already gone / cancel timed out: owner intent is
        // unambiguous, so terminalize anyway rather than strand (AC5) — an
        // explicit FORCE, distinct from an ack.
        Ok(Err(error @ McpError::TransportClosed(_))) => {
            tracing::warn!(%node_id, %task_id, %error, "tasks/cancel unreachable; driving local cascade on owner intent");
        }
        Err(_elapsed) => {
            tracing::warn!(%node_id, %task_id, "tasks/cancel timed out; driving local cascade on owner intent");
        }
        // Server actively rejected cancellation: do NOT forge a Cancelled —
        // propagate so the driver finalizes the node Failed.
        Ok(Err(error)) => return Err(error),
    }
    drive_state(runtime, node_id, NodeState::Cancelled).await?;
    Ok(())
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
    fn input_required_maps_to_no_transition_in_17_5a() {
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
            Ok(TaskAck {
                result_type: Some("complete".into()),
            })
        }
    }

    fn get_reply(status: &str) -> TaskGetReply {
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
            input_requests: None,
        }
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

    async fn fixture() -> Fixture {
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
            McpTaskRuntime::new(tree.clone(), tree.clone(), room, clock)
                .with_poll_config(PollConfig::fast_config()),
        );
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

    /// AC3 (17.5a half): `input_required` is decoded and logged without any
    /// FSM transition; the task continues polling and reaches terminal.
    #[tokio::test]
    async fn input_required_decodes_and_logs_without_transition() {
        let fx = fixture().await;
        let transport = ScriptedTransport::new(&["working", "input_required", "completed"]);
        fx.runtime
            .start_task("srv", create_reply(), transport, CancellationToken::new())
            .await
            .unwrap();
        // Straight to Completed — a Waiting edge does not exist in 17.5a and
        // an illegal transition attempt would surface as a driver error and a
        // Failed node instead.
        wait_node_state(&fx.tree, &node_id(), NodeState::Completed).await;
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
}
