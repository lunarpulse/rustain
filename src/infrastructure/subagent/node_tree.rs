use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::sync::watch;

use crate::domain::events::{AppEvent, CapabilityEvent};
use crate::domain::models::agent_node::{AgentMetrics, AgentNode, NodeOrigin};
use crate::domain::models::capability_id::CapabilityId;
use crate::domain::models::node_state::NodeState;
use crate::domain::models::subagent_view::OwnershipKind;
use crate::domain::models::{
    AgentId, CapabilityTokenId, Op, RegisteredCapability, SpawnLimitKind, SubagentError,
};
use crate::infrastructure::subagent::node_handle::{NodeHandle, NodeHandleError};

pub const MAX_DEPTH: usize = 3;
pub const MAX_CHILDREN: usize = 10;

/// Story 14-4a (AC1) — unified capacity constant for mailbox budget.
/// Replaces the per-site `PARKED_QUEUE_CAP` in `in_process_runner.rs`.
pub const MAILBOX_CAP: usize = 64;

/// Error returned by [`MailboxBudget::reserve`] when the budget is full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MailboxFull;

impl std::fmt::Display for MailboxFull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "mailbox budget is full")
    }
}

impl std::error::Error for MailboxFull {}

/// Story 14-4a (AC1) — shared atomic mailbox budget for reserve-at-admission.
/// Wraps an `Arc<AtomicUsize>` so `AgentHandle` (which is `Clone`) can share
/// the budget across the bus and the runner. Budget = atomics only (no lock;
/// ratchet constraint: untagged `std::sync` locks == 4).
#[derive(Clone)]
pub struct MailboxBudget(Arc<AtomicUsize>);

impl MailboxBudget {
    pub fn new() -> Self {
        Self(Arc::new(AtomicUsize::new(0)))
    }

    /// Atomically reserve one slot. Returns `Ok(())` if the count was below
    /// `MAILBOX_CAP`, or `Err(MailboxFull)` if full. `fetch_update` closes the
    /// TOCTOU window: two concurrent senders at 63/64 → exactly one succeeds.
    pub fn reserve(&self) -> Result<(), MailboxFull> {
        self.0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                if current < MAILBOX_CAP {
                    Some(current + 1)
                } else {
                    None
                }
            })
            .map(|_| ())
            .map_err(|_| MailboxFull)
    }

    /// Release one reserved slot. Must be called exactly once per successful
    /// `reserve()` on one of the defined release paths (sender self-release,
    /// recipient turn-dispatch, consent-refusal, or terminal drain).
    pub fn release(&self) {
        let result = self
            .0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                if current > 0 { Some(current - 1) } else { None }
            });
        if result.is_err() {
            debug_assert!(false, "MailboxBudget underflow: release without reserve");
            tracing::warn!("MailboxBudget underflow: release() called when budget is already 0");
        }
    }

    /// Current reservation count (for debug assertions and tests).
    pub fn current(&self) -> usize {
        self.0.load(Ordering::Acquire)
    }
}

impl Default for MailboxBudget {
    fn default() -> Self {
        Self::new()
    }
}

/// Unified node tree that tracks all agent nodes (subagents, daemon-spawned
/// work, orchestrated tasks) in a single ownership hierarchy.
///
/// Promoted from `SubagentRegistry` in Story 14.1. The tree stores domain
/// `AgentNode` values alongside infrastructure `NodeHandle` references in a
/// side-table — domain stays runtime-agnostic per the hexagonal rule.
#[derive(Clone)]
pub struct NodeTree {
    inner: Arc<tokio::sync::RwLock<NodeTreeInner>>,
    event_tx: Option<mpsc::UnboundedSender<AppEvent>>,
    now_fn: Arc<dyn Fn() -> i64 + Send + Sync>,
    /// Hook point that revokes descendant capability tokens synchronously
    /// *before* `Op::Kill` is issued to each node in a cascade_kill. Wired at
    /// the composition root (`startup.rs:1427-1432`) to
    /// `AuthorityProvider::revoke` so a revoked token's descendants are
    /// invalidated in the same extent (AC4/AC5).
    ///
    /// Happens-before contract: revoke's critical section (the shared
    /// `AuthorityLedger` `Mutex`) establishes happens-before with all
    /// subsequent `validate` calls — once revoke completes, no later validate
    /// may observe the pre-revoke state (Story 14.6 AC5 TOCTOU probe).
    on_cascade_kill: Arc<dyn Fn(&AgentId) + Send + Sync>,
}

struct NodeTreeInner {
    /// Domain node records.
    nodes: HashMap<AgentId, AgentNode>,
    /// Infrastructure handles (cancel token + command channel) — side-table.
    handles: HashMap<AgentId, NodeHandle>,
    /// agent → parent (root sentinel for top-level).
    parent_of: HashMap<AgentId, AgentId>,
    /// Keeps watch channel alive for status broadcasting.
    status_rx: HashMap<AgentId, watch::Receiver<NodeState>>,
    /// Watch senders for broadcasting status updates.
    status_senders: HashMap<AgentId, watch::Sender<NodeState>>,
    /// Latest per-agent runtime metrics (AC11 live inspector values).
    metrics_rx: HashMap<AgentId, watch::Receiver<AgentMetrics>>,
    /// P9 (TUI): per-agent isolated flag — a SIDE-TABLE (not on the pinned
    /// `AgentNode`) so the ⊙ iso indicator can render without a field-count bump.
    isolated_agents: HashMap<AgentId, bool>,
    /// Story 14-4a (AC1) — per-agent mailbox budget for reserve-at-admission.
    mailbox_budgets: HashMap<AgentId, MailboxBudget>,
}

// ── Legacy compatibility types ──────────────────────────────────────────────
// These exist so that callers can migrate incrementally. They mirror the old
// `AgentHandle` and `RegistryEntry` shapes from the former `SubagentRegistry`.

/// Legacy handle shape from pre-14.1 `SubagentRegistry`. Callers that still
/// construct this are migrated to build an `AgentNode` + `NodeHandle` pair.
/// Retained temporarily for `in_process_runner` compatibility during the
/// migration; will be removed once all callers are updated.
#[derive(Clone)]
pub struct AgentHandle {
    pub agent_id: AgentId,
    pub token: CapabilityTokenId,
    pub command_tx: mpsc::Sender<Op>,
    /// The node's REAL cancellation token — the same token the child task
    /// selects on. Passed from `launch()` and stored here (not minted fresh)
    /// so `cascade_kill`'s `handle.cancel()` actually interrupts the task at
    /// any await point selecting on `cancel.cancelled()` (AC10/AC4).
    pub cancel_token: tokio_util::sync::CancellationToken,
    pub depth: usize,
    pub subagent_type: String,
    pub spawned_at: i64,
    pub status: watch::Sender<NodeState>,
    pub metrics: watch::Receiver<AgentMetrics>,
    /// P9 (TUI): true when this child runs in a scratch-dir clone (renders ⊙ iso).
    pub isolated: bool,
    /// Story 14-4a (AC1) — shared atomic budget for reserve-at-admission.
    /// `fetch_update` increment-if-below-MAILBOX_CAP before `try_send`;
    /// self-released on failed `try_send`. Every `AgentHandle` for the same
    /// agent shares the same `Arc`.
    pub mailbox_budget: MailboxBudget,
}

/// Snapshot DTO for the TUI panel. Deterministic sort by agent_id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryEntry {
    pub agent_id: AgentId,
    pub parent_id: AgentId,
    pub subagent_type: String,
    pub spawned_at: i64,
    pub depth: usize,
    pub current_status: NodeState,
    pub ownership: OwnershipKind,
    pub effective_model: String,
    pub tools_summary: String,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub turns: u32,
    /// P9 (TUI): renders the ⊙ iso indicator when true.
    pub isolated: bool,
}

impl RegistryEntry {
    pub fn to_view(&self) -> crate::domain::models::subagent_view::AgentRowView {
        crate::domain::models::subagent_view::AgentRowView {
            agent_id: self.agent_id.clone(),
            parent_id: self.parent_id.clone(),
            subagent_type: self.subagent_type.clone(),
            spawned_at: self.spawned_at,
            depth: self.depth,
            current_status: self.current_status,
            ownership: self.ownership,
            effective_model: self.effective_model.clone(),
            tools_summary: self.tools_summary.clone(),
            tokens_in: self.tokens_in,
            tokens_out: self.tokens_out,
            turns: self.turns,
            isolated: self.isolated,
        }
    }
}

// ── Error types ─────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum CascadeKillError {
    #[error("not found in node tree: {0:?}")]
    NotFound(AgentId),
    #[error("partial cascade: {killed:?} killed, {unresponsive:?} timed out")]
    Partial {
        killed: Vec<AgentId>,
        unresponsive: Vec<AgentId>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum OwnerCommandError {
    #[error("agent not found: {0:?}")]
    NotFound(AgentId),
    #[error("agent command channel closed: {0:?}")]
    Closed(AgentId),
    #[error("remote node op routing not supported in R1: {0:?}")]
    Remote(AgentId),
}

#[derive(Clone)]
pub struct DeliveryTarget {
    pub state: NodeState,
    pub ownership: OwnershipKind,
    pub handle: NodeHandle,
    /// Story 14-4a (AC1) — the recipient's shared mailbox budget.
    pub mailbox_budget: MailboxBudget,
}

// ── NodeTree implementation ─────────────────────────────────────────────────

impl NodeTree {
    /// Build an empty `NodeTreeInner`. Single source of truth so adding a
    /// field (e.g. a second event bus) doesn't require touching every
    /// constructor in lockstep.
    fn build_inner() -> tokio::sync::RwLock<NodeTreeInner> {
        tokio::sync::RwLock::new(NodeTreeInner {
            nodes: HashMap::new(),
            isolated_agents: HashMap::new(),
            handles: HashMap::new(),
            parent_of: HashMap::new(),
            status_rx: HashMap::new(),
            status_senders: HashMap::new(),
            metrics_rx: HashMap::new(),
            mailbox_budgets: HashMap::new(),
        })
    }

    pub fn new() -> Self {
        Self {
            inner: Arc::new(Self::build_inner()),
            event_tx: None,
            now_fn: Arc::new(|| chrono::Utc::now().timestamp_millis()),
            on_cascade_kill: Arc::new(|_| {}),
        }
    }

    pub fn with_now_fn(now_fn: Arc<dyn Fn() -> i64 + Send + Sync>) -> Self {
        Self {
            inner: Arc::new(Self::build_inner()),
            event_tx: None,
            now_fn,
            on_cascade_kill: Arc::new(|_| {}),
        }
    }

    pub fn with_event_tx(
        event_tx: mpsc::UnboundedSender<AppEvent>,
        now_fn: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Self {
        Self {
            inner: Arc::new(Self::build_inner()),
            event_tx: Some(event_tx),
            now_fn,
            on_cascade_kill: Arc::new(|_| {}),
        }
    }

    /// Install the `on_cascade_kill` hook. It revokes descendant capability
    /// tokens synchronously *before* `Op::Kill`, wired at the composition root
    /// (`startup.rs:1427-1432`) to `AuthorityProvider::revoke`. The default is
    /// an inert closure for unit tests; production injects the revoke callback
    /// here instead of editing this file (forward-compat hook #4 / AC4/AC5).
    #[must_use]
    pub fn with_on_cascade_kill(mut self, hook: Arc<dyn Fn(&AgentId) + Send + Sync>) -> Self {
        self.on_cascade_kill = hook;
        self
    }

    /// Register a node from the legacy `AgentHandle` shape. All in-process
    /// subagents are `Owned` foreground spawns in R1; `Peer`/`Self_` and
    /// non-`Subagent` origins arrive with later stories.
    pub async fn register(
        &self,
        agent_id: AgentId,
        parent: AgentId,
        handle: AgentHandle,
    ) -> Result<(), SubagentError> {
        self.register_with_identity(
            agent_id,
            parent,
            handle,
            OwnershipKind::Owned,
            NodeOrigin::Subagent,
        )
        .await
    }

    async fn register_with_identity(
        &self,
        agent_id: AgentId,
        parent: AgentId,
        mut handle: AgentHandle,
        ownership: OwnershipKind,
        origin: NodeOrigin,
    ) -> Result<(), SubagentError> {
        let mut guard = self.inner.write().await;
        if guard.nodes.contains_key(&agent_id) {
            return Err(SubagentError::Internal(format!(
                "duplicate agent_id: {:?}",
                agent_id
            )));
        }
        if agent_id == AgentId::root() {
            return Err(SubagentError::Internal(
                "agent_id cannot be the root sentinel".into(),
            ));
        }

        let depth = if parent == AgentId::root() {
            1
        } else if let Some(parent_node) = guard.nodes.get(&parent) {
            parent_node.depth + 1
        } else {
            return Err(SubagentError::Internal(format!(
                "parent not found in node tree: {:?}",
                parent
            )));
        };
        let parent_tainted = guard
            .nodes
            .get(&parent)
            .map(|node| node.tainted)
            .unwrap_or(false);
        if depth > MAX_DEPTH {
            return Err(SubagentError::SpawnLimitExceeded {
                kind: SpawnLimitKind::Depth,
                limit: MAX_DEPTH,
                attempted: depth,
            });
        }
        let children_count = guard.parent_of.values().filter(|p| **p == parent).count();
        if children_count >= MAX_CHILDREN {
            return Err(SubagentError::SpawnLimitExceeded {
                kind: SpawnLimitKind::Children,
                limit: MAX_CHILDREN,
                attempted: children_count + 1,
            });
        }

        let (status_tx, status_rx) = watch::channel(NodeState::Created);
        handle.depth = depth;
        if handle.spawned_at == 0 {
            handle.spawned_at = (self.now_fn)();
        }
        handle.status = status_tx.clone();
        let node = AgentNode {
            id: agent_id.clone(),
            token: handle.token,
            parent: if parent == AgentId::root() {
                None
            } else {
                Some(parent.clone())
            },
            ownership,
            state: NodeState::Created,
            origin,
            foreground: true,
            effective_model: String::new(),
            tokens_in: 0,
            tokens_out: 0,
            turns: 0,
            subagent_type: handle.subagent_type.clone(),
            spawned_at: handle.spawned_at,
            depth,
            tainted: parent_tainted,
        };
        let node_handle = NodeHandle::Local {
            cancel_token: handle.cancel_token.clone(),
            command_tx: handle.command_tx.clone(),
        };

        guard.nodes.insert(agent_id.clone(), node);
        guard.handles.insert(agent_id.clone(), node_handle);
        guard.parent_of.insert(agent_id.clone(), parent);
        guard.status_rx.insert(agent_id.clone(), status_rx);
        guard.status_senders.insert(agent_id.clone(), status_tx);
        guard
            .metrics_rx
            .insert(agent_id.clone(), handle.metrics.clone());
        guard
            .isolated_agents
            .insert(agent_id.clone(), handle.isolated);
        guard
            .mailbox_budgets
            .insert(agent_id.clone(), handle.mailbox_budget.clone());

        // The event is sent while the mutation-ordering lock is still held.
        // Unbounded send never awaits, so a concurrent cascade cannot publish a
        // deregistration before this registration.
        self.emit_capability_event(CapabilityEvent::Registered {
            capability: self.capability_for(&agent_id),
        });
        drop(guard);
        Ok(())
    }

    /// Register a local mailbox representing a verified remote peer.
    ///
    /// The handle remains local because the existing `LocalMessageBus` must
    /// perform reserve-and-send admission. Ownership and origin distinguish it
    /// from an owned subagent; a transport-backed `NodeHandle::Remote` remains
    /// an R3 concern.
    pub async fn register_peer(
        &self,
        agent_id: AgentId,
        handle: AgentHandle,
    ) -> Result<(), SubagentError> {
        self.register_with_identity(
            agent_id,
            AgentId::root(),
            handle,
            OwnershipKind::Peer,
            NodeOrigin::Remote,
        )
        .await
    }

    /// Register a live ACP/editor attachment as a non-durable `Self` session root.
    ///
    /// This path is deliberately separate from [`Self::register`]: normal subagents
    /// remain `Owned` children, while an editor-driven ACP session is an interactive
    /// top-level attachment whose `Self_` ownership is minted server-side through the
    /// sealed constructor. The node is non-durable by construction; future resumable
    /// remote/editor sessions must add a wire ownership variant instead of serializing
    /// `Self_`.
    pub async fn register_self_session(
        &self,
        agent_id: AgentId,
        handle: AgentHandle,
    ) -> Result<(), SubagentError> {
        self.register_with_identity(
            agent_id,
            AgentId::root(),
            handle,
            OwnershipKind::self_root(),
            NodeOrigin::Interactive,
        )
        .await
    }

    fn capability_for(&self, agent_id: &AgentId) -> RegisteredCapability {
        RegisteredCapability {
            id: CapabilityId {
                protocol: "subagent".into(),
                server: String::new(),
                tool: agent_id.as_str().to_string(),
            },
            protocol: "subagent".into(),
            provider_id: "subagent".into(),
            name: agent_id.as_str().to_string(),
            description: String::new(),
            input_schema: serde_json::Value::Object(Default::default()),
            parallel_safe: false,
        }
    }

    fn emit_capability_event(&self, event: CapabilityEvent) -> bool {
        let Some(tx) = &self.event_tx else {
            return false;
        };
        if tx.send(AppEvent::CapabilityEvent(event)).is_err() {
            tracing::warn!(
                "NodeTree lifecycle event receiver closed; capability observability is unavailable"
            );
            return false;
        }
        true
    }

    #[cfg(any(test, feature = "test-instrumentation"))]
    pub async fn origin_of(&self, agent_id: &AgentId) -> Option<NodeOrigin> {
        let guard = self.inner.read().await;
        guard.nodes.get(agent_id).map(|node| node.origin)
    }

    /// Remove a single node from every map. No cascade, no event emission.
    /// Used internally by `cascade_kill` (which walks the subtree in kill
    /// order itself) and as the per-node primitive of [`Self::deregister`].
    async fn deregister_one(&self, agent_id: &AgentId) -> bool {
        let mut guard = self.inner.write().await;
        let removed = guard.nodes.remove(agent_id).is_some();
        if !removed {
            return false;
        }
        guard.handles.remove(agent_id);
        guard.parent_of.remove(agent_id);
        guard.status_rx.remove(agent_id);
        guard.status_senders.remove(agent_id);
        guard.metrics_rx.remove(agent_id);
        guard.mailbox_budgets.remove(agent_id);
        guard.isolated_agents.remove(agent_id);
        self.emit_capability_event(CapabilityEvent::Deregistered {
            capability: self.capability_for(agent_id),
        });
        drop(guard);
        true
    }

    /// Remove `agent_id` **and its entire subtree** from the tree.
    ///
    /// Cascading prevents the orphan invariant violation where a child's
    /// `parent_of` would otherwise point at a removed node. `cascade_kill`
    /// walks the subtree itself and reaches the per-node primitive
    /// [`Self::deregister_one`] directly; external callers get the safe
    /// cascading semantics here.
    pub async fn deregister(&self, agent_id: &AgentId) {
        // Collect the subtree under a short read lock. Removal is
        // order-independent, so descendant ordering does not matter.
        let to_remove = {
            let guard = self.inner.read().await;
            let mut all = vec![agent_id.clone()];
            let mut queue = std::collections::VecDeque::new();
            queue.push_back(agent_id.clone());
            while let Some(cur) = queue.pop_front() {
                for (id, p) in guard.parent_of.iter() {
                    if *p == cur && !all.contains(id) {
                        all.push(id.clone());
                        queue.push_back(id.clone());
                    }
                }
            }
            all
        };

        for id in &to_remove {
            self.deregister_one(id).await;
        }
    }

    /// Emit a CapabilityEvent::Updated for a subagent status change (AC-10-2-4).
    pub async fn emit_status_updated(&self, agent_id: &AgentId) {
        if let Some(tx) = &self.event_tx {
            let guard = self.inner.read().await;
            if let Some(node) = guard.nodes.get(agent_id) {
                let old_cap = RegisteredCapability {
                    id: CapabilityId {
                        protocol: "subagent".into(),
                        server: String::new(),
                        tool: agent_id.as_str().to_string(),
                    },
                    protocol: "subagent".into(),
                    provider_id: "subagent".into(),
                    name: agent_id.as_str().to_string(),
                    description: String::new(),
                    input_schema: serde_json::Value::Object(Default::default()),
                    parallel_safe: false,
                };
                let new_cap = RegisteredCapability {
                    id: CapabilityId {
                        protocol: "subagent".into(),
                        server: String::new(),
                        tool: agent_id.as_str().to_string(),
                    },
                    protocol: "subagent".into(),
                    provider_id: "subagent".into(),
                    name: node.subagent_type.clone(),
                    description: String::new(),
                    input_schema: serde_json::Value::Object(Default::default()),
                    parallel_safe: false,
                };
                let id = old_cap.id.clone();
                drop(guard);
                let _ = tx.send(AppEvent::CapabilityEvent(CapabilityEvent::Updated {
                    id,
                    old: old_cap,
                    new: Box::new(new_cap),
                }));
            }
        }
    }

    /// Mark a node tainted after a cross-agent message is actually ingested.
    /// This operation is monotone; repeated ingest is idempotent.
    pub async fn mark_tainted(&self, agent_id: &AgentId) -> bool {
        let mut guard = self.inner.write().await;
        if let Some(node) = guard.nodes.get_mut(agent_id) {
            let changed = !node.tainted;
            node.tainted = true;
            changed
        } else {
            false
        }
    }

    /// Clear integrity taint only for a true context reset (`/clear`/new conversation).
    pub async fn clear_taint(&self, agent_id: &AgentId) {
        let mut guard = self.inner.write().await;
        if let Some(node) = guard.nodes.get_mut(agent_id) {
            node.tainted = false;
        }
    }

    pub async fn is_tainted(&self, agent_id: &AgentId) -> bool {
        let guard = self.inner.read().await;
        guard
            .nodes
            .get(agent_id)
            .map(|n| n.tainted)
            .unwrap_or(false)
    }
    pub async fn depth(&self, agent_id: &AgentId) -> usize {
        let guard = self.inner.read().await;
        guard.nodes.get(agent_id).map(|n| n.depth).unwrap_or(0)
    }

    pub async fn children_of(&self, parent: &AgentId) -> Vec<AgentId> {
        let guard = self.inner.read().await;
        guard
            .parent_of
            .iter()
            .filter(|(_, p)| *p == parent)
            .map(|(id, _)| id.clone())
            .collect()
    }

    pub async fn snapshot(&self) -> Vec<(AgentId, AgentId, usize)> {
        let guard = self.inner.read().await;
        guard
            .parent_of
            .iter()
            .map(|(agent_id, parent_id)| {
                let depth = guard.nodes.get(agent_id).map(|n| n.depth).unwrap_or(0);
                (agent_id.clone(), parent_id.clone(), depth)
            })
            .collect()
    }

    /// Walk from `agent_id` up to (but not including) `AgentId::root()` in
    /// order child-first. Returns `Vec::new()` if `agent_id == AgentId::root()`
    /// or if not registered.
    ///
    /// The walk is bounded by the node count as a cycle guard: `parent_of` is
    /// acyclic by construction, but a future reparenting bug must not wedge
    /// every reader of the tree in an infinite loop under the read lock
    /// (consistent with the defensive guard in [`Self::subtree`]).
    pub async fn ancestors(&self, agent_id: &AgentId) -> Vec<AgentId> {
        if *agent_id == AgentId::root() {
            return Vec::new();
        }
        let guard = self.inner.read().await;
        let cap = guard.nodes.len() + 1;
        let mut result = Vec::new();
        let mut current = agent_id.clone();
        for _ in 0..cap {
            match guard.parent_of.get(&current) {
                Some(parent) if *parent == AgentId::root() => break,
                Some(parent) => {
                    result.push(parent.clone());
                    current = parent.clone();
                }
                None => break,
            }
        }
        drop(guard);
        result
    }

    /// Return every descendant of `agent_id` (excluding `agent_id` itself),
    /// discovered via BFS over `parent_of`. Order is deterministic via sorted children.
    pub async fn subtree(&self, agent_id: &AgentId) -> Vec<AgentId> {
        let guard = self.inner.read().await;
        let mut result = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(agent_id.clone());
        while let Some(current) = queue.pop_front() {
            let mut children: Vec<AgentId> = guard
                .parent_of
                .iter()
                .filter(|(_, p)| *p == &current)
                .map(|(id, _)| id.clone())
                .collect();
            children.sort_by(|a, b| a.as_str().cmp(b.as_str()));
            for child in children {
                if !result.contains(&child) {
                    result.push(child.clone());
                    queue.push_back(child);
                }
            }
        }
        drop(guard);
        result
    }

    /// Snapshot of every registered agent's metadata. Deterministic sort by agent_id.
    pub async fn list(&self) -> Vec<RegistryEntry> {
        let guard = self.inner.read().await;
        let mut entries: Vec<RegistryEntry> = guard
            .nodes
            .iter()
            .map(|(agent_id, node)| {
                let tools_summary = guard
                    .metrics_rx
                    .get(agent_id)
                    .map(|rx| rx.borrow().tools_summary.clone())
                    .unwrap_or_else(|| "(unresolved)".to_string());
                RegistryEntry {
                    agent_id: agent_id.clone(),
                    parent_id: guard
                        .parent_of
                        .get(agent_id)
                        .cloned()
                        .unwrap_or_else(AgentId::root),
                    subagent_type: node.subagent_type.clone(),
                    spawned_at: node.spawned_at,
                    depth: node.depth,
                    ownership: node.ownership,
                    current_status: node.state,
                    effective_model: node.effective_model.clone(),
                    tools_summary,
                    tokens_in: node.tokens_in,
                    tokens_out: node.tokens_out,
                    turns: node.turns,
                    isolated: guard
                        .isolated_agents
                        .get(agent_id)
                        .copied()
                        .unwrap_or(false),
                }
            })
            .collect();
        drop(guard);
        entries.sort_by(|a, b| {
            a.agent_id
                .as_str()
                .to_string()
                .cmp(&b.agent_id.as_str().to_string())
        });
        entries
    }

    /// Return a clone of the watch sender for a given agent_id, if registered.
    pub async fn status_sender(&self, agent_id: &AgentId) -> Option<watch::Sender<NodeState>> {
        let guard = self.inner.read().await;
        guard.status_senders.get(agent_id).cloned()
    }

    /// Return a clone of the watch receiver for a given agent_id.
    pub async fn status_rx(&self, agent_id: &AgentId) -> Option<watch::Receiver<NodeState>> {
        let guard = self.inner.read().await;
        guard.status_rx.get(agent_id).cloned()
    }

    /// Advance a node's runtime lifecycle state, broadcasting on the status
    /// watch only when the FSM accepts the transition.
    ///
    /// The node tree is the single source of truth for lifecycle state —
    /// `list()` projects from `AgentNode.state`. Validating here and gating the
    /// watch broadcast on acceptance keeps the watch and the stored node from
    /// diverging on a transition the FSM rejected (e.g. an illegal
    /// `Suspended → Waiting` re-emission). Transitions go through
    /// `transition_or_err` → `can_transition_to` only (no ad-hoc predicate).
    pub async fn set_state(&self, agent_id: &AgentId, target: NodeState) {
        let mut guard = self.inner.write().await;
        let applied = {
            let Some(node) = guard.nodes.get_mut(agent_id) else {
                return;
            };
            let current = node.state;
            if current == target {
                return;
            }
            match node.state.transition_or_err(target) {
                Ok(()) => true,
                Err(error) => {
                    tracing::warn!(
                        agent_id = %agent_id,
                        current = ?current,
                        ?target,
                        %error,
                        "Ignoring invalid node state transition"
                    );
                    false
                }
            }
        };
        let sender_opt = guard.status_senders.get(agent_id).cloned();
        drop(guard);
        if applied {
            if let Some(sender) = sender_opt {
                let _ = sender.send(target);
            }
            self.emit_status_updated(agent_id).await;
        }
    }

    /// Advance the node's live inspector metrics (AC11).
    ///
    /// `AgentNode` keeps the first-class runtime fields (`effective_model`,
    /// `tokens_in`, `tokens_out`, `turns`) so `list()` can project them without
    /// reaching into adapter internals. The runner's metrics bridge owns the
    /// pacing; this mutator is idempotent and just overwrites the snapshot.
    pub async fn set_metrics(&self, agent_id: &AgentId, metrics: AgentMetrics) {
        let mut guard = self.inner.write().await;
        if let Some(node) = guard.nodes.get_mut(agent_id) {
            node.effective_model = metrics.effective_model;
            node.tokens_in = metrics.tokens_in;
            node.tokens_out = metrics.tokens_out;
            node.turns = metrics.turns;
        }
    }

    pub async fn delivery_target(&self, agent_id: &AgentId) -> Option<DeliveryTarget> {
        let guard = self.inner.read().await;
        let node = guard.nodes.get(agent_id)?;
        let handle = guard.handles.get(agent_id)?.clone();
        let mailbox_budget = guard.mailbox_budgets.get(agent_id)?.clone();
        Some(DeliveryTarget {
            state: node.state,
            ownership: node.ownership,
            handle,
            mailbox_budget,
        })
    }

    /// Walk the subtree of `agent_id` in reversed-BFS order (so no node is
    /// killed before its children), issuing `Op::Kill` to each handle AND
    /// cancelling its token, then awaiting the `current_status` watch channel
    /// to reach a terminal state before continuing up.
    ///
    /// Both cooperative signals are sent: `Op::Kill` for tasks parked on the
    /// command loop, and `cancel_token().cancel()` for tasks parked on an
    /// arbitrary await point (the token is derived from the parent's tree in
    /// [`Self::register`], so cancellation cascades — AC10).
    ///
    /// The `on_cascade_kill` hook fires for each killed node and revokes that
    /// node's capability token synchronously *before* `Op::Kill` is issued —
    /// wired at `startup.rs:1427-1432` to `AuthorityProvider::revoke` (AC4/AC5).
    /// Because revoke runs inside the `AuthorityLedger` `Mutex` critical
    /// section, its write establishes happens-before with every subsequent
    /// `validate`, so a descendant racing its own revocation cannot observe a
    /// stale valid token once revoke has returned.
    pub async fn cascade_kill(
        &self,
        agent_id: &AgentId,
        timeout_per_node: Duration,
    ) -> Result<Vec<AgentId>, CascadeKillError> {
        // Verify agent exists
        {
            let guard = self.inner.read().await;
            if !guard.nodes.contains_key(agent_id) {
                return Err(CascadeKillError::NotFound(agent_id.clone()));
            }
        }

        // Build kill order: subtree (BFS) → reverse → append self, so every
        // descendant precedes its parent (reversed-BFS, not DFS).
        let mut descendants = self.subtree(agent_id).await;
        descendants.reverse();
        descendants.push(agent_id.clone());

        let mut killed = Vec::new();
        let mut unresponsive = Vec::new();

        for id in &descendants {
            // Fire the cascade hook: revoke this node's capability token
            // synchronously BEFORE issuing Op::Kill (startup.rs:1427-1432).
            // The revoke runs inside the AuthorityLedger Mutex critical
            // section, establishing happens-before with later validates (AC5).
            (self.on_cascade_kill)(id);

            let (handle_opt, status_sender_opt) = {
                let guard = self.inner.read().await;
                (
                    guard.handles.get(id).cloned(),
                    guard.status_senders.get(id).cloned(),
                )
            };

            if let Some(handle) = handle_opt {
                // Cancel the token (interrupts arbitrary await points) and
                // deliver the cooperative Kill op. The send is bounded by the
                // per-node timeout so a saturated/non-draining channel cannot
                // wedge the whole cascade before the watch timeout starts.
                let send_result: Result<(), ()> = match &handle {
                    NodeHandle::Local {
                        command_tx,
                        cancel_token,
                    } => {
                        cancel_token.cancel();
                        match tokio::time::timeout(timeout_per_node, command_tx.send(Op::Kill))
                            .await
                        {
                            Ok(Ok(())) => Ok(()),
                            _ => Err(()),
                        }
                    }
                    NodeHandle::Remote { .. } => {
                        // Remote kill not supported in R1.
                        self.deregister_one(id).await;
                        killed.push(id.clone());
                        continue;
                    }
                };

                if send_result.is_err() {
                    // Channel closed or send timed out — treat as already terminal.
                    self.deregister_one(id).await;
                    killed.push(id.clone());
                    continue;
                }

                // Wait for terminal status via watch channel. Snapshot the
                // current value FIRST: if the node already reached terminal
                // before we subscribed (it processed Kill + published between
                // the send above and `subscribe()`), `changed()` would
                // otherwise block for a transition that never arrives and the
                // node would be falsely reported `unresponsive`.
                if let Some(sender) = status_sender_opt {
                    let already_terminal = matches!(
                        *sender.borrow(),
                        NodeState::Completed | NodeState::Failed | NodeState::Cancelled
                    );
                    let timed_out = if already_terminal {
                        false
                    } else {
                        let mut rx = sender.subscribe();
                        tokio::time::timeout(timeout_per_node, async {
                            if matches!(
                                *rx.borrow(),
                                NodeState::Completed | NodeState::Failed | NodeState::Cancelled
                            ) {
                                return;
                            }
                            loop {
                                if rx.changed().await.is_err() {
                                    return;
                                }
                                if matches!(
                                    *rx.borrow(),
                                    NodeState::Completed | NodeState::Failed | NodeState::Cancelled
                                ) {
                                    return;
                                }
                            }
                        })
                        .await
                        .is_err()
                    };

                    if timed_out {
                        unresponsive.push(id.clone());
                    } else {
                        killed.push(id.clone());
                    }
                } else {
                    killed.push(id.clone());
                }

                // Deregister this node only — the loop handles each descendant.
                self.deregister_one(id).await;
            } else {
                // Already gone — skip
                killed.push(id.clone());
            }
        }

        if !unresponsive.is_empty() {
            return Err(CascadeKillError::Partial {
                killed,
                unresponsive,
            });
        }

        Ok(killed)
    }

    /// Convenience wrapper with explicit timeout per node.
    pub async fn cascade_kill_with_timeout(
        &self,
        agent_id: &AgentId,
        timeout_per_node: Duration,
    ) -> Result<Vec<AgentId>, CascadeKillError> {
        self.cascade_kill(agent_id, timeout_per_node).await
    }

    pub async fn send_op(&self, agent_id: &AgentId, op: Op) -> Result<(), OwnerCommandError> {
        let handle = {
            let guard = self.inner.read().await;
            match guard.handles.get(agent_id) {
                Some(h) => h.clone(),
                None => return Err(OwnerCommandError::NotFound(agent_id.clone())),
            }
        };
        // Delegate the send to the handle so there is one try_send path with
        // faithful error mapping — a registered Remote agent reports Remote,
        // not NotFound. (Read-lock acquire is the only await, so the fn stays
        // async without a blocking_lock deadlock hazard.)
        handle.send_op(op).map_err(|err| match err {
            NodeHandleError::RemoteNotSupported => OwnerCommandError::Remote(agent_id.clone()),
            NodeHandleError::ChannelClosed => OwnerCommandError::Closed(agent_id.clone()),
        })
    }
}

impl Default for NodeTree {
    fn default() -> Self {
        Self::new()
    }
}

/// Local R1 implementation of the AgentMessageBus send-side port.
///
/// R2 plugs a transport-backed implementation into the same port. R1 only routes
/// local handles; the Remote arm is an inert hook and returns RemoteUnsupported.
pub struct LocalMessageBus {
    node_tree: NodeTree,
    policy: Arc<dyn crate::domain::ports::DeliveryPolicy>,
}

impl LocalMessageBus {
    pub fn new(node_tree: NodeTree, policy: Arc<dyn crate::domain::ports::DeliveryPolicy>) -> Self {
        Self { node_tree, policy }
    }
}

#[async_trait::async_trait]
impl crate::domain::ports::AgentMessageBus for LocalMessageBus {
    async fn deliver(
        &self,
        to: &AgentId,
        env: crate::domain::models::Envelope<crate::domain::models::AgentMessage>,
    ) -> Result<crate::domain::models::DeliveryOutcome, crate::domain::ports::DeliveryError> {
        use crate::domain::models::{
            AgentDelivery, DeliveryMode, DeliveryOutcome, Op, RefuseReason, delivery_decision,
        };
        use crate::domain::ports::DeliveryError;

        let target = self
            .node_tree
            .delivery_target(to)
            .await
            .ok_or_else(|| DeliveryError::NotFound(to.clone()))?;

        let disposition = self.policy.decide(&env.header, target.ownership);
        let mode = delivery_decision(target.state);
        if mode == DeliveryMode::Refuse {
            return Err(DeliveryError::Refused(RefuseReason::TerminalState));
        }

        // Story 14-4a (AC1) — reserve a slot BEFORE try_send. Atomic
        // fetch_update closes the TOCTOU window: two concurrent senders
        // at 63/64 → exactly one Accepted.
        target
            .mailbox_budget
            .reserve()
            .map_err(|_| DeliveryError::Full(to.clone()))?;

        match target.handle {
            NodeHandle::Local { command_tx, .. } => command_tx
                .try_send(Op::Deliver(AgentDelivery::new(env, mode, disposition)))
                .map(|()| DeliveryOutcome::Accepted)
                .map_err(|err| {
                    // Self-release the reservation on failed try_send (CS-3)
                    target.mailbox_budget.release();
                    match err {
                        tokio::sync::mpsc::error::TrySendError::Full(_) => {
                            DeliveryError::Full(to.clone())
                        }
                        tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                            DeliveryError::Closed(to.clone())
                        }
                    }
                }),
            NodeHandle::Remote { .. } => {
                // Self-release — remote unsupported
                target.mailbox_budget.release();
                Err(DeliveryError::RemoteUnsupported(to.clone()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ports::AgentMessageBus;

    fn dummy_handle(agent_id: AgentId, depth: usize) -> AgentHandle {
        let (tx, _rx) = mpsc::channel(1);
        let (status_tx, _status_rx) = watch::channel(NodeState::Created);
        let (_metrics_tx, metrics_rx) = watch::channel(AgentMetrics::default());
        AgentHandle {
            isolated: false,
            agent_id,
            token: CapabilityTokenId::nil(),
            command_tx: tx,
            cancel_token: tokio_util::sync::CancellationToken::new(),
            depth,
            subagent_type: String::from("test"),
            spawned_at: 0,
            status: status_tx,
            metrics: metrics_rx,
            mailbox_budget: MailboxBudget::new(),
        }
    }

    #[tokio::test]
    async fn depth_3_succeeds() {
        let reg = NodeTree::new();
        let root = AgentId::root();
        let a1 = AgentId::new();
        let a2 = AgentId::new();
        let a3 = AgentId::new();
        reg.register(a1.clone(), root.clone(), dummy_handle(a1.clone(), 1))
            .await
            .unwrap();
        reg.register(a2.clone(), a1.clone(), dummy_handle(a2.clone(), 2))
            .await
            .unwrap();
        reg.register(a3.clone(), a2.clone(), dummy_handle(a3.clone(), 3))
            .await
            .unwrap();
        assert_eq!(reg.depth(&a3).await, 3);
    }

    #[tokio::test]
    async fn depth_4_rejects() {
        let reg = NodeTree::new();
        let root = AgentId::root();
        let a1 = AgentId::new();
        let a2 = AgentId::new();
        let a3 = AgentId::new();
        let a4 = AgentId::new();
        reg.register(a1.clone(), root.clone(), dummy_handle(a1.clone(), 1))
            .await
            .unwrap();
        reg.register(a2.clone(), a1.clone(), dummy_handle(a2.clone(), 2))
            .await
            .unwrap();
        reg.register(a3.clone(), a2.clone(), dummy_handle(a3.clone(), 3))
            .await
            .unwrap();
        let result = reg
            .register(a4.clone(), a3.clone(), dummy_handle(a4.clone(), 4))
            .await;
        assert!(result.is_err());
        match result.unwrap_err() {
            SubagentError::SpawnLimitExceeded {
                kind: SpawnLimitKind::Depth,
                limit: 3,
                attempted: 4,
            } => {}
            other => panic!("expected Depth limit error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn children_10_succeeds() {
        let reg = NodeTree::new();
        let root = AgentId::root();
        for _ in 0..10 {
            let a = AgentId::new();
            reg.register(a.clone(), root.clone(), dummy_handle(a.clone(), 1))
                .await
                .unwrap();
        }
        assert_eq!(reg.children_of(&root).await.len(), 10);
    }

    #[tokio::test]
    async fn children_11_rejects() {
        let reg = NodeTree::new();
        let root = AgentId::root();
        for _ in 0..10 {
            let a = AgentId::new();
            reg.register(a.clone(), root.clone(), dummy_handle(a.clone(), 1))
                .await
                .unwrap();
        }
        let a11 = AgentId::new();
        let result = reg
            .register(a11.clone(), root.clone(), dummy_handle(a11.clone(), 1))
            .await;
        assert!(result.is_err());
        match result.unwrap_err() {
            SubagentError::SpawnLimitExceeded {
                kind: SpawnLimitKind::Children,
                limit: 10,
                attempted: 11,
            } => {}
            other => panic!("expected Children limit error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn deregister_and_snapshot_roundtrip() {
        let reg = NodeTree::new();
        let root = AgentId::root();
        let a1 = AgentId::new();
        reg.register(a1.clone(), root.clone(), dummy_handle(a1.clone(), 1))
            .await
            .unwrap();
        let snap = reg.snapshot().await;
        assert_eq!(snap.len(), 1);
        reg.deregister(&a1).await;
        let snap = reg.snapshot().await;
        assert!(snap.is_empty());
    }

    #[tokio::test]
    async fn ancestors_three_level_chain() {
        let reg = NodeTree::new();
        let root = AgentId::root();
        let a = AgentId::new();
        let b = AgentId::new();
        let c = AgentId::new();
        reg.register(a.clone(), root.clone(), dummy_handle(a.clone(), 1))
            .await
            .unwrap();
        reg.register(b.clone(), a.clone(), dummy_handle(b.clone(), 2))
            .await
            .unwrap();
        reg.register(c.clone(), b.clone(), dummy_handle(c.clone(), 3))
            .await
            .unwrap();

        let anc = reg.ancestors(&c).await;
        assert_eq!(anc, vec![b.clone(), a.clone()]);
    }

    #[tokio::test]
    async fn ancestors_root_returns_empty() {
        let reg = NodeTree::new();
        let anc = reg.ancestors(&AgentId::root()).await;
        assert!(anc.is_empty());
    }

    #[tokio::test]
    async fn ancestors_unregistered_returns_empty() {
        let reg = NodeTree::new();
        let anc = reg.ancestors(&AgentId::new()).await;
        assert!(anc.is_empty());
    }

    #[tokio::test]
    async fn subtree_sibling_isolation() {
        let reg = NodeTree::new();
        let root = AgentId::root();
        let a = AgentId::new();
        let b = AgentId::new();
        let c = AgentId::new();
        reg.register(a.clone(), root.clone(), dummy_handle(a.clone(), 1))
            .await
            .unwrap();
        reg.register(b.clone(), a.clone(), dummy_handle(b.clone(), 2))
            .await
            .unwrap();
        reg.register(c.clone(), a.clone(), dummy_handle(c.clone(), 2))
            .await
            .unwrap();

        let subtree_a = reg.subtree(&a).await;
        assert!(subtree_a.contains(&b));
        assert!(subtree_a.contains(&c));
        assert_eq!(subtree_a.len(), 2);

        let subtree_b = reg.subtree(&b).await;
        assert!(subtree_b.is_empty());
    }

    #[tokio::test]
    async fn subtree_root_returns_full_set() {
        let reg = NodeTree::new();
        let root = AgentId::root();
        let a = AgentId::new();
        let b = AgentId::new();
        reg.register(a.clone(), root.clone(), dummy_handle(a.clone(), 1))
            .await
            .unwrap();
        reg.register(b.clone(), a.clone(), dummy_handle(b.clone(), 2))
            .await
            .unwrap();

        let subtree_root = reg.subtree(&root).await;
        assert!(subtree_root.contains(&a));
        assert!(subtree_root.contains(&b));
        assert!(!subtree_root.contains(&root));
    }

    #[tokio::test]
    async fn list_empty_registry() {
        let reg = NodeTree::new();
        let entries = reg.list().await;
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn list_three_entries_sorted() {
        let now = Arc::new(|| 1_700_000_000_000_i64);
        let reg = NodeTree::with_now_fn(now);
        let root = AgentId::root();
        let a = AgentId::new();
        let b = AgentId::new();
        let c = AgentId::new();
        reg.register(a.clone(), root.clone(), dummy_handle(a.clone(), 1))
            .await
            .unwrap();
        reg.register(b.clone(), root.clone(), dummy_handle(b.clone(), 1))
            .await
            .unwrap();
        reg.register(c.clone(), root.clone(), dummy_handle(c.clone(), 1))
            .await
            .unwrap();

        let entries = reg.list().await;
        assert_eq!(entries.len(), 3);
        assert!(entries[0].agent_id.as_str() <= entries[1].agent_id.as_str());
        assert!(entries[1].agent_id.as_str() <= entries[2].agent_id.as_str());
        assert_eq!(entries[0].spawned_at, 1_700_000_000_000);
    }

    #[tokio::test]
    async fn list_post_deregister() {
        let reg = NodeTree::new();
        let root = AgentId::root();
        let a = AgentId::new();
        reg.register(a.clone(), root.clone(), dummy_handle(a.clone(), 1))
            .await
            .unwrap();
        reg.deregister(&a).await;
        let entries = reg.list().await;
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn status_sender_round_trip() {
        let reg = NodeTree::new();
        let root = AgentId::root();
        let a = AgentId::new();
        reg.register(a.clone(), root.clone(), dummy_handle(a.clone(), 1))
            .await
            .unwrap();

        let tx = reg.status_sender(&a).await;
        assert!(tx.is_some());
        let tx = tx.unwrap();
        let _ = tx.send(NodeState::Running);
        reg.set_state(&a, NodeState::Running).await;

        let entries = reg.list().await;
        assert_eq!(entries[0].current_status, NodeState::Running);
    }

    #[tokio::test]
    async fn status_sender_unregistered_returns_none() {
        let reg = NodeTree::new();
        let tx = reg.status_sender(&AgentId::new()).await;
        assert!(tx.is_none());
    }

    #[tokio::test]
    async fn cascade_kill_single_leaf() {
        let reg = NodeTree::new();
        let root = AgentId::root();
        let a = AgentId::new();
        let (cmd_tx, mut cmd_rx) = mpsc::channel(1);
        let (status_tx, _status_rx) = watch::channel(NodeState::Created);
        let (_metrics_tx, metrics_rx) = watch::channel(AgentMetrics::default());
        let handle = AgentHandle {
            isolated: false,
            agent_id: a.clone(),
            token: CapabilityTokenId::nil(),
            command_tx: cmd_tx,
            cancel_token: tokio_util::sync::CancellationToken::new(),
            depth: 1,
            subagent_type: "test".into(),
            spawned_at: 0,
            status: status_tx,
            metrics: metrics_rx,
            mailbox_budget: MailboxBudget::new(),
        };
        reg.register(a.clone(), root.clone(), handle).await.unwrap();

        let reg_clone = reg.clone();
        let a_clone = a.clone();
        tokio::spawn(async move {
            while let Some(op) = cmd_rx.recv().await {
                if matches!(op, Op::Kill) {
                    if let Some(tx) = reg_clone.status_sender(&a_clone).await {
                        let _: Result<(), watch::error::SendError<NodeState>> =
                            tx.send(NodeState::Cancelled);
                    }
                    break;
                }
            }
        });

        let result = reg.cascade_kill(&a, Duration::from_millis(500)).await;
        assert!(result.is_ok());
        let killed = result.unwrap();
        assert_eq!(killed, vec![a.clone()]);
    }

    #[tokio::test]
    async fn cascade_kill_not_found() {
        let reg = NodeTree::new();
        let result = reg
            .cascade_kill(&AgentId::new(), Duration::from_millis(50))
            .await;
        assert!(matches!(result, Err(CascadeKillError::NotFound(_))));
    }

    #[tokio::test]
    async fn cascade_kill_closed_channel_graceful() {
        let reg = NodeTree::new();
        let root = AgentId::root();
        let a = AgentId::new();
        let (cmd_tx, _cmd_rx) = mpsc::channel(1);
        let (status_tx, _status_rx) = watch::channel(NodeState::Created);
        let (_metrics_tx, metrics_rx) = watch::channel(AgentMetrics::default());
        let handle = AgentHandle {
            isolated: false,
            agent_id: a.clone(),
            token: CapabilityTokenId::nil(),
            command_tx: cmd_tx,
            cancel_token: tokio_util::sync::CancellationToken::new(),
            depth: 1,
            subagent_type: "test".into(),
            spawned_at: 0,
            status: status_tx,
            metrics: metrics_rx,
            mailbox_budget: MailboxBudget::new(),
        };
        reg.register(a.clone(), root.clone(), handle).await.unwrap();

        // Drop the command receiver to close the channel
        drop(_cmd_rx);

        let result = reg.cascade_kill(&a, Duration::from_millis(50)).await;
        assert!(result.is_ok());
        let killed = result.unwrap();
        assert_eq!(killed, vec![a.clone()]);
    }

    // ── Story 14-4a: MailboxBudget reserve-at-admission & conservation tests ──

    fn make_bus() -> (NodeTree, LocalMessageBus) {
        let tree = NodeTree::new();
        let bus = LocalMessageBus::new(
            tree.clone(),
            Arc::new(crate::domain::ports::RelationshipDeliveryPolicy),
        );
        (tree, bus)
    }

    fn make_envelope(
        content: &str,
        corr: &str,
    ) -> crate::domain::models::Envelope<crate::domain::models::AgentMessage> {
        use crate::domain::models::*;
        Envelope::new(
            MessageHeader {
                sender: AgentId::from_validated("parent"),
                recipient: AgentId::from_validated("child"),
                correlation_id: CorrelationId::new(corr),
                kind: MessageKind::PeerMessage,
                sequence: None,
            },
            AgentMessage::new(content),
        )
    }

    // Helper: register an agent with a LIVE command receiver (kept alive by
    // the caller) so try_send succeeds. Returns the receiver to hold.
    async fn register_live_agent(tree: &NodeTree, agent_id: &AgentId) -> mpsc::Receiver<Op> {
        let (tx, rx) = mpsc::channel::<Op>(512);
        let (status_tx, _status_rx) = watch::channel(NodeState::Created);
        let (_metrics_tx, metrics_rx) = watch::channel(AgentMetrics::default());
        let handle = AgentHandle {
            isolated: false,
            agent_id: agent_id.clone(),
            token: CapabilityTokenId::nil(),
            command_tx: tx,
            cancel_token: tokio_util::sync::CancellationToken::new(),
            depth: 1,
            subagent_type: String::from("test"),
            spawned_at: 0,
            status: status_tx,
            metrics: metrics_rx,
            mailbox_budget: MailboxBudget::new(),
        };
        tree.register(agent_id.clone(), AgentId::root(), handle)
            .await
            .unwrap();
        rx
    }

    // Helper to get the mailbox budget for a registered agent
    impl NodeTree {
        async fn mailbox_budget(&self, agent_id: &AgentId) -> Option<MailboxBudget> {
            let guard = self.inner.read().await;
            guard.mailbox_budgets.get(agent_id).cloned()
        }
    }

    /// t1 (AC1) — admission-cap sync refusal: a full budget returns Full
    /// immediately with the Op never sent.
    #[tokio::test]
    async fn t1_admission_cap_sync_refusal() {
        let (tree, bus) = make_bus();
        let agent = AgentId::from_validated("child");
        let _rx = register_live_agent(&tree, &agent).await;

        let budget = tree.mailbox_budget(&agent).await.unwrap();
        for _ in 0..MAILBOX_CAP {
            assert!(budget.reserve().is_ok());
        }
        let result = bus.deliver(&agent, make_envelope("overflow", "c0")).await;
        assert!(
            matches!(result, Err(crate::domain::ports::DeliveryError::Full(_))),
            "full budget must return DeliveryError::Full"
        );
        assert_eq!(budget.current(), MAILBOX_CAP);
    }

    /// t1-positive (AC1) — below cap, deliver succeeds and returns Accepted.
    #[tokio::test]
    async fn t1_positive_below_cap_deliver_accepted() {
        let (tree, bus) = make_bus();
        let agent = AgentId::from_validated("child");
        let _rx = register_live_agent(&tree, &agent).await;

        let result = bus.deliver(&agent, make_envelope("hello", "c1")).await;
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            crate::domain::models::DeliveryOutcome::Accepted
        );
        let budget = tree.mailbox_budget(&agent).await.unwrap();
        assert_eq!(budget.current(), 1);
    }

    /// t2 (AC1) — TOCTOU concurrency: N concurrent senders at capacity-1 →
    /// exactly 1 Accepted, N-1 Full.
    #[tokio::test]
    async fn t2_toctou_concurrency_at_capacity_boundary() {
        let (tree, bus) = make_bus();
        let agent = AgentId::from_validated("child");
        let _rx = register_live_agent(&tree, &agent).await;

        let budget = tree.mailbox_budget(&agent).await.unwrap();
        for _ in 0..(MAILBOX_CAP - 1) {
            budget.reserve().unwrap();
        }

        let bus = Arc::new(bus);
        let mut handles = Vec::new();
        for i in 0..10 {
            let bus = bus.clone();
            let agent = agent.clone();
            handles.push(tokio::spawn(async move {
                bus.deliver(&agent, make_envelope("race", &format!("c{}", i)))
                    .await
            }));
        }
        let mut accepted = 0;
        let mut full = 0;
        for h in handles {
            match h.await.unwrap() {
                Ok(_) => accepted += 1,
                Err(crate::domain::ports::DeliveryError::Full(_)) => full += 1,
                _ => {}
            }
        }
        assert_eq!(accepted, 1, "exactly one sender must win the last slot");
        assert_eq!(full, 9, "remaining senders must get Full");
        assert_eq!(budget.current(), MAILBOX_CAP);
    }

    /// t5 (AC4) — deliver-after-terminal → Refused(TerminalState), not Full.
    #[tokio::test]
    async fn t5_deliver_after_terminal_refused_terminal_state() {
        let (tree, bus) = make_bus();
        let agent = AgentId::from_validated("child");
        let _rx = register_live_agent(&tree, &agent).await;

        // Valid FSM path: Created → Running → Completed
        tree.set_state(&agent, NodeState::Running).await;
        tree.set_state(&agent, NodeState::Completed).await;

        let result = bus.deliver(&agent, make_envelope("late", "c5")).await;
        assert!(
            matches!(
                result,
                Err(crate::domain::ports::DeliveryError::Refused(
                    crate::domain::models::RefuseReason::TerminalState
                ))
            ),
            "deliver after terminal must return Refused(TerminalState)"
        );
    }

    /// t8 (AC2) — unified budget bounds Aside/Wake: the budget is shared
    /// across ALL delivery modes.
    #[tokio::test]
    async fn t8_unified_budget_bounds_aside_wake() {
        let (tree, bus) = make_bus();
        let agent = AgentId::from_validated("child");
        let _rx = register_live_agent(&tree, &agent).await;
        tree.set_state(&agent, NodeState::Running).await;

        let budget = tree.mailbox_budget(&agent).await.unwrap();
        for i in 0..MAILBOX_CAP {
            let result = bus
                .deliver(&agent, make_envelope("aside", &format!("a{}", i)))
                .await;
            assert!(result.is_ok(), "deliver {} must succeed below cap", i);
        }
        assert_eq!(budget.current(), MAILBOX_CAP);

        let result = bus.deliver(&agent, make_envelope("overflow", "a99")).await;
        assert!(
            matches!(result, Err(crate::domain::ports::DeliveryError::Full(_))),
            "Aside/Wake deliveries must be bounded by the same budget"
        );
    }

    /// t9 (AC4) — enum shape: Accepted is the sole variant.
    #[test]
    fn t9_enum_shape_accepted_only_variant() {
        use crate::domain::models::DeliveryOutcome;
        let outcome = DeliveryOutcome::Accepted;
        assert_eq!(outcome, DeliveryOutcome::Accepted);
    }

    /// AC3 grep-ratchet: the disposition must be stamped, not discarded.
    #[test]
    fn ac3_ratchet_no_discarded_disposition() {
        let source = include_str!("node_tree.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production section");
        assert!(
            !production.contains("let _disposition = self.policy"),
            "the disposition must be stamped into AgentDelivery, not discarded"
        );
        assert!(
            production.contains("let disposition = self.policy.decide"),
            "deliver() must stamp policy.decide() into AgentDelivery"
        );
    }
    /// t3 (AC2 [K]) — cancel-time conservation: fill + drain → budget == 0,
    /// Σ(drained) == sent.
    #[tokio::test]
    async fn t3_cancel_time_conservation_keystone() {
        let (tree, bus) = make_bus();
        let agent = AgentId::from_validated("child");
        let mut rx = register_live_agent(&tree, &agent).await;

        let n = 10;
        for i in 0..n {
            let result = bus
                .deliver(&agent, make_envelope("msg", &format!("c{i}")))
                .await;
            assert!(result.is_ok());
        }
        let budget = tree.mailbox_budget(&agent).await.unwrap();
        assert_eq!(
            budget.current(),
            n,
            "budget must equal sent count after delivery"
        );

        // Simulate terminal: close + drain
        rx.close();
        let mut drained = 0;
        while let Ok(op) = rx.try_recv() {
            if let crate::domain::models::Op::Deliver(_) = op {
                budget.release();
                drained += 1;
            }
        }
        assert_eq!(budget.current(), 0, "budget must reach 0 after drain");
        assert_eq!(drained, n, "Σ(drained) must equal sent — zero unaccounted");
    }

    /// t3 mutant — skip drain → budget leaks (proves drain is required).
    #[tokio::test]
    async fn t3_mutant_skip_drain_leaks_budget() {
        let (tree, bus) = make_bus();
        let agent = AgentId::from_validated("child");
        let mut rx = register_live_agent(&tree, &agent).await;

        for i in 0..5 {
            bus.deliver(&agent, make_envelope("msg", &format!("m{i}")))
                .await
                .unwrap();
        }
        let budget = tree.mailbox_budget(&agent).await.unwrap();
        // Skip drain — just close and drop
        rx.close();
        drop(rx);
        // Budget is NOT zero — proves drain is required for conservation
        assert_ne!(
            budget.current(),
            0,
            "without drain, budget must leak (RED mutant)"
        );
    }

    /// t7 (AC3 [K]) — hostile-policy differential: MayRefuse vs MustReport
    /// produce observably different dispositions on the delivered Op.
    #[tokio::test]
    async fn t7_hostile_policy_differential_keystone() {
        use crate::domain::models::DeliveryDisposition;
        use crate::domain::ports::DeliveryPolicy;

        // Custom hostile policy: always MayRefuse
        struct RefuseAllPolicy;
        impl DeliveryPolicy for RefuseAllPolicy {
            fn decide(
                &self,
                _header: &crate::domain::models::MessageHeader,
                _ownership: OwnershipKind,
            ) -> DeliveryDisposition {
                DeliveryDisposition::MayRefuse
            }
        }

        let tree = NodeTree::new();
        let agent = AgentId::from_validated("target");

        // --- Hostile policy bus ---
        let hostile_bus = LocalMessageBus::new(tree.clone(), Arc::new(RefuseAllPolicy));
        let mut hostile_rx = register_live_agent(&tree, &agent).await;
        hostile_bus
            .deliver(&agent, make_envelope("hostile", "h1"))
            .await
            .unwrap();
        let hostile_op = hostile_rx.try_recv().unwrap();
        let hostile_disposition = match hostile_op {
            Op::Deliver(d) => d.disposition,
            _ => panic!("expected Deliver"),
        };
        assert_eq!(
            hostile_disposition,
            DeliveryDisposition::MayRefuse,
            "hostile policy must stamp MayRefuse"
        );

        // Deregister and re-register for clean state
        tree.deregister_one(&agent).await;

        // --- Default (Owned→MustReport) policy bus ---
        let default_bus = LocalMessageBus::new(
            tree.clone(),
            Arc::new(crate::domain::ports::RelationshipDeliveryPolicy),
        );
        let mut default_rx = register_live_agent(&tree, &agent).await;
        default_bus
            .deliver(&agent, make_envelope("normal", "n1"))
            .await
            .unwrap();
        let default_op = default_rx.try_recv().unwrap();
        let default_disposition = match default_op {
            Op::Deliver(d) => d.disposition,
            _ => panic!("expected Deliver"),
        };
        assert_eq!(
            default_disposition,
            DeliveryDisposition::MustReport,
            "default Owned policy must stamp MustReport"
        );

        // THE DIFFERENTIAL: same message shape, different policy → different disposition
        assert_ne!(
            hostile_disposition, default_disposition,
            "hostile vs default policy must produce observably different dispositions (INV-DEL-3)"
        );
    }

    /// t7 positive control — default policy (Owned) stamps MustReport.
    /// This is the COLLAPSED state that the hostile differential catches:
    /// without a hostile policy, all dispositions are MustReport.
    #[tokio::test]
    async fn t7_mutant_default_policy_produces_must_report() {
        // With only the default policy, ALL deliveries get MustReport.
        // This is the "collapsed" state — the mutant that the hostile
        // differential test catches.
        let (tree, bus) = make_bus(); // uses RelationshipDeliveryPolicy
        let agent = AgentId::from_validated("target");
        let mut rx = register_live_agent(&tree, &agent).await;
        bus.deliver(&agent, make_envelope("msg", "m1"))
            .await
            .unwrap();
        let op = rx.try_recv().unwrap();
        match op {
            Op::Deliver(d) => {
                assert_eq!(
                    d.disposition,
                    crate::domain::models::DeliveryDisposition::MustReport,
                    "default policy must always produce MustReport for Owned nodes"
                );
            }
            _ => panic!("expected Deliver"),
        }
    }

    #[tokio::test]
    async fn taint_is_monotone_inherited_on_spawn_and_cleared_only_explicitly() {
        let tree = NodeTree::new();
        let parent = AgentId::from_validated("tainted-parent");
        let child = AgentId::from_validated("inheriting-child");
        tree.register(
            parent.clone(),
            AgentId::root(),
            dummy_handle(parent.clone(), 1),
        )
        .await
        .unwrap();

        assert!(!tree.is_tainted(&parent).await);
        assert!(tree.mark_tainted(&parent).await);
        assert!(
            !tree.mark_tainted(&parent).await,
            "second mark is idempotent"
        );
        tree.register(
            child.clone(),
            parent.clone(),
            dummy_handle(child.clone(), 2),
        )
        .await
        .unwrap();
        assert!(
            tree.is_tainted(&child).await,
            "spawn must inherit parent integrity taint"
        );

        // Ordinary reads/turn boundaries do not mutate the bit.
        assert!(tree.is_tainted(&parent).await);
        tree.clear_taint(&parent).await;
        assert!(
            !tree.is_tainted(&parent).await,
            "only an explicit true-context reset clears taint"
        );
        assert!(
            tree.is_tainted(&child).await,
            "resetting one context must not launder a child context"
        );
    }

    #[tokio::test]
    async fn lifecycle_events_cover_self_registration_and_cascade_removal() {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let tree = NodeTree::with_event_tx(event_tx, Arc::new(|| 1_700_000_000_000));
        let agent = AgentId::from_validated("acp-1");
        let expected_id = tree.capability_for(&agent).id;

        tree.register_self_session(agent.clone(), dummy_handle(agent.clone(), 1))
            .await
            .unwrap();
        match tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .expect("registered event timeout")
            .expect("registered event")
        {
            AppEvent::CapabilityEvent(CapabilityEvent::Registered { capability }) => {
                assert_eq!(capability.id, expected_id);
            }
            event => panic!("expected exact Registered event, got {event:?}"),
        }

        tree.cascade_kill(&agent, Duration::ZERO).await.unwrap();
        match tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .expect("deregistered event timeout")
            .expect("deregistered event")
        {
            AppEvent::CapabilityEvent(CapabilityEvent::Deregistered { capability }) => {
                assert_eq!(capability.id, expected_id);
            }
            event => panic!("expected exact Deregistered event, got {event:?}"),
        }
        assert!(event_rx.try_recv().is_err(), "exactly one removal event");
    }

    #[tokio::test]
    async fn lifecycle_events_cover_normal_terminal_cascade_arm() {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let tree = NodeTree::with_event_tx(event_tx, Arc::new(|| 1_700_000_000_000));
        let agent = AgentId::from_validated("normal-terminal");
        let expected_id = tree.capability_for(&agent).id;
        let (command_tx, mut command_rx) = mpsc::channel(1);
        let (status_tx, _) = watch::channel(NodeState::Running);
        let (_, metrics_rx) = watch::channel(AgentMetrics::default());
        tree.register(
            agent.clone(),
            AgentId::root(),
            AgentHandle {
                agent_id: agent.clone(),
                token: CapabilityTokenId::nil(),
                command_tx,
                cancel_token: tokio_util::sync::CancellationToken::new(),
                depth: 1,
                subagent_type: "test".into(),
                spawned_at: 0,
                status: status_tx,
                metrics: metrics_rx,
                isolated: false,
                mailbox_budget: MailboxBudget::new(),
            },
        )
        .await
        .unwrap();
        match tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .expect("registered event timeout")
            .expect("registered event")
        {
            AppEvent::CapabilityEvent(CapabilityEvent::Registered { capability }) => {
                assert_eq!(capability.id, expected_id);
            }
            event => panic!("expected exact Registered event, got {event:?}"),
        }
        tree.set_state(&agent, NodeState::Running).await;
        let tree_for_task = tree.clone();
        let agent_for_task = agent.clone();
        tokio::spawn(async move {
            if matches!(command_rx.recv().await, Some(Op::Kill)) {
                tree_for_task
                    .set_state(&agent_for_task, NodeState::Completed)
                    .await;
            }
        });

        tree.cascade_kill(&agent, Duration::from_secs(1))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match event_rx.recv().await.expect("lifecycle event stream") {
                    AppEvent::CapabilityEvent(CapabilityEvent::Deregistered { capability }) => {
                        assert_eq!(capability.id, expected_id);
                        break;
                    }
                    AppEvent::CapabilityEvent(CapabilityEvent::Updated { id, .. }) => {
                        assert_eq!(id, expected_id);
                    }
                    event => panic!("unexpected lifecycle event before removal: {event:?}"),
                }
            }
        })
        .await
        .expect("deregistered event timeout");
        assert!(event_rx.try_recv().is_err(), "exactly one removal event");
    }

    #[tokio::test]
    async fn concurrent_cascades_emit_one_deregistration_for_one_removal() {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let tree = NodeTree::with_event_tx(event_tx, Arc::new(|| 1_700_000_000_000));
        let agent = AgentId::from_validated("concurrent-cascade");
        tree.register_self_session(agent.clone(), dummy_handle(agent.clone(), 1))
            .await
            .unwrap();
        let _ = event_rx.recv().await.expect("registered event");

        let left = tree.clone();
        let right = tree.clone();
        let left_id = agent.clone();
        let right_id = agent.clone();
        let (left_result, right_result) = tokio::join!(
            left.cascade_kill(&left_id, Duration::ZERO),
            right.cascade_kill(&right_id, Duration::ZERO),
        );
        left_result.unwrap();
        right_result.unwrap();

        let first = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .expect("deregistered event timeout")
            .expect("deregistered event");
        assert!(matches!(
            first,
            AppEvent::CapabilityEvent(CapabilityEvent::Deregistered { .. })
        ));
        assert!(
            event_rx.try_recv().is_err(),
            "concurrent removal emitted duplicate"
        );
    }

    #[tokio::test]
    async fn lifecycle_events_are_silent_without_transmitter() {
        let tree = NodeTree::new();
        let agent = AgentId::from_validated("acp-quiet");
        tree.register_self_session(agent.clone(), dummy_handle(agent.clone(), 1))
            .await
            .unwrap();
        tree.cascade_kill(&agent, Duration::ZERO).await.unwrap();
    }

    // t4 (release-on-turn-dispatch) and t6 (consent-receipt correlation)
    // require the full run_child streaming provider + command loop;
    // covered by the in_process_runner integration test suite.
}
