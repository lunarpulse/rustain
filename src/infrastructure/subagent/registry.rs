use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::sync::watch;

use crate::domain::events::{AppEvent, CapabilityEvent};
use crate::domain::models::capability_id::CapabilityId;
use crate::domain::models::{
    AgentId, Op, RegisteredCapability, SpawnLimitKind, SubagentError, SubagentRunStatus,
};

const MAX_DEPTH: usize = 3;
const MAX_CHILDREN: usize = 10;

#[derive(Clone)]
pub struct SubagentRegistry {
    inner: Arc<tokio::sync::RwLock<RegistryInner>>,
    event_tx: Option<mpsc::UnboundedSender<AppEvent>>,
    now_fn: Arc<dyn Fn() -> i64 + Send + Sync>,
}

struct RegistryInner {
    handles: HashMap<AgentId, AgentHandle>, // live in-process handles
    parent_of: HashMap<AgentId, AgentId>,   // agent → parent (root sentinel for top-level)
    status_rx: HashMap<AgentId, watch::Receiver<SubagentRunStatus>>, // keeps watch channel alive
}

#[derive(Clone)]
pub struct AgentHandle {
    pub agent_id: AgentId,
    pub command_tx: mpsc::Sender<Op>, // owner-issued ops
    pub depth: usize,
    pub subagent_type: String,
    pub spawned_at: i64,                          // epoch millis
    pub status: watch::Sender<SubagentRunStatus>, // broadcasts current status
}

/// Snapshot of every registered agent's metadata for the master's TUI panel.
/// Order is deterministic (sorted by agent_id) so snapshot tests are byte-stable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryEntry {
    pub agent_id: AgentId,
    pub parent_id: AgentId,
    pub subagent_type: String,
    pub spawned_at: i64,
    pub depth: usize,
    pub current_status: SubagentRunStatus,
}

#[derive(Debug, thiserror::Error)]
pub enum CascadeKillError {
    #[error("not found in registry: {0:?}")]
    NotFound(AgentId),
    #[error("partial cascade: {killed:?} killed, {unresponsive:?} timed out")]
    Partial {
        killed: Vec<AgentId>,
        unresponsive: Vec<AgentId>,
    },
}

impl SubagentRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(tokio::sync::RwLock::new(RegistryInner {
                handles: HashMap::new(),
                parent_of: HashMap::new(),
                status_rx: HashMap::new(),
            })),
            event_tx: None,
            now_fn: Arc::new(|| chrono::Utc::now().timestamp_millis()),
        }
    }

    pub fn with_now_fn(now_fn: Arc<dyn Fn() -> i64 + Send + Sync>) -> Self {
        Self {
            inner: Arc::new(tokio::sync::RwLock::new(RegistryInner {
                handles: HashMap::new(),
                parent_of: HashMap::new(),
                status_rx: HashMap::new(),
            })),
            event_tx: None,
            now_fn,
        }
    }

    pub fn with_event_tx(
        event_tx: mpsc::UnboundedSender<AppEvent>,
        now_fn: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Self {
        Self {
            inner: Arc::new(tokio::sync::RwLock::new(RegistryInner {
                handles: HashMap::new(),
                parent_of: HashMap::new(),
                status_rx: HashMap::new(),
            })),
            event_tx: Some(event_tx),
            now_fn,
        }
    }

    pub async fn register(
        &self,
        agent_id: AgentId,
        parent: AgentId,
        mut handle: AgentHandle,
    ) -> Result<(), SubagentError> {
        let mut guard = self.inner.write().await;

        if guard.handles.contains_key(&agent_id) {
            return Err(SubagentError::Internal(format!(
                "duplicate agent_id: {:?}",
                agent_id
            )));
        }

        // 1. Compute depth = depth(parent) + 1 (root depth = 0)
        let depth = if parent == AgentId::root() {
            1
        } else if let Some(parent_handle) = guard.handles.get(&parent) {
            parent_handle.depth + 1
        } else {
            return Err(SubagentError::Internal(format!(
                "parent not found in registry: {:?}",
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
        let children_count = guard.parent_of.values().filter(|&p| *p == parent).count();
        if children_count >= MAX_CHILDREN {
            return Err(SubagentError::SpawnLimitExceeded {
                kind: SpawnLimitKind::Children,
                limit: MAX_CHILDREN,
                attempted: children_count + 1,
            });
        }

        // 4. Create watch channel for status broadcasting
        let (status_tx, status_rx) = watch::channel(SubagentRunStatus::Idle);

        // 5. Set computed depth and spawn time on handle
        handle.depth = depth;
        if handle.spawned_at == 0 {
            handle.spawned_at = (self.now_fn)();
        }
        handle.status = status_tx;

        guard.handles.insert(agent_id.clone(), handle);
        guard.parent_of.insert(agent_id.clone(), parent);
        guard.status_rx.insert(agent_id.clone(), status_rx);

        // Release write guard BEFORE any subsequent .await (CLAUDE.md async-lock policy)
        drop(guard);

        // 6. Emit registration event
        if let Some(ref tx) = self.event_tx {
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

    pub async fn deregister(&self, agent_id: &AgentId) {
        let mut guard = self.inner.write().await;
        guard.handles.remove(agent_id);
        guard.parent_of.remove(agent_id);
        guard.status_rx.remove(agent_id);
        drop(guard);

        // Emit deregistration event
        if let Some(ref tx) = self.event_tx {
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
            let _ = tx.send(AppEvent::CapabilityEvent(CapabilityEvent::Deregistered {
                capability: cap,
            }));
        }
    }

    /// Emit a CapabilityEvent::Updated for a subagent status change (AC-10-2-4).
    pub async fn emit_status_updated(&self, agent_id: &AgentId) {
        if let Some(ref tx) = self.event_tx {
            let guard = self.inner.read().await;
            if let Some(handle) = guard.handles.get(agent_id) {
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
                    name: handle.subagent_type.clone(),
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
        guard.handles.get(agent_id).map(|h| h.depth).unwrap_or(0)
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
                let depth = guard.handles.get(agent_id).map(|h| h.depth).unwrap_or(0);
                (agent_id.clone(), parent_id.clone(), depth)
            })
            .collect()
    }

    /// Walk from `agent_id` up to (but not including) `AgentId::root()` in order child-first.
    /// Returns `Vec::new()` if `agent_id == AgentId::root()` or if not registered.
    pub async fn ancestors(&self, agent_id: &AgentId) -> Vec<AgentId> {
        if *agent_id == AgentId::root() {
            return Vec::new();
        }
        let guard = self.inner.read().await;
        let mut result = Vec::new();
        let mut current = agent_id.clone();
        while let Some(parent) = guard.parent_of.get(&current) {
            if *parent == AgentId::root() {
                break;
            }
            result.push(parent.clone());
            current = parent.clone();
        }
        drop(guard);
        result
    }

    /// Return every descendant of `agent_id` (excluding `agent_id` itself),
    /// discovered via BFS over `parent_of`. Order is deterministic via BTreeMap intermediate.
    pub async fn subtree(&self, agent_id: &AgentId) -> Vec<AgentId> {
        let guard = self.inner.read().await;
        let mut result = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(agent_id.clone());
        while let Some(current) = queue.pop_front() {
            // Collect children of current, sorted for determinism
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
            .handles
            .iter()
            .map(|(agent_id, handle)| RegistryEntry {
                agent_id: agent_id.clone(),
                parent_id: guard
                    .parent_of
                    .get(agent_id)
                    .cloned()
                    .unwrap_or_else(AgentId::root),
                subagent_type: handle.subagent_type.clone(),
                spawned_at: handle.spawned_at,
                depth: handle.depth,
                current_status: *handle.status.borrow(),
            })
            .collect();
        drop(guard);
        entries.sort_by(|a, b| a.agent_id.0.cmp(&b.agent_id.0));
        entries
    }

    /// Return a clone of the watch sender for a given agent_id.
    pub async fn status_sender(
        &self,
        agent_id: &AgentId,
    ) -> Option<watch::Sender<SubagentRunStatus>> {
        let guard = self.inner.read().await;
        guard.handles.get(agent_id).map(|h| h.status.clone())
    }

    /// Return a clone of the watch receiver for a given agent_id.
    pub async fn status_rx(
        &self,
        agent_id: &AgentId,
    ) -> Option<watch::Receiver<SubagentRunStatus>> {
        let guard = self.inner.read().await;
        guard.status_rx.get(agent_id).cloned()
    }

    /// DFS-walk the subtree of `agent_id` (children before self), issuing
    /// Op::Kill to each handle and awaiting its `current_status` watch
    /// channel to reach a terminal state before continuing up.
    pub async fn cascade_kill(
        &self,
        agent_id: &AgentId,
        timeout_per_node: Duration,
    ) -> Result<Vec<AgentId>, CascadeKillError> {
        // Verify agent exists
        let guard = self.inner.read().await;
        if !guard.handles.contains_key(agent_id) {
            return Err(CascadeKillError::NotFound(agent_id.clone()));
        }
        drop(guard);

        // Build kill order: subtree (BFS) → reverse → append self
        let mut descendants = self.subtree(agent_id).await;
        descendants.reverse();
        descendants.push(agent_id.clone());

        let mut killed = Vec::new();
        let mut unresponsive = Vec::new();

        for id in &descendants {
            let handle_opt = {
                let guard = self.inner.read().await;
                guard.handles.get(id).cloned()
            };

            if let Some(handle) = handle_opt {
                // Send Kill op
                if let Err(_e) = handle.command_tx.send(Op::Kill).await {
                    // Channel closed — treat as already terminal
                    self.deregister(id).await;
                    killed.push(id.clone());
                    continue;
                }

                // Wait for terminal status via watch channel
                let mut rx = handle.status.subscribe();
                let timeout_result = tokio::time::timeout(timeout_per_node, async {
                    loop {
                        if rx.changed().await.is_err() {
                            // Sender dropped — treat as terminal
                            return;
                        }
                        let status = *rx.borrow();
                        if matches!(
                            status,
                            SubagentRunStatus::Completed
                                | SubagentRunStatus::Failed
                                | SubagentRunStatus::Killed
                        ) {
                            return;
                        }
                    }
                })
                .await;

                if timeout_result.is_err() {
                    unresponsive.push(id.clone());
                } else {
                    killed.push(id.clone());
                }

                // Deregister regardless of timeout (best effort)
                self.deregister(id).await;
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

    /// Convenience wrapper with default 500ms timeout per node.
    pub async fn cascade_kill_with_timeout(
        &self,
        agent_id: &AgentId,
        timeout_per_node: Duration,
    ) -> Result<Vec<AgentId>, CascadeKillError> {
        self.cascade_kill(agent_id, timeout_per_node).await
    }
}

impl Default for SubagentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_handle(agent_id: AgentId, depth: usize) -> AgentHandle {
        let (tx, _rx) = mpsc::channel(1);
        let (status_tx, _status_rx) = watch::channel(SubagentRunStatus::Idle);
        AgentHandle {
            agent_id,
            command_tx: tx,
            depth,
            subagent_type: String::from("test"),
            spawned_at: 0,
            status: status_tx,
        }
    }

    #[tokio::test]
    async fn depth_3_succeeds() {
        let reg = SubagentRegistry::new();
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
        let reg = SubagentRegistry::new();
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
        let reg = SubagentRegistry::new();
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
        let reg = SubagentRegistry::new();
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
        let reg = SubagentRegistry::new();
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

    // ── AC-10-2-1: ancestors + subtree ──────────────────────────────────

    #[tokio::test]
    async fn ancestors_three_level_chain() {
        let reg = SubagentRegistry::new();
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
        let reg = SubagentRegistry::new();
        let anc = reg.ancestors(&AgentId::root()).await;
        assert!(anc.is_empty());
    }

    #[tokio::test]
    async fn ancestors_unregistered_returns_empty() {
        let reg = SubagentRegistry::new();
        let anc = reg.ancestors(&AgentId::new()).await;
        assert!(anc.is_empty());
    }

    #[tokio::test]
    async fn subtree_sibling_isolation() {
        let reg = SubagentRegistry::new();
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
        let reg = SubagentRegistry::new();
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

    // ── AC-10-2-3: list ─────────────────────────────────────────────────

    #[tokio::test]
    async fn list_empty_registry() {
        let reg = SubagentRegistry::new();
        let entries = reg.list().await;
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn list_three_entries_sorted() {
        let now = Arc::new(|| 1_700_000_000_000_i64);
        let reg = SubagentRegistry::with_now_fn(now);
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
        // Verify sorted by agent_id
        assert!(entries[0].agent_id.0 <= entries[1].agent_id.0);
        assert!(entries[1].agent_id.0 <= entries[2].agent_id.0);
        // All have the fixed spawn time
        assert_eq!(entries[0].spawned_at, 1_700_000_000_000);
    }

    #[tokio::test]
    async fn list_post_deregister() {
        let reg = SubagentRegistry::new();
        let root = AgentId::root();
        let a = AgentId::new();
        reg.register(a.clone(), root.clone(), dummy_handle(a.clone(), 1))
            .await
            .unwrap();
        reg.deregister(&a).await;
        let entries = reg.list().await;
        assert!(entries.is_empty());
    }

    // ── AC-10-2-7: status_sender ────────────────────────────────────────

    #[tokio::test]
    async fn status_sender_round_trip() {
        let reg = SubagentRegistry::new();
        let root = AgentId::root();
        let a = AgentId::new();
        reg.register(a.clone(), root.clone(), dummy_handle(a.clone(), 1))
            .await
            .unwrap();

        let tx = reg.status_sender(&a).await;
        assert!(tx.is_some());
        let tx = tx.unwrap();
        let _ = tx.send(SubagentRunStatus::RunningFg);

        let entries = reg.list().await;
        assert_eq!(entries[0].current_status, SubagentRunStatus::RunningFg);
    }

    #[tokio::test]
    async fn status_sender_unregistered_returns_none() {
        let reg = SubagentRegistry::new();
        let tx = reg.status_sender(&AgentId::new()).await;
        assert!(tx.is_none());
    }

    // ── AC-10-2-6: cascade_kill ─────────────────────────────────────────

    #[tokio::test]
    async fn cascade_kill_single_leaf() {
        let reg = SubagentRegistry::new();
        let root = AgentId::root();
        let a = AgentId::new();
        let (cmd_tx, mut cmd_rx) = mpsc::channel(1);
        let (status_tx, _status_rx) = watch::channel(SubagentRunStatus::Idle);
        let handle = AgentHandle {
            agent_id: a.clone(),
            command_tx: cmd_tx,
            depth: 1,
            subagent_type: "test".into(),
            spawned_at: 0,
            status: status_tx,
        };
        reg.register(a.clone(), root.clone(), handle).await.unwrap();

        // Spawn a fake child that reacts to Op::Kill by updating the registry watch
        let reg_clone = reg.clone();
        let a_clone = a.clone();
        tokio::spawn(async move {
            while let Some(op) = cmd_rx.recv().await {
                if matches!(op, Op::Kill) {
                    if let Some(tx) = reg_clone.status_sender(&a_clone).await {
                        let _: Result<(), watch::error::SendError<SubagentRunStatus>> =
                            tx.send(SubagentRunStatus::Killed);
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
        let reg = SubagentRegistry::new();
        let result = reg
            .cascade_kill(&AgentId::new(), Duration::from_millis(50))
            .await;
        assert!(matches!(result, Err(CascadeKillError::NotFound(_))));
    }

    #[tokio::test]
    async fn cascade_kill_closed_channel_graceful() {
        let reg = SubagentRegistry::new();
        let root = AgentId::root();
        let a = AgentId::new();
        let (cmd_tx, _cmd_rx) = mpsc::channel(1);
        let (status_tx, _status_rx) = watch::channel(SubagentRunStatus::Idle);
        let handle = AgentHandle {
            agent_id: a.clone(),
            command_tx: cmd_tx,
            depth: 1,
            subagent_type: "test".into(),
            spawned_at: 0,
            status: status_tx,
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
