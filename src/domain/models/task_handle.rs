use crate::domain::models::{NodeState, UnifiedDiff};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// Owner-side handle returned by `SubagentRunner::launch`. Owns the per-agent
/// CancellationToken + bounded command channel (Op mpsc) **and** the
/// owner-liveness sender the child watches for abandonment (AC3).
///
/// Dropping the handle does NOT immediately abort the child — the child stays
/// bound to the parent's CancellationToken tree for explicit kill, but the
/// `parent_disconnect` sender drops with the handle, which lets an `Owned`
/// child detect loss of its owner connection and follow
/// `disconnect → retry → self-destruct`.
pub struct TaskHandle {
    pub agent_id: crate::domain::models::AgentId,
    pub status_rx: mpsc::Receiver<NodeState>, // unified node-state event stream
    pub command_tx: mpsc::Sender<Op>,         // 512-cap, see in_process_runner.rs
    pub cancel: CancellationToken,            // child token derived from parent
    pub task_id: String,                      // matches spool filename (nanoid 12 char)
    pub subagent_type: String,                // threaded through from SubagentProvider
    pub spawned_at: i64,                      // epoch millis from registry::register
    pub parent_disconnect: mpsc::UnboundedSender<()>, // drop of this sender = owner connection lost
    /// Optional structured-yield channel (Story 14.3 AC6). When `Some`, the
    /// child emits its final assistant text as a JSON `SpokeYield` here, which
    /// the fork-join collector drains to drive the structured result contract
    /// (validate / retry / salvage). `None` in R1 production runners that
    /// surface the assistant text through the conversation store instead — the
    /// collector then records an honest `Empty` for a "completed" spoke that
    /// produced no capturable yield.
    pub yield_rx: Option<mpsc::Receiver<String>>,
    /// Optional isolation-delta channel (Story 14.5 AC2 / DD3). When `Some`,
    /// the runner captured the isolated child's `UnifiedDiff` on terminal and
    /// sends it here; the fork-join collector drains it (bounded await) and
    /// inserts into `ForkJoinRun::delta_store[agent_id]` — the NFR68
    /// capturable-but-inert seam (write-only in R1; read by nobody until R2).
    /// `None` for non-isolated children.
    pub isolation_diff_rx: Option<oneshot::Receiver<UnifiedDiff>>,
}

/// Owner-issued operations on a running subagent. Story 10.4 consumes this; Story 10.2 wires panel keybinds.
/// Pause/Resume/ChangeModel/UpdateTools are reserved for Story 10.2; v0 only constructs Kill.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum Op {
    Kill,
    Pause,
    Resume,
    ChangeModel(String),
    UpdateTools(Vec<String>),
    Deliver(crate::domain::models::AgentDelivery),
    ReportFull,
}
