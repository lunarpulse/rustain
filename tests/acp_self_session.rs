//! Story 14-7 (Task 2 foundations) — `NodeTree::register_self_session` and the
//! non-durable `Self` invariant.
//!
//! These tests pin the domain contracts the ACP server-mode adapter depends on
//! when it materializes an editor session as the first-ever `Self`-rooted node
//! in the tree (AC5). They drive the **real** `NodeTree` through its public
//! surface (`register_self_session`, `list`, `children_of`) and read ownership
//! off the snapshot DTO — never reaching into private node state.
//!
//! Contracts defended:
//! - One `register_self_session` yields exactly one top-level `Self_` node
//!   (ownership `Self_` ∧ parent root ∧ depth 1 ∧ count(Self_)==1). The
//!   fall-through-to-`Owned` mutant reddens the ownership arm.
//! - A second session is a sibling, not a child.
//! - The root sentinel and duplicate ids are rejected.
//! - `Self_` and `Owned` coexist in one tree (the new path is additive, not a
//!   rerouting of `register`).
//! - The wire/export ownership type (`WireOwnershipKind`, used by
//!   `NodeCheckpoint`) refuses a forged `"self_"` — so a `Self` session root is
//!   non-durable by construction and a wire checkpoint can never claim root
//!   (the R2-additivity trap).
//!
//! `origin == NodeOrigin::Interactive` (AC5 / DD2-B) is asserted via the
//! cfg-gated `NodeTree::origin_of` instrumentation seam; that test runs only
//! under `--features test-instrumentation`.

use std::collections::HashSet;

use rustain::domain::models::agent_node::AgentMetrics;
#[cfg(feature = "test-instrumentation")]
use rustain::domain::models::agent_node::NodeOrigin;
use rustain::domain::models::node_state::NodeState;
use rustain::domain::models::subagent_view::{OwnershipKind, WireOwnershipKind};
use rustain::domain::models::{AgentId, CapabilityTokenId, Op};
use rustain::infrastructure::subagent::{AgentHandle, MailboxBudget, NodeTree, RegistryEntry};
use tokio::sync::{mpsc, watch};

/// Build a minimal, hermetic `AgentHandle` mirroring the in-tree test helper.
/// `spawned_at` is fixed (non-zero) so the test never depends on wall-clock;
/// `register_self_session` overrides `depth` to 1 regardless of the input.
fn dummy_handle(agent_id: AgentId) -> AgentHandle {
    let (tx, _rx) = mpsc::channel::<Op>(1);
    let (status_tx, _status_rx) = watch::channel(NodeState::Created);
    let (_metrics_tx, metrics_rx) = watch::channel(AgentMetrics::default());
    AgentHandle {
        agent_id: agent_id.clone(),
        token: CapabilityTokenId::nil(),
        command_tx: tx,
        cancel_token: tokio_util::sync::CancellationToken::new(),
        depth: 0,
        subagent_type: String::from("acp-editor"),
        spawned_at: 1_700_000_000_000,
        status: status_tx,
        metrics: metrics_rx,
        isolated: false,
        mailbox_budget: MailboxBudget::new(),
    }
}

/// Count nodes whose ownership is the sealed `Self_` tier. Matching
/// `OwnershipKind::Self_(_)` from outside the crate is legal; constructing it
/// is not (the seal). This is the AC5 exact-triple's count arm.
fn count_self(entries: &[RegistryEntry]) -> usize {
    entries
        .iter()
        .filter(|e| matches!(e.ownership, OwnershipKind::Self_(_)))
        .count()
}

/// AC5 exact triple: one `register_self_session` materializes exactly one
/// top-level `Self_` node. Each assertion arm kills a distinct mutant:
/// fall-through-to-`Owned` (ownership), child registration (parent/depth), and a
/// no-op/non-inserting impl (count).
#[tokio::test]
async fn register_self_session_materializes_one_self_rooted_top_level_node() {
    let tree = NodeTree::new();
    let session = AgentId::new();

    tree.register_self_session(session.clone(), dummy_handle(session.clone()))
        .await
        .expect("register_self_session must accept a fresh non-root agent id");

    let entries = tree.list().await;
    let node = entries
        .iter()
        .find(|e| e.agent_id == session)
        .expect("the session node must appear in list()");

    assert!(
        matches!(node.ownership, OwnershipKind::Self_(_)),
        "ownership must be the sealed Self_ tier minted via self_root(), not Owned"
    );
    assert_eq!(
        node.parent_id,
        AgentId::root(),
        "a Self-rooted session has no parent node (parent_of maps to the root sentinel)"
    );
    assert_eq!(node.depth, 1, "the Self session root sits at depth 1");
    assert_eq!(
        count_self(&entries),
        1,
        "exactly one Self_ node after one session (a second phantom node would break the invariant)"
    );
}

/// AC5 sibling invariant: a second session registers as a sibling top-level
/// `Self_` node, never a child of the first. Kills a mutant that nests the
/// second session or reuses the first node.
#[tokio::test]
async fn second_self_session_is_a_sibling_not_a_child() {
    let tree = NodeTree::new();
    let a = AgentId::new();
    let b = AgentId::new();

    tree.register_self_session(a.clone(), dummy_handle(a.clone()))
        .await
        .unwrap();
    tree.register_self_session(b.clone(), dummy_handle(b.clone()))
        .await
        .unwrap();

    let entries = tree.list().await;
    let self_nodes: Vec<&RegistryEntry> = entries
        .iter()
        .filter(|e| matches!(e.ownership, OwnershipKind::Self_(_)))
        .collect();
    assert_eq!(self_nodes.len(), 2, "two sessions => two Self_ nodes");

    let self_ids: HashSet<&AgentId> = self_nodes.iter().map(|n| &n.agent_id).collect();
    let expected: HashSet<&AgentId> = [&a, &b].into_iter().collect();
    assert_eq!(
        self_ids, expected,
        "the two Self_ nodes are the two distinct sessions"
    );

    for n in &self_nodes {
        assert_eq!(
            n.parent_id,
            AgentId::root(),
            "each Self session is top-level"
        );
        assert_eq!(n.depth, 1, "each Self session is depth 1");
    }
    assert!(
        tree.children_of(&a).await.is_empty(),
        "the second session must NOT nest under the first (siblings, not parent/child)"
    );
}

/// The root sentinel must remain unregisterable — `register_self_session` is a
/// new sibling path, not a relaxation of the structural "root cannot be a node"
/// guarantee shared with `register`.
#[tokio::test]
async fn register_self_session_rejects_the_root_sentinel() {
    let tree = NodeTree::new();
    let root = AgentId::root();
    let result = tree
        .register_self_session(root.clone(), dummy_handle(root.clone()))
        .await;
    assert!(
        result.is_err(),
        "the root sentinel must not be registerable as a session node"
    );
}

/// Duplicate agent ids are rejected — each editor session is a distinct node.
#[tokio::test]
async fn register_self_session_rejects_duplicate_agent_id() {
    let tree = NodeTree::new();
    let s = AgentId::new();
    tree.register_self_session(s.clone(), dummy_handle(s.clone()))
        .await
        .unwrap();
    let result = tree
        .register_self_session(s.clone(), dummy_handle(s.clone()))
        .await;
    assert!(
        result.is_err(),
        "registering the same agent_id twice must be rejected"
    );
}

/// The new `Self`-rooting path is additive: a regular `Owned` subagent
/// (registered via `register`) and a `Self_` session coexist in one tree with
/// distinct ownership. Proves `register_self_session` is not silently rerouting
/// normal subagent registration, and that the two ownership tiers do not bleed
/// into each other.
#[tokio::test]
async fn self_session_coexists_with_owned_subagents() {
    let tree = NodeTree::new();
    let root = AgentId::root();
    let parent = AgentId::new();
    let child = AgentId::new();
    let session = AgentId::new();

    // Regular Owned subagent chain via the existing register() path.
    tree.register(parent.clone(), root.clone(), dummy_handle(parent.clone()))
        .await
        .unwrap();
    tree.register(child.clone(), parent.clone(), dummy_handle(child.clone()))
        .await
        .unwrap();
    // Self-rooted editor session via the new path.
    tree.register_self_session(session.clone(), dummy_handle(session.clone()))
        .await
        .unwrap();

    let entries = tree.list().await;
    assert_eq!(
        count_self(&entries),
        1,
        "exactly one Self_ node alongside Owned nodes"
    );
    let owned_count = entries
        .iter()
        .filter(|e| matches!(e.ownership, OwnershipKind::Owned))
        .count();
    assert_eq!(owned_count, 2, "the two Owned subagents keep their tier");

    let sess = entries.iter().find(|e| e.agent_id == session).unwrap();
    assert!(matches!(sess.ownership, OwnershipKind::Self_(_)));
    assert_eq!(sess.depth, 1);
    // The Owned child is nested under its parent; the Self session is not.
    let kid = entries.iter().find(|e| e.agent_id == child).unwrap();
    assert_eq!(kid.parent_id, parent);
    assert!(kid.depth > 1);
}

/// Non-durable `Self` invariant (AC5 / DD2, the R2-additivity trap): the
/// wire/export ownership type — the one `NodeCheckpoint` serializes — MUST
/// refuse a forged `"self_"`. If it ever accepted one, a crafted checkpoint
/// could claim the privileged root tier on resume. This is green today
/// (`WireOwnershipKind` has only `Owned`/`Peer`); it reddens the instant a
/// `Self_` variant is added to the wire type.
#[test]
fn wire_ownership_kind_rejects_forged_self_root_claim() {
    let err = serde_json::from_str::<WireOwnershipKind>("\"self_\"");
    assert!(
        err.is_err(),
        "WireOwnershipKind must not deserialize \"self_\": a serializable Self tier \
         would let a wire checkpoint forge root (non-durability invariant)"
    );
}

/// Belt-and-suspenders: the two legitimate wire variants still parse, so the
/// forged-`"self_"` rejection above is a real refusal of that token, not a
/// blanket deserialization failure.
#[test]
fn wire_ownership_kind_accepts_legitimate_tiers() {
    let owned: WireOwnershipKind =
        serde_json::from_str("\"owned\"").expect("\"owned\" is a valid wire tier");
    let peer: WireOwnershipKind =
        serde_json::from_str("\"peer\"").expect("\"peer\" is a valid wire tier");
    assert_eq!(owned, WireOwnershipKind::Owned);
    assert_eq!(peer, WireOwnershipKind::Peer);
}

/// AC5 (DD2-B): `register_self_session` stamps `NodeOrigin::Interactive` — an
/// editor turn is an interactive human turn transported over stdio, NOT
/// `Remote`, which stays reserved for R2's genuinely-remote principals. Origin
/// describes the submitter, not the transport. Requires the cfg-gated
/// `NodeTree::origin_of` instrumentation seam, so this test runs only under
/// `--features test-instrumentation`.
#[cfg(feature = "test-instrumentation")]
#[tokio::test]
async fn register_self_session_stamps_interactive_origin() {
    let tree = NodeTree::new();
    let session = AgentId::new();
    tree.register_self_session(session.clone(), dummy_handle(session.clone()))
        .await
        .unwrap();
    assert_eq!(
        tree.origin_of(&session).await,
        Some(NodeOrigin::Interactive),
        "an editor ACP session is an interactive human turn, not a Remote principal"
    );

    // Contrast: a node registered via the existing register() path keeps the
    // Subagent origin — the two insertion paths do not bleed origin metadata.
    let sub = AgentId::new();
    tree.register(sub.clone(), AgentId::root(), dummy_handle(sub.clone()))
        .await
        .unwrap();
    assert_eq!(
        tree.origin_of(&sub).await,
        Some(NodeOrigin::Subagent),
        "the regular register() path still stamps Subagent (Interactive is ACP-only)"
    );
}
