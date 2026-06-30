use std::collections::HashMap;
use std::sync::Arc;
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
    /// Hook point where Story 14.2 will wire `AuthorityProvider::revoke` for
    /// token invalidation during cascade_kill. No-op closure in R1.
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
}

// ── NodeTree implementation ─────────────────────────────────────────────────

impl NodeTree {
    /// Build an empty `NodeTreeInner`. Single source of truth so adding a
    /// field (e.g. a second event bus) doesn't require touching every
    /// constructor in lockstep.
    fn build_inner() -> tokio::sync::RwLock<NodeTreeInner> {
        tokio::sync::RwLock::new(NodeTreeInner {
            nodes: HashMap::new(),
            handles: HashMap::new(),
            parent_of: HashMap::new(),
            status_rx: HashMap::new(),
            status_senders: HashMap::new(),
            metrics_rx: HashMap::new(),
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

    /// Install the `on_cascade_kill` hook. Story 14.2 wires
    /// `AuthorityProvider::revoke` here for descendant token invalidation
    /// during cascade_kill. Default is a no-op; this builder lets 14.2 inject
    /// the callback at the composition root instead of editing this file
    /// (forward-compat hook #4 / AC4).
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
        mut handle: AgentHandle,
    ) -> Result<(), SubagentError> {
        let mut guard = self.inner.write().await;

        if guard.nodes.contains_key(&agent_id) {
            return Err(SubagentError::Internal(format!(
                "duplicate agent_id: {:?}",
                agent_id
            )));
        }

        // The root sentinel is a parent-only marker; registering it as a node
        // would corrupt parent_of/depth invariants.
        if agent_id == AgentId::root() {
            return Err(SubagentError::Internal(
                "agent_id cannot be the root sentinel".into(),
            ));
        }

        // 1. Compute depth = depth(parent) + 1 (root depth = 0)
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

        // 2. Reject if depth > MAX_DEPTH
        if depth > MAX_DEPTH {
            return Err(SubagentError::SpawnLimitExceeded {
                kind: SpawnLimitKind::Depth,
                limit: MAX_DEPTH,
                attempted: depth,
            });
        }

        // 3. Count current children_of(parent); reject if >= MAX_CHILDREN
        let children_count = guard.parent_of.values().filter(|p| **p == parent).count();
        if children_count >= MAX_CHILDREN {
            return Err(SubagentError::SpawnLimitExceeded {
                kind: SpawnLimitKind::Children,
                limit: MAX_CHILDREN,
                attempted: children_count + 1,
            });
        }

        // 4. Create watch channel for status broadcasting
        let (status_tx, status_rx) = watch::channel(NodeState::Created);

        // 5. Set computed depth and spawn time on handle
        handle.depth = depth;
        if handle.spawned_at == 0 {
            handle.spawned_at = (self.now_fn)();
        }
        handle.status = status_tx.clone();

        // Build AgentNode from legacy AgentHandle
        let node = AgentNode {
            id: agent_id.clone(),
            token: handle.token,
            parent: if parent == AgentId::root() {
                None
            } else {
                Some(parent.clone())
            },
            ownership: OwnershipKind::Owned,
            state: NodeState::Created,
            origin: NodeOrigin::Subagent,
            foreground: true,
            effective_model: String::new(),
            tokens_in: 0,
            tokens_out: 0,
            turns: 0,
            subagent_type: handle.subagent_type.clone(),
            spawned_at: handle.spawned_at,
            depth,
        };

        // Build NodeHandle from legacy AgentHandle. The cancel token is the
        // child task's REAL token (carried on the handle from `launch()`), so
        // `cascade_kill`'s `handle.cancel()` interrupts the task at any await
        // point it selects on `cancel.cancelled()` — not an orphan minted here.
        // `cascade_kill` walks every descendant and cancels each explicitly, so
        // per-node storage is sufficient for the cascade (AC10 child-cascade).
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

        // Release write guard BEFORE any subsequent .await (CLAUDE.md async-lock policy)
        drop(guard);

        // 6. Emit registration event
        if let Some(tx) = &self.event_tx {
            let cap = RegisteredCapability {
                id: CapabilityId {
                    protocol: "subagent".into(),
                    server: String::new(),
                    tool: agent_id.0.clone(),
                },
                protocol: "subagent".into(),
                provider_id: "subagent".into(),
                name: agent_id.0.clone(),
                description: String::new(),
                input_schema: serde_json::Value::Object(Default::default()),
                parallel_safe: false,
            };
            let _ = tx.send(AppEvent::CapabilityEvent(CapabilityEvent::Registered {
                capability: cap,
            }));
        }

        Ok(())
    }

    /// Remove a single node from every map. No cascade, no event emission.
    /// Used internally by `cascade_kill` (which walks the subtree in kill
    /// order itself) and as the per-node primitive of [`Self::deregister`].
    async fn deregister_one(&self, agent_id: &AgentId) {
        let mut guard = self.inner.write().await;
        guard.nodes.remove(agent_id);
        guard.handles.remove(agent_id);
        guard.parent_of.remove(agent_id);
        guard.status_rx.remove(agent_id);
        guard.status_senders.remove(agent_id);
        guard.metrics_rx.remove(agent_id);
        drop(guard);
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

        // Emit a deregistration event per removed node.
        if let Some(tx) = &self.event_tx {
            for id in &to_remove {
                let cap = RegisteredCapability {
                    id: CapabilityId {
                        protocol: "subagent".into(),
                        server: String::new(),
                        tool: id.0.clone(),
                    },
                    protocol: "subagent".into(),
                    provider_id: "subagent".into(),
                    name: id.0.clone(),
                    description: String::new(),
                    input_schema: serde_json::Value::Object(Default::default()),
                    parallel_safe: false,
                };
                let _ = tx.send(AppEvent::CapabilityEvent(CapabilityEvent::Deregistered {
                    capability: cap,
                }));
            }
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
                        tool: agent_id.0.clone(),
                    },
                    protocol: "subagent".into(),
                    provider_id: "subagent".into(),
                    name: agent_id.0.clone(),
                    description: String::new(),
                    input_schema: serde_json::Value::Object(Default::default()),
                    parallel_safe: false,
                };
                let new_cap = RegisteredCapability {
                    id: CapabilityId {
                        protocol: "subagent".into(),
                        server: String::new(),
                        tool: agent_id.0.clone(),
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
            children.sort_by(|a, b| a.0.cmp(&b.0));
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
                }
            })
            .collect();
        drop(guard);
        entries.sort_by(|a, b| a.agent_id.0.cmp(&b.agent_id.0));
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
                        agent_id = %agent_id.0,
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
        Some(DeliveryTarget {
            state: node.state,
            ownership: node.ownership,
            handle,
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
    /// The `on_cascade_kill` hook fires for each killed node — Story 14.2
    /// wires `AuthorityProvider::revoke` here (AC4).
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
            // Fire the cascade hook (no-op in R1; Story 14.2 wires token revocation)
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
                        // Remote kill not supported in R1
                        self.deregister_one(id).await;
                        killed.push(id.clone());
                        continue;
                    }
                };

                if send_result.is_err() {
                    // Channel closed or send timed out — treat as already terminal
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

        let _disposition = self.policy.decide(&env.header, target.ownership);
        let mode = delivery_decision(target.state);
        if mode == DeliveryMode::Refuse {
            return Err(DeliveryError::Refused(RefuseReason::TerminalState));
        }

        match target.handle {
            NodeHandle::Local { command_tx, .. } => command_tx
                .try_send(Op::Deliver(AgentDelivery::new(env, mode)))
                .map(|()| match mode {
                    DeliveryMode::Queue => DeliveryOutcome::Queued,
                    DeliveryMode::Aside | DeliveryMode::Wake => DeliveryOutcome::Delivered,
                    DeliveryMode::Refuse => DeliveryOutcome::Refused {
                        reason: RefuseReason::TerminalState,
                    },
                })
                .map_err(|err| match err {
                    tokio::sync::mpsc::error::TrySendError::Full(_) => {
                        DeliveryError::Full(to.clone())
                    }
                    tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                        DeliveryError::Closed(to.clone())
                    }
                }),
            NodeHandle::Remote { .. } => Err(DeliveryError::RemoteUnsupported(to.clone())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_handle(agent_id: AgentId, depth: usize) -> AgentHandle {
        let (tx, _rx) = mpsc::channel(1);
        let (status_tx, _status_rx) = watch::channel(NodeState::Created);
        let (_metrics_tx, metrics_rx) = watch::channel(AgentMetrics::default());
        AgentHandle {
            agent_id,
            token: CapabilityTokenId::nil(),
            command_tx: tx,
            cancel_token: tokio_util::sync::CancellationToken::new(),
            depth,
            subagent_type: String::from("test"),
            spawned_at: 0,
            status: status_tx,
            metrics: metrics_rx,
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
        assert!(entries[0].agent_id.0 <= entries[1].agent_id.0);
        assert!(entries[1].agent_id.0 <= entries[2].agent_id.0);
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
            agent_id: a.clone(),
            token: CapabilityTokenId::nil(),
            command_tx: cmd_tx,
            cancel_token: tokio_util::sync::CancellationToken::new(),
            depth: 1,
            subagent_type: "test".into(),
            spawned_at: 0,
            status: status_tx,
            metrics: metrics_rx,
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
            agent_id: a.clone(),
            token: CapabilityTokenId::nil(),
            command_tx: cmd_tx,
            cancel_token: tokio_util::sync::CancellationToken::new(),
            depth: 1,
            subagent_type: "test".into(),
            spawned_at: 0,
            status: status_tx,
            metrics: metrics_rx,
        };
        reg.register(a.clone(), root.clone(), handle).await.unwrap();

        // Drop the command receiver to close the channel
        drop(_cmd_rx);

        let result = reg.cascade_kill(&a, Duration::from_millis(50)).await;
        assert!(result.is_ok());
        let killed = result.unwrap();
        assert_eq!(killed, vec![a.clone()]);
    }
}
