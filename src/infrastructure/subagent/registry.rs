use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::domain::models::{AgentId, Op, SpawnLimitKind, SubagentError};

const MAX_DEPTH: usize = 3;
const MAX_CHILDREN: usize = 10;

pub struct SubagentRegistry {
    inner: Arc<tokio::sync::RwLock<RegistryInner>>,
}

struct RegistryInner {
    handles: HashMap<AgentId, AgentHandle>, // live in-process handles
    parent_of: HashMap<AgentId, AgentId>,   // agent → parent (root sentinel for top-level)
}

pub struct AgentHandle {
    pub agent_id: AgentId,
    pub command_tx: mpsc::Sender<Op>, // owner-issued ops
    pub depth: usize,
    pub subagent_type: String,
}

impl SubagentRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(tokio::sync::RwLock::new(RegistryInner {
                handles: HashMap::new(),
                parent_of: HashMap::new(),
            })),
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

        // 4. Set computed depth on handle, then insert
        handle.depth = depth;
        guard.handles.insert(agent_id.clone(), handle);
        guard.parent_of.insert(agent_id.clone(), parent);

        // Release write guard BEFORE any subsequent .await (CLAUDE.md async-lock policy)
        drop(guard);

        Ok(())
    }

    pub async fn deregister(&self, agent_id: &AgentId) {
        let mut guard = self.inner.write().await;
        guard.handles.remove(agent_id);
        guard.parent_of.remove(agent_id);
        drop(guard);
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_handle(agent_id: AgentId, depth: usize) -> AgentHandle {
        let (tx, _rx) = mpsc::channel(1);
        AgentHandle {
            agent_id,
            command_tx: tx,
            depth,
            subagent_type: String::from("test"),
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
}
