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
//! ruling 4): it exposes exactly the mutation a 17-2c code path calls — no dead
//! methods. Durable park does NOT go through here (it is a write-ahead journal
//! record written through the supervisor's own `NodeJournal` handle).

use std::time::Duration;

use crate::domain::models::AgentId;

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
}
