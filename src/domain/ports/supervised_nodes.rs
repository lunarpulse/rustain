//! `SupervisedNodes` — the narrow domain seam by which the `Supervisor`
//! reaches node-tree lifecycle mutation WITHOUT holding a concrete `NodeTree`.
//!
//! Story 17.2c (discharges the 17-2b review's D7). The fork-join executor still
//! holds no `NodeTree` — it reaches lifecycle only through the `Supervisor`
//! (17-2b R6), and the supervisor reaches the tree only through this `dyn`
//! trait. The concrete `NodeTree` is constructed only at the composition root;
//! this port carries NO concrete coupling (the domain-import guards in
//! `conformance_node_tree.rs` stay green). See **ADR-17-2c-01** for the
//! conscious revisit of ADR-14-3-01 D-2 (its intent — no infra coupling in the
//! executor — is preserved; its letter is deliberately superseded).
//!
//! The port is intentionally minimal (party ruling fork 1 + dev-story roundtable
//! ruling 4): it exposes exactly the mutations the supervisor-driven lifecycle
//! needs — no dead methods. Story 17.2d-b added `register_parked` (durable
//! park-time node registration); the `Parked` record itself rides the SAME
//! atomic batch as the checkpoint so registration + park record are
//! all-or-nothing.

use std::time::Duration;

use crate::domain::models::AgentId;
use crate::domain::models::agent_node::NodeCheckpoint;
use crate::domain::models::orchestration::SpokeSpec;

/// Failure surface for a lifecycle mutation reached through the seam. Kept in
/// `domain/` so the trait carries no infra error type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisedNodesError {
    /// The root of the requested subtree is not (or no longer) in the tree —
    /// e.g. never launched, or already reaped. Benign for wave-abort: there is
    /// nothing to cascade.
    NotFound(AgentId),
    /// The mutation could not complete (e.g. an unresponsive child during a
    /// cascade). Carries a sanitized message.
    Internal(String),
}

impl std::fmt::Display for SupervisedNodesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(id) => write!(f, "supervised node not found: {id}"),
            Self::Internal(msg) => write!(f, "supervised-nodes mutation failed: {msg}"),
        }
    }
}

impl std::error::Error for SupervisedNodesError {}

/// Lifecycle mutation the supervisor may drive on the node tree through a
/// domain boundary.
#[async_trait::async_trait]
pub trait SupervisedNodes: Send + Sync {
    /// Drive `root` and its entire subtree terminal-`Cancelled`: revoke each
    /// token, deliver the cooperative kill, journal `set_state(Cancelled)`, then
    /// `deregister` — bottom-up (17-2b R7), all-or-nothing on the terminal-batch
    /// checkpoint (D2). Returns the ids driven terminal. `NotFound` is benign
    /// for a node that was never launched.
    async fn cascade_kill(
        &self,
        root: &AgentId,
        timeout_per_node: Duration,
    ) -> Result<Vec<AgentId>, SupervisedNodesError>;

    /// Story 17.2d-b (AC-b1): register a fork-join spoke parked on upstream
    /// artifacts as a durable `Suspended` tree node, journaling the identity
    /// checkpoint + `RoomEvent::NodeRegistered` + the `Parked` record
    /// (relaunch plan + readiness edges) as ONE atomic batch — the durable
    /// park write and the node registration are all-or-nothing (a partial
    /// write leaves no orphaned readiness). `checkpoint` arrives with
    /// `state == Suspended` and `wait_reason == AwaitingUpstreamArtifact`;
    /// `checkpoint.id` is the full nonce-qualified id. The live launch of the
    /// same spoke later ADOPTS this node (no second register).
    async fn register_parked(
        &self,
        checkpoint: NodeCheckpoint,
        spec: SpokeSpec,
        producers: Vec<AgentId>,
        concurrency: usize,
    ) -> Result<(), SupervisedNodesError>;

    /// Terminalize and remove a never-launched durable park. The terminal
    /// checkpoint, state-change event, and `Unparked` record are one atomic
    /// journal batch; already-adopted or already-removed nodes are a no-op.
    async fn cancel_parked(&self, node: &AgentId) -> Result<(), SupervisedNodesError>;
}
