//! `AgentNode`, `NodeOrigin`, and `NodeCheckpoint` — the tree-resident agent
//! record introduced by Story 14.1.
//!
//! `AgentNode` is the runtime, mutable view of an agent that lives inside the
//! `NodeTree`. It carries scheduling metadata (foreground/background — settled
//! D1 as NOT lifecycle), inspector counters (AC11), and the lineage fields
//! carried over from the legacy `RegistryEntry`.
//!
//! - **`AgentNode`** holds live, possibly-mutated runtime state and intentionally
//!   does **not** impl `Serialize`/`Deserialize` — it owns transient fields and
//!   is not a persistence shape.
//! - **`NodeCheckpoint`** is the serializable snapshot of an `AgentNode`, used
//!   for persistence, IPC, and snapshotting. It mirrors `AgentNode` field-for-
//!   field but has no transient handles (those live in the `NodeTree`
//!   side-table, not on the node).
//! - **`NodeOrigin`** classifies how a node entered the tree.

use serde::{Deserialize, Serialize};

use crate::domain::models::agent_id::AgentId;
use crate::domain::models::capability_token::CapabilityTokenId;
use crate::domain::models::node_state::NodeState;
use crate::domain::models::subagent_view::OwnershipKind;

/// How an `AgentNode` entered the tree.
///
/// `#[non_exhaustive]` so new entry paths (e.g. future transport-backed
/// spawns) can be added without breaking exhaustive matches downstream.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NodeOrigin {
    /// Spawned by an interactive user action (e.g. `/agents spawn`).
    Interactive,
    /// Spawned by another agent via the task/subagent tool.
    Subagent,
    /// Spawned by a scheduled cron trigger.
    Cron,
    /// Spawned by an inbound channel message.
    Channel,
    /// Spawned by a remote peer over the mesh transport.
    Remote,
}

/// Runtime, tree-resident record of an agent.
///
/// Lives inside `NodeTree`. Mutable. Deliberately **not** serializable —
/// runtime fields (counters, model selection) change every turn and are not a
/// persistence shape. For persistence/IPC use [`AgentNode::checkpoint`].
///
/// There is intentionally **no `handle` field**: the `NodeHandle` lives in a
/// `NodeTree` side-table keyed by [`AgentId`], not on the node itself.
#[derive(Clone, Debug)]
pub struct AgentNode {
    /// Stable identity of this node.
    pub id: AgentId,
    /// Authority token backing this node's delegated capabilities.
    pub token: CapabilityTokenId,
    /// Parent node, `None` for the root.
    pub parent: Option<AgentId>,
    /// Ownership relationship to the parent.
    pub ownership: OwnershipKind,
    /// Lifecycle state machine position (settled D2).
    pub state: NodeState,
    /// How this node entered the tree.
    pub origin: NodeOrigin,
    /// Scheduling metadata — `true` when running in the foreground.
    /// Foreground/background is NOT lifecycle (settled D1).
    pub foreground: bool,
    /// Resolved model after profile/tool resolution (AC11 inspector).
    pub effective_model: String,
    /// Cumulative input tokens (AC11 inspector).
    pub tokens_in: u32,
    /// Cumulative output tokens (AC11 inspector).
    pub tokens_out: u32,
    /// Cumulative turns taken (AC11 inspector).
    pub turns: u32,
    /// Subagent type label, carried from `RegistryEntry`.
    pub subagent_type: String,
    /// Epoch-millis spawn timestamp, carried from `RegistryEntry`.
    pub spawned_at: i64,
    /// Tree depth (root = 0), carried from `RegistryEntry`.
    pub depth: usize,
}

/// Live runtime metrics surfaced by a node for owner-facing inspection.
///
/// Pure data only — no runtime handles, channels, or adapter types — so both
/// the subagent runner and the unified node tree can depend on it without
/// violating the hexagonal dependency rule.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentMetrics {
    pub effective_model: String,
    pub tools_summary: String,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub turns: u32,
}

/// Serializable snapshot of an [`AgentNode`].
///
/// Mirrors `AgentNode` field-for-field but is `Serialize + Deserialize` for
/// persistence and IPC. Contains **no** transient handles — those live in the
/// `NodeTree` side-table and never cross a serialization boundary.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeCheckpoint {
    /// Stable identity of this node.
    pub id: AgentId,
    /// Authority token backing this node's delegated capabilities.
    pub token: CapabilityTokenId,
    /// Parent node, `None` for the root.
    pub parent: Option<AgentId>,
    /// Ownership relationship to the parent.
    pub ownership: OwnershipKind,
    /// Lifecycle state machine position.
    pub state: NodeState,
    /// How this node entered the tree.
    pub origin: NodeOrigin,
    /// `true` when running in the foreground.
    pub foreground: bool,
    /// Resolved model after profile/tool resolution.
    pub effective_model: String,
    /// Cumulative input tokens.
    pub tokens_in: u32,
    /// Cumulative output tokens.
    pub tokens_out: u32,
    /// Cumulative turns taken.
    pub turns: u32,
    /// Subagent type label.
    pub subagent_type: String,
    /// Epoch-millis spawn timestamp.
    pub spawned_at: i64,
    /// Tree depth (root = 0).
    pub depth: usize,
}

impl AgentNode {
    /// Produce a serializable snapshot of this node.
    ///
    /// Copies every field; does not touch the `NodeTree` side-table. The
    /// resulting `NodeCheckpoint` is safe to persist or send over IPC.
    pub fn checkpoint(&self) -> NodeCheckpoint {
        NodeCheckpoint {
            id: self.id.clone(),
            token: self.token,
            parent: self.parent.clone(),
            ownership: self.ownership,
            state: self.state,
            origin: self.origin,
            foreground: self.foreground,
            effective_model: self.effective_model.clone(),
            tokens_in: self.tokens_in,
            tokens_out: self.tokens_out,
            turns: self.turns,
            subagent_type: self.subagent_type.clone(),
            spawned_at: self.spawned_at,
            depth: self.depth,
        }
    }
}

/// Result of evaluating whether an owned node should be abandoned when its
/// parent's command channel has been dropped.
///
/// Returned by [`abandonment_action`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbandonmentAction {
    /// Continue normally — no disconnection detected.
    Continue,
    /// Retry reconnection to the parent's command channel.
    Retry,
    /// Self-destruct: transition to `Cancelled` and clean up.
    SelfDestruct,
    /// Ignore the disconnect — this node type never self-destructs.
    ///
    /// Returned for [`OwnershipKind::Peer`] (a peer node continues
    /// independently of any parent) and [`OwnershipKind::Self_`] (the root
    /// cannot be abandoned).
    Ignore,
}

/// Pure decision function for the owned-node abandonment protocol (AC3).
///
/// Given the node's [`OwnershipKind`], whether a disconnect has been detected,
/// and the current retry budget, decide what the node should do next. This is
/// intentionally a pure, synchronous function — no I/O, no timers, no side
/// effects — so the full input matrix is trivially testable.
///
/// # Protocol
///
/// - **`Owned`** nodes are bound to their parent's lifetime. On a detected
///   disconnect they retry reconnection up to `max_retries` times; once that
///   budget is exhausted they self-destruct (transition to `Cancelled` and
///   clean up).
/// - **`Peer`** nodes continue independently — a parent disconnect does not
///   affect them, so the signal is ignored.
/// - **`Self_`** (the root) can never be abandoned, so the signal is ignored.
///
/// If no disconnect has been detected the function short-circuits to
/// [`AbandonmentAction::Continue`] regardless of ownership — there is nothing
/// to react to.
///
/// # Async layer
///
/// The async monitoring layer that drives this function lives in
/// `in_process_runner::run_child` (`handle_abandonment_disconnect`): it watches
/// the parent `parent_disconnect` channel for owner drop, retries with backoff,
/// and transitions to `Cancelled` when the retry budget is exhausted. Only the
/// pure decision core lives here. That layer's tests should drive the
/// retry→backoff→self-destruct sequence via `tokio::time::pause()` +
/// `tokio::time::advance()` rather than real wall-clock sleeps.
pub fn abandonment_action(
    ownership: OwnershipKind,
    disconnect_detected: bool,
    retry_count: u8,
    max_retries: u8,
) -> AbandonmentAction {
    if !disconnect_detected {
        return AbandonmentAction::Continue;
    }
    match ownership {
        OwnershipKind::Owned => {
            if retry_count >= max_retries {
                AbandonmentAction::SelfDestruct
            } else {
                AbandonmentAction::Retry
            }
        }
        // `Peer` and `Self_` intentionally coincide here in R1: a Peer ignores
        // parent disconnect (independent lifetime) and the root `Self_` can
        // never be abandoned. They diverge in R2 once Peer nodes gain a
        // peer-liveness signal distinct from the ownership hierarchy.
        OwnershipKind::Peer | OwnershipKind::Self_ => AbandonmentAction::Ignore,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a representative `AgentNode` covering the non-default branches
    /// (`Some(parent)`, `Peer` ownership, foreground) so a regression that
    /// flips a field is more likely to surface.
    fn sample_node() -> AgentNode {
        AgentNode {
            id: AgentId("child123".into()),
            parent: Some(AgentId("parentabc".into())),
            token: CapabilityTokenId::root(),
            ownership: OwnershipKind::Peer,
            state: NodeState::Running,
            origin: NodeOrigin::Subagent,
            foreground: true,
            effective_model: "claude-opus-4".into(),
            tokens_in: 1234,
            tokens_out: 5678,
            turns: 7,
            subagent_type: "code-reviewer".into(),
            spawned_at: 1_700_000_000_000,
            depth: 2,
        }
    }

    #[test]
    fn checkpoint_roundtrips_through_json() {
        let node = sample_node();
        let original = node.checkpoint();

        let json = serde_json::to_string(&original).expect("serialize NodeCheckpoint");
        let back: NodeCheckpoint = serde_json::from_str(&json).expect("deserialize NodeCheckpoint");

        assert_eq!(original, back);
    }

    #[test]
    fn checkpoint_serializes_exact_field_set() {
        // Schema pin: adding/removing/renaming a field on NodeCheckpoint breaks
        // this test, which is the point — the persisted shape is a contract.
        let node = sample_node();
        let value = serde_json::to_value(node.checkpoint()).expect("serialize to Value");

        let object = value
            .as_object()
            .expect("NodeCheckpoint serializes to a JSON object");

        let actual: std::collections::BTreeSet<&str> = object.keys().map(String::as_str).collect();

        let expected: std::collections::BTreeSet<&str> = [
            "id",
            "parent",
            "token",
            "ownership",
            "state",
            "origin",
            "foreground",
            "effective_model",
            "tokens_in",
            "tokens_out",
            "turns",
            "subagent_type",
            "spawned_at",
            "depth",
        ]
        .into_iter()
        .collect();

        assert_eq!(actual, expected);
    }

    #[test]
    fn node_origin_has_five_variants() {
        // Count distinct variants by round-tripping each through serde and
        // checking the resulting tagged strings are unique and total five.
        let variants = [
            NodeOrigin::Interactive,
            NodeOrigin::Subagent,
            NodeOrigin::Cron,
            NodeOrigin::Channel,
            NodeOrigin::Remote,
        ];

        let tags: std::collections::BTreeSet<String> = variants
            .iter()
            .map(|v| serde_json::to_string(v).expect("serialize NodeOrigin"))
            .collect();

        // Exactly five distinct serializations.
        assert_eq!(
            tags.len(),
            5,
            "NodeOrigin must serialize to 5 distinct tags"
        );

        // Each variant round-trips intact.
        for v in variants {
            let json = serde_json::to_string(&v).expect("serialize NodeOrigin");
            let back: NodeOrigin = serde_json::from_str(&json).expect("deserialize NodeOrigin");
            assert_eq!(v, back);
        }
    }

    #[test]
    fn checkpoint_copies_all_fields() {
        let node = sample_node();
        let cp = node.checkpoint();

        // Identity / lineage.
        assert_eq!(cp.id, node.id);
        assert_eq!(cp.token, node.token);
        assert_eq!(cp.parent, node.parent);
        assert_eq!(cp.ownership, node.ownership);

        // Lifecycle + scheduling.
        assert_eq!(cp.state, node.state);
        assert_eq!(cp.origin, node.origin);
        assert_eq!(cp.foreground, node.foreground);

        // Inspector counters (AC11).
        assert_eq!(cp.effective_model, node.effective_model);
        assert_eq!(cp.tokens_in, node.tokens_in);
        assert_eq!(cp.tokens_out, node.tokens_out);
        assert_eq!(cp.turns, node.turns);

        // Legacy RegistryEntry fields.
        assert_eq!(cp.subagent_type, node.subagent_type);
        assert_eq!(cp.spawned_at, node.spawned_at);
        assert_eq!(cp.depth, node.depth);
    }

    // ---- AC3 abandonment protocol tests -------------------------------------

    #[test]
    fn abandonment_no_disconnect_all_kinds() {
        // When no disconnect has been detected, every ownership kind
        // short-circuits to Continue — there is nothing to react to.
        for ownership in [
            OwnershipKind::Owned,
            OwnershipKind::Peer,
            OwnershipKind::Self_,
        ] {
            assert_eq!(
                abandonment_action(ownership, false, 0, 3),
                AbandonmentAction::Continue,
                "{:?} with no disconnect must Continue",
                ownership,
            );
        }
    }

    #[test]
    fn abandonment_owned_retry() {
        // Owned + disconnect + retries remaining → Retry.
        assert_eq!(
            abandonment_action(OwnershipKind::Owned, true, 0, 3),
            AbandonmentAction::Retry,
        );
        assert_eq!(
            abandonment_action(OwnershipKind::Owned, true, 2, 3),
            AbandonmentAction::Retry,
        );
    }

    #[test]
    fn abandonment_owned_self_destruct() {
        // Owned + disconnect + retries exhausted → SelfDestruct.
        // Boundary: retry_count == max_retries trips self-destruct.
        assert_eq!(
            abandonment_action(OwnershipKind::Owned, true, 3, 3),
            AbandonmentAction::SelfDestruct,
        );
        // max_retries == 0 self-destructs on the first observed disconnect.
        assert_eq!(
            abandonment_action(OwnershipKind::Owned, true, 0, 0),
            AbandonmentAction::SelfDestruct,
        );
        // retries overshot: still SelfDestruct, never Retry past the budget.
        assert_eq!(
            abandonment_action(OwnershipKind::Owned, true, 5, 3),
            AbandonmentAction::SelfDestruct,
        );
    }

    #[test]
    fn abandonment_peer_ignores() {
        // Peer nodes continue independently of their parent — a disconnect is
        // ignored regardless of the retry budget.
        assert_eq!(
            abandonment_action(OwnershipKind::Peer, true, 0, 3),
            AbandonmentAction::Ignore,
        );
        assert_eq!(
            abandonment_action(OwnershipKind::Peer, true, 99, 3),
            AbandonmentAction::Ignore,
        );
        assert_eq!(
            abandonment_action(OwnershipKind::Peer, true, 3, 0),
            AbandonmentAction::Ignore,
        );
    }

    #[test]
    fn abandonment_self_ignores() {
        // The root (Self_) can never be abandoned — ignore the disconnect
        // regardless of the retry budget.
        assert_eq!(
            abandonment_action(OwnershipKind::Self_, true, 0, 3),
            AbandonmentAction::Ignore,
        );
        assert_eq!(
            abandonment_action(OwnershipKind::Self_, true, 99, 3),
            AbandonmentAction::Ignore,
        );
    }

    #[test]
    fn abandonment_differential_by_ownership() {
        // Positive control: identical disconnect stimulus and retry budget,
        // but ownership alone drives the decision.
        //   Owned  (retries exhausted) → SelfDestruct
        //   Peer                      → Ignore
        //   Self_                     → Ignore
        // The Owned outcome is distinct from both non-owned outcomes; Peer
        // and Self_ agree on Ignore (their shared branch in the protocol).
        let owned = abandonment_action(OwnershipKind::Owned, true, 5, 3);
        let peer = abandonment_action(OwnershipKind::Peer, true, 5, 3);
        let self_kind = abandonment_action(OwnershipKind::Self_, true, 5, 3);

        assert_eq!(owned, AbandonmentAction::SelfDestruct);
        assert_eq!(peer, AbandonmentAction::Ignore);
        assert_eq!(self_kind, AbandonmentAction::Ignore);

        // The Owned outcome differs from the non-owned outcomes.
        assert_ne!(owned, peer);
        assert_ne!(owned, self_kind);

        // And the same stimulus with retries remaining flips Owned to Retry
        // while leaving the non-owned kinds on Ignore — proving the retry
        // budget is scoped to Owned only.
        let owned_retry = abandonment_action(OwnershipKind::Owned, true, 1, 3);
        assert_eq!(owned_retry, AbandonmentAction::Retry);
        assert_eq!(
            abandonment_action(OwnershipKind::Peer, true, 1, 3),
            AbandonmentAction::Ignore,
        );
        assert_eq!(
            abandonment_action(OwnershipKind::Self_, true, 1, 3),
            AbandonmentAction::Ignore,
        );
    }
}
