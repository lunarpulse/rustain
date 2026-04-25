//! ApprovalRuntime — pub/sub coordination for tool-call permission requests.
//!
//! # Broadcast capacity policy
//! `event_capacity` of `0` falls back to `1024` with a `tracing::warn!`.
//!
//! # Invariants
//! - `ApprovalSource` is **always** an explicit argument; `tokio::task_local!`
//!   is forbidden (ADR-06-05).
//! - Fast-path (`is_auto_approved == true`) emits **no** broadcast event,
//!   inserts **nothing** into `pending`, and allocates **no** `RequestId`.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use tokio::sync::{broadcast, oneshot, RwLock};

use crate::domain::models::tool_call::{ApprovalSource, RequestId};
use crate::domain::models::ToolRisk;
use crate::domain::models::{ApprovalOutcome, ApprovalScope};
use crate::domain::ports::ApprovalPersistencePort;

/// In-memory session-level auto-allow set.
#[derive(Default, Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SessionApprovalSet {
    pub always_tools: BTreeSet<String>,
    pub always_servers: BTreeSet<String>,
    pub always_paths: Vec<String>,
    #[serde(skip)]
    glob_cache: Option<globset::GlobSet>,
}

impl SessionApprovalSet {
    /// Check whether the given tool/server/path is auto-approved.
    pub fn is_auto_approved(&mut self, tool: &str, server: Option<&str>, path: Option<&str>) -> bool {
        if self.always_tools.contains(tool) {
            return true;
        }
        if server.map(|s| self.always_servers.contains(s)).unwrap_or(false) {
            return true;
        }
        if let Some(p) = path {
            if !self.always_paths.is_empty() {
                if self.glob_cache.is_none() {
                    let mut builder = globset::GlobSetBuilder::new();
                    for pattern in &self.always_paths {
                        if let Ok(glob) = globset::Glob::new(pattern) {
                            let _ = builder.add(glob);
                        }
                    }
                    if let Ok(gs) = builder.build() {
                        self.glob_cache = Some(gs);
                    }
                }
                if let Some(ref gs) = self.glob_cache {
                    if gs.is_match(p) {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Invalidate the glob cache (call after mutating `always_paths`).
    pub fn invalidate_glob_cache(&mut self) {
        self.glob_cache = None;
    }
}

/// Event emitted on the approval runtime broadcast channel.
#[derive(Clone, Debug, serde::Serialize)]
pub enum ApprovalRuntimeEvent {
    Requested {
        id: RequestId,
        source: ApprovalSource,
        tool: String,
        input_preview: String,
        risk: ToolRisk,
    },
    Resolved {
        id: RequestId,
        outcome: ApprovalOutcome,
    },
    Cancelled {
        id: RequestId,
        reason: CancelReason,
    },
}

/// Reason an approval was cancelled.
#[derive(Clone, Debug, serde::Serialize)]
pub enum CancelReason {
    SourceAborted,
    RuntimeShutdown,
    Timeout,
}

/// Internal record for a pending approval.
struct PendingRecord {
    request: ApprovalRequest,
    responder: oneshot::Sender<ApprovalOutcome>,
}

/// Internal request struct.
struct ApprovalRequest {
    id: RequestId,
    source: ApprovalSource,
    tool: String,
    input: serde_json::Value,
    risk: ToolRisk,
    server_id: Option<String>,
    path_hint: Option<String>,
}

/// Pub/sub runtime for tool-call approvals.
pub struct ApprovalRuntime {
    pending: Arc<RwLock<HashMap<RequestId, PendingRecord>>>,
    events: broadcast::Sender<ApprovalRuntimeEvent>,
    session: Arc<RwLock<SessionApprovalSet>>,
    persistence: Arc<dyn ApprovalPersistencePort>,
}

impl ApprovalRuntime {
    /// Construct a new runtime. `event_capacity` defaults to 1024 when 0.
    pub fn new(event_capacity: usize, persistence: Arc<dyn ApprovalPersistencePort>) -> Arc<Self> {
        let cap = if event_capacity == 0 {
            tracing::warn!("ApprovalRuntime event_capacity was 0, falling back to 1024");
            1024
        } else {
            event_capacity
        };
        let (events, _) = broadcast::channel(cap);
        Arc::new(Self {
            pending: Arc::new(RwLock::new(HashMap::new())),
            events,
            session: Arc::new(RwLock::new(SessionApprovalSet::default())),
            persistence,
        })
    }

    /// Subscribe to runtime events.
    pub fn subscribe(&self) -> broadcast::Receiver<ApprovalRuntimeEvent> {
        self.events.subscribe()
    }

    /// Request approval for a tool call.
    /// Returns `(Some(id), rx)` for slow-path, `(None, rx)` for fast-path.
    pub async fn request(
        &self,
        source: ApprovalSource,
        tool: String,
        input: serde_json::Value,
        risk: ToolRisk,
        server_id: Option<&str>,
        path_hint: Option<&str>,
    ) -> (Option<RequestId>, oneshot::Receiver<ApprovalOutcome>) {
        {
            let mut set = self.session.write().await;
            if set.is_auto_approved(&tool, server_id, path_hint) {
                let (tx, rx) = oneshot::channel();
                let _ = tx.send(ApprovalOutcome::Once);
                return (None, rx);
            }
        }

        let id = RequestId::new();
        let (tx, rx) = oneshot::channel();
        let input_preview = summarize_for_display(&input, 140);
        let server_id_owned = server_id.map(|s| s.to_string());
        let path_hint_owned = path_hint.map(|s| s.to_string());
        let request = ApprovalRequest {
            id: id.clone(),
            source: source.clone(),
            tool: tool.clone(),
            input,
            risk,
            server_id: server_id_owned,
            path_hint: path_hint_owned,
        };
        self.pending.write().await.insert(id.clone(), PendingRecord { request, responder: tx });
        let _ = self.events.send(ApprovalRuntimeEvent::Requested {
            id: id.clone(),
            source: source.clone(),
            tool: tool.clone(),
            input_preview,
            risk,
        });
        (Some(id), rx)
    }

    /// Resolve a pending approval with an outcome.
    pub async fn resolve(&self, id: &RequestId, outcome: ApprovalOutcome) {
        let record = self.pending.write().await.remove(id);
        if let Some(record) = record {
            {
                let mut session = self.session.write().await;
                match &outcome {
                    ApprovalOutcome::Once => {}
                    ApprovalOutcome::AlwaysTool { tool_name } => {
                        session.always_tools.insert(tool_name.clone());
                    }
                    ApprovalOutcome::AlwaysServer { server_id } => {
                        session.always_servers.insert(server_id.clone());
                    }
                    ApprovalOutcome::AlwaysAndSave { scope } => {
                        match scope {
                            ApprovalScope::Tool(t) => {
                                session.always_tools.insert(t.clone());
                            }
                            ApprovalScope::Server(s) => {
                                session.always_servers.insert(s.clone());
                            }
                            ApprovalScope::PathPrefix(p) => {
                                if !session.always_paths.iter().any(|existing| existing == p) {
                                    session.always_paths.push(p.clone());
                                    session.invalidate_glob_cache();
                                }
                            }
                        }
                        if let Err(e) = self.persistence.save(scope.clone()).await {
                            tracing::warn!("failed to persist approval scope: {}", e);
                        }
                    }
                    ApprovalOutcome::Reject { .. } => {}
                    ApprovalOutcome::Cancel => {}
                }
            }

            {
                let mut session = self.session.write().await;
                let mut to_resolve: Vec<RequestId> = Vec::new();
                {
                    let pending = self.pending.read().await;
                    for (pid, rec) in pending.iter() {
                        let srv: Option<&str> = rec.request.server_id.as_deref();
                        let pth: Option<&str> = rec.request.path_hint.as_deref();
                        if session.is_auto_approved(&rec.request.tool, srv, pth) {
                            to_resolve.push(pid.clone());
                        }
                    }
                }
                drop(session);
                for pid in to_resolve {
                    if let Some(rec) = self.pending.write().await.remove(&pid) {
                        let _ = rec.responder.send(ApprovalOutcome::Once);
                        let _ = self.events.send(ApprovalRuntimeEvent::Resolved {
                            id: pid,
                            outcome: ApprovalOutcome::Once,
                        });
                    }
                }
            }

            let _ = record.responder.send(outcome.clone());
            let _ = self.events.send(ApprovalRuntimeEvent::Resolved {
                id: id.clone(),
                outcome,
            });
        }
    }

    /// Cancel all pending approvals matching the given source.
    pub async fn cancel_by_source(&self, source: &ApprovalSource, reason: CancelReason) {
        let ids: Vec<RequestId> = {
            let pending = self.pending.read().await;
            pending
                .iter()
                .filter(|(_, r)| r.request.source == *source)
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in ids {
            if let Some(record) = self.pending.write().await.remove(&id) {
                let _ = record.responder.send(ApprovalOutcome::Cancel);
                let _ = self.events.send(ApprovalRuntimeEvent::Cancelled {
                    id: id.clone(),
                    reason: reason.clone(),
                });
            }
        }
    }

    /// Snapshot the current session approval set (for `rustain doctor` + tests).
    pub async fn snapshot_session(&self) -> SessionApprovalSet {
        self.session.read().await.clone()
    }

    /// Load persisted approvals and merge into session set.
    pub async fn load_session(&self) {
        match self.persistence.load().await {
            Ok(loaded) => {
                let mut session = self.session.write().await;
                for tool in loaded.always_tools {
                    session.always_tools.insert(tool);
                }
                for server in loaded.always_servers {
                    session.always_servers.insert(server);
                }
                for path in loaded.always_paths {
                    if !session.always_paths.iter().any(|p| p == &path) {
                        session.always_paths.push(path);
                    }
                }
                session.invalidate_glob_cache();
            }
            Err(e) => {
                tracing::warn!("failed to load persisted approval rules: {}", e);
            }
        }
    }

    pub async fn seed_session(&self, seed: SessionApprovalSet) {
        let mut session = self.session.write().await;
        for tool in seed.always_tools {
            session.always_tools.insert(tool);
        }
        for server in seed.always_servers {
            session.always_servers.insert(server);
        }
        for path in seed.always_paths {
            if !session.always_paths.iter().any(|p| p == &path) {
                session.always_paths.push(path);
            }
        }
        session.invalidate_glob_cache();
    }
}

/// Truncate input JSON to a display-friendly preview.
fn summarize_for_display(input: &serde_json::Value, max_len: usize) -> String {
    let s = input.to_string();
    if s.chars().count() <= max_len {
        s
    } else {
        let truncated: String = s.chars().take(max_len).collect();
        format!("{}...", truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoOpPersistence;

    #[async_trait::async_trait]
    impl ApprovalPersistencePort for NoOpPersistence {
        async fn load(&self) -> Result<SessionApprovalSet, crate::domain::errors::ApprovalPersistenceError> {
            Ok(SessionApprovalSet::default())
        }
        async fn save(&self, _scope: ApprovalScope) -> Result<(), crate::domain::errors::ApprovalPersistenceError> {
            Ok(())
        }
    }

    fn make_runtime() -> Arc<ApprovalRuntime> {
        ApprovalRuntime::new(1024, Arc::new(NoOpPersistence))
    }

    #[tokio::test]
    async fn fast_path_tool_auto_approved() {
        let rt = make_runtime();
        rt.session.write().await.always_tools.insert("Read".into());
        let (id, rx) = rt.request(
            ApprovalSource::ForegroundTurn { conversation_id: "c1".into() },
            "Read".into(),
            serde_json::json!({"file_path": "/tmp/x"}),
            ToolRisk::Safe,
            None,
            None,
        ).await;
        assert!(id.is_none(), "fast-path should return None id");
        assert_eq!(rx.await.unwrap(), ApprovalOutcome::Once);
    }

    #[tokio::test]
    async fn slow_path_generates_unique_ids() {
        let rt = make_runtime();
        for _ in 0..100 {
            let (id, _rx) = rt.request(
                ApprovalSource::ForegroundTurn { conversation_id: "c1".into() },
                "Bash".into(),
                serde_json::json!({"command": "echo hi"}),
                ToolRisk::Elevated,
                None,
                None,
            ).await;
            assert!(id.is_some());
        }
        assert_eq!(rt.pending.read().await.len(), 100);
    }

    #[tokio::test]
    async fn resolve_side_effects() {
        let rt = make_runtime();
        let (id, rx) = rt.request(
            ApprovalSource::ForegroundTurn { conversation_id: "c1".into() },
            "Bash".into(),
            serde_json::json!({"command": "echo hi"}),
            ToolRisk::Elevated,
            None,
            None,
        ).await;
        let mut events = rt.subscribe();
        rt.resolve(id.as_ref().unwrap(), ApprovalOutcome::AlwaysTool { tool_name: "Bash".into() }).await;
        assert_eq!(rx.await.unwrap(), ApprovalOutcome::AlwaysTool { tool_name: "Bash".into() });
        let snapshot = rt.snapshot_session().await;
        assert!(snapshot.always_tools.contains("Bash"));
        let ev = events.recv().await.unwrap();
        assert!(matches!(ev, ApprovalRuntimeEvent::Resolved { .. }));
    }

    #[tokio::test]
    async fn cancel_by_source_drains_matching() {
        let rt = make_runtime();
        let (_id1, _rx1) = rt.request(
            ApprovalSource::ForegroundTurn { conversation_id: "c1".into() },
            "Bash".into(),
            serde_json::json!({"command": "echo hi"}),
            ToolRisk::Elevated,
            None,
            None,
        ).await;
        let (_id2, rx2) = rt.request(
            ApprovalSource::ForegroundSubagent { conversation_id: "c1".into(), parent_tool_call_id: "t1".into(), subagent_type: "code-reviewer".into() },
            "Bash".into(),
            serde_json::json!({"command": "echo hi2"}),
            ToolRisk::Elevated,
            None,
            None,
        ).await;
        rt.cancel_by_source(&ApprovalSource::ForegroundSubagent {
            conversation_id: "c1".into(),
            parent_tool_call_id: "t1".into(),
            subagent_type: "code-reviewer".into(),
        }, CancelReason::SourceAborted).await;
        assert_eq!(rx2.await.unwrap(), ApprovalOutcome::Cancel);
        assert!(!rt.pending.read().await.is_empty());
    }
}
