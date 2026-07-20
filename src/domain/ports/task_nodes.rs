//! `TaskNodes` — the narrow domain seam by which an external-task driver
//! (17.5a: the MCP Tasks driver in `adapters/mcp`) reaches node-tree
//! lifecycle WITHOUT holding a concrete `NodeTree` (which lives in
//! `infrastructure/subagent/`).
//!
//! Story 17.5a (ADR-17-5-01, ruling D2). Sibling of [`SupervisedNodes`]
//! (ADR-17-2c-01): that port carries subtree cascade; this one carries
//! single-node register / transition / deregister. Deliberately NOT a
//! widening of `SupervisedNodes` (ADR-11-3: a new capability is a sibling
//! seam). The concrete `NodeTree` implements this port and is constructed
//! only at the composition root (`startup.rs`).
//!
//! The port carries exactly the methods a 17.5a code path calls — no dead
//! methods. Every mutation is durable: the implementing tree journals
//! checkpoint + room event per call (`register_peer` / `try_set_state`).

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::domain::models::{AgentId, NodeState, Op};

/// Live handles a task driver needs after registration: the cooperative
/// cancel token and the owner-command channel (`Op::Kill` arrives here via
/// `SupervisedNodes::cascade_kill`). Both ends stay with the driver; the tree
/// keeps the matching `NodeHandle::Local`.
pub struct TaskNodeHandle {
    pub cancel_token: CancellationToken,
    pub command_rx: mpsc::Receiver<Op>,
}

/// Failure surface for node lifecycle reached through the seam. Kept in
/// `domain/` so the trait carries no infra error type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskNodesError {
    /// The node is not (or no longer) in the tree.
    NotFound(AgentId),
    /// The FSM rejected the transition (e.g. `Running -> Created`, or any
    /// edge out of a terminal state). Loud by design — callers must
    /// propagate, never swallow (17.4b review: silent `set_state` discarded
    /// exactly this class).
    InvalidTransition { from: NodeState, to: NodeState },
    /// Registration or durability failed. Carries a sanitized message.
    Internal(String),
}

impl std::fmt::Display for TaskNodesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(id) => write!(f, "task node not found: {id}"),
            Self::InvalidTransition { from, to } => {
                write!(f, "illegal task node transition: {from:?} -> {to:?}")
            }
            Self::Internal(msg) => write!(f, "task node lifecycle failed: {msg}"),
        }
    }
}

impl std::error::Error for TaskNodesError {}

/// Single-node lifecycle for external-task drivers across a domain boundary.
#[async_trait::async_trait]
pub trait TaskNodes: Send + Sync {
    /// Register `node_id` as a live peer node (`OwnershipKind::Peer`,
    /// `NodeOrigin::Remote`, `NodeHandle::Local`) and return the driver-side
    /// handles. Registration is durable (checkpoint + `NodeRegistered`
    /// journaled by the implementing tree).
    async fn register_task_node(
        &self,
        node_id: &AgentId,
        subagent_type: &str,
    ) -> Result<TaskNodeHandle, TaskNodesError>;

    /// Drive a lifecycle transition, propagating every error loudly. The
    /// implementing tree journals checkpoint + `NodeStateChanged` atomically
    /// and rejects illegal edges; callers MUST NOT retry or swallow
    /// `InvalidTransition`.
    async fn try_set_state(
        &self,
        node_id: &AgentId,
        target: NodeState,
    ) -> Result<(), TaskNodesError>;
}
