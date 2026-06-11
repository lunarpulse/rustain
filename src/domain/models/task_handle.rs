use crate::domain::models::SubagentRunStatus;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Owner-side handle returned by `SubagentRunner::launch`. Owns the per-agent
/// CancellationToken + bounded command channel (Op mpsc). Closing the handle
/// (drop) does NOT abort the child — child is bound to the parent's
/// CancellationToken tree. Explicit kill: `cancel_token.cancel()`.
pub struct TaskHandle {
    pub agent_id: crate::domain::models::AgentId,
    pub status_rx: mpsc::Receiver<SubagentRunStatus>, // FG event stream (codex precedent)
    pub command_tx: mpsc::Sender<Op>,                 // 512-cap, see in_process_runner.rs
    pub cancel: CancellationToken,                    // child token derived from parent
    pub task_id: String,                              // matches spool filename (nanoid 12 char)
    pub subagent_type: String,                        // threaded through from SubagentProvider
    pub spawned_at: i64,                              // epoch millis from registry::register
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
    ReportFull,
}
