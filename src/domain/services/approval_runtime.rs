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
//! - Story 19.20c `A3`: the exact input lives only in the pending record.
//!   `Requested` carries no argument values; a deciding surface reads the
//!   input through [`ApprovalRuntime::pending_review`], which validates and
//!   acquires under one pending-state lock acquisition.
//! - Story 19.20c `P5`: `cancel_all` irreversibly shuts down request admission.
//!   Later requests return Cancel without pending state or broadcast. Admission
//!   rechecks shutdown under the pending write lock, so it cannot race the drain.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use tokio::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard, broadcast, oneshot};

use crate::domain::models::ToolRisk;
use crate::domain::models::tool_call::{ApprovalSource, RequestId};
use crate::domain::models::{ApprovalOutcome, ApprovalScope, AutoApprovePolicy};
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
    pub fn is_auto_approved(
        &mut self,
        tool: &str,
        server: Option<&str>,
        path: Option<&str>,
    ) -> bool {
        if self.always_tools.contains(tool) {
            return true;
        }
        if server
            .map(|s| self.always_servers.contains(s))
            .unwrap_or(false)
        {
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
///
/// `Deserialize` (Story 12.2b): forwarded over the daemon attach protocol inside
/// [`crate::infrastructure::runtime::event_bus::RawEventKind::Approval`]; the
/// attach client deserializes it to render the permission card.
///
/// Story 19.20c `P3`: `Requested` carries no argument values — id, source, tool
/// and risk only. A deciding surface reads the exact input through
/// [`ApprovalRuntime::pending_review`]. Sender-consent requests keep their
/// card text in `consent_card` (unchanged copy).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum ApprovalRuntimeEvent {
    Requested {
        id: RequestId,
        source: ApprovalSource,
        tool: String,
        risk: ToolRisk,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        consent_card: Option<String>,
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

/// Reason an approval was cancelled. Story 19.20c `P5`: no reading deadline
/// exists, so there is no timeout reason.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum CancelReason {
    SourceAborted,
    RuntimeShutdown,
}

/// Internal record for a pending approval.
struct PendingRecord {
    request: ApprovalRequest,
    responder: oneshot::Sender<ResolvedApproval>,
    /// Server-side fingerprint computed at request time.
    /// The verifier never rides the wire on the thing it verifies (Winston).
    fp: crate::domain::models::invocation_fingerprint::InvocationFingerprint,
    /// Monotonic request order (Story 19.20c: lag rescan and reissue keep it).
    seq: u64,
}

/// In-memory wrapper carrying the approved fingerprint alongside the outcome.
///
/// Replaces the bare `ApprovalOutcome` on the `PendingRecord.responder` oneshot
/// so `run_one` can re-match the fingerprint before executing. This keeps the
/// fp off the wire and off the `Serialize`-shaped `ApprovalOutcome` enum.
/// (DD1 compile-coupling — Amelia; REFRESH 2026-07-02 shape correction.)
#[derive(Clone, Debug)]
pub struct ResolvedApproval {
    pub outcome: ApprovalOutcome,
    pub fp: crate::domain::models::invocation_fingerprint::InvocationFingerprint,
}

/// Internal request struct.
#[allow(dead_code)]
struct ApprovalRequest {
    id: RequestId,
    source: ApprovalSource,
    tool: String,
    /// The exact input, shared immutably with review snapshots (`A3`).
    input: Arc<serde_json::Value>,
    risk: ToolRisk,
    server_id: Option<String>,
    path_hint: Option<String>,
}

/// Story 19.20c `A3` — the private pending snapshot a deciding surface reads.
///
/// `input` is the exact pending `serde_json::Value`, shared rather than
/// re-cloned per frame. It is presentation input only; the scheduler still
/// executes, and the fingerprint still covers, its own copy.
#[derive(Clone, Debug)]
pub struct PendingReview {
    pub id: RequestId,
    pub source: ApprovalSource,
    pub tool: String,
    pub risk: ToolRisk,
    pub input: Arc<serde_json::Value>,
}

/// Metadata of one pending tool approval — no argument values (lag rescan).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingApprovalMeta {
    pub id: RequestId,
    pub source: ApprovalSource,
    pub tool: String,
    pub risk: ToolRisk,
}

/// Which pending tool approvals a reader may see. Sender-consent requests are
/// never tool approvals and match no scope.
#[derive(Clone, Copy, Debug)]
pub enum ReviewScope<'a> {
    /// Every tool approval the runtime holds (standalone TUI, daemon gate).
    AllTools,
    /// Only requests raised by this ACP session.
    AcpSession(&'a str),
}

impl ReviewScope<'_> {
    fn admits(self, request: &ApprovalRequest) -> bool {
        if is_sender_consent(&request.tool, &request.source) {
            return false;
        }
        match self {
            Self::AllTools => true,
            Self::AcpSession(session) => matches!(
                &request.source,
                ApprovalSource::AcpSession { session_id, .. } if session_id == session
            ),
        }
    }
}

fn is_sender_consent(tool: &str, source: &ApprovalSource) -> bool {
    tool == "a2a/sender-consent" && matches!(source, ApprovalSource::RemotePeer { .. })
}

/// The pending table. Every acquisition goes through [`Self::read`] /
/// [`Self::write`] so the Rule 4 ratchet can count them.
#[derive(Default)]
struct PendingTable {
    inner: RwLock<HashMap<RequestId, PendingRecord>>,
    #[cfg(any(test, feature = "test-instrumentation"))]
    acquisitions: AtomicUsize,
}

impl PendingTable {
    fn note_acquisition(&self) {
        #[cfg(any(test, feature = "test-instrumentation"))]
        self.acquisitions.fetch_add(1, Ordering::SeqCst);
    }

    async fn read(&self) -> RwLockReadGuard<'_, HashMap<RequestId, PendingRecord>> {
        self.note_acquisition();
        self.inner.read().await
    }

    async fn write(&self) -> RwLockWriteGuard<'_, HashMap<RequestId, PendingRecord>> {
        self.note_acquisition();
        self.inner.write().await
    }
}

/// Pub/sub runtime for tool-call approvals.
pub struct ApprovalRuntime {
    pending: PendingTable,
    events: broadcast::Sender<ApprovalRuntimeEvent>,
    session: Arc<RwLock<SessionApprovalSet>>,
    persistence: Arc<dyn ApprovalPersistencePort>,
    subagent_auto_approve: AutoApprovePolicy,
    rejected_count: AtomicUsize,
    next_seq: AtomicU64,
    shut_down: AtomicBool,
}
impl ApprovalRuntime {
    /// Construct a new runtime. `event_capacity` defaults to 1024 when 0.
    /// Defaults `subagent_auto_approve` to `Ask` (preserves existing behavior).
    pub fn new(event_capacity: usize, persistence: Arc<dyn ApprovalPersistencePort>) -> Arc<Self> {
        Self::new_with_subagent_policy(event_capacity, persistence, AutoApprovePolicy::Ask)
    }

    /// Construct a new runtime with an explicit subagent auto-approve policy.
    pub fn new_with_subagent_policy(
        event_capacity: usize,
        persistence: Arc<dyn ApprovalPersistencePort>,
        subagent_auto_approve: AutoApprovePolicy,
    ) -> Arc<Self> {
        let cap = if event_capacity == 0 {
            tracing::warn!("ApprovalRuntime event_capacity was 0, falling back to 1024");
            1024
        } else {
            event_capacity
        };
        let (events, _) = broadcast::channel(cap);
        Arc::new(Self {
            pending: PendingTable::default(),
            events,
            session: Arc::new(RwLock::new(SessionApprovalSet::default())),
            persistence,
            subagent_auto_approve,
            rejected_count: AtomicUsize::new(0),
            next_seq: AtomicU64::new(0),
            shut_down: AtomicBool::new(false),
        })
    }

    /// Return the number of approvals resolved with [`ApprovalOutcome::Reject`].
    /// This is the authoritative count for CLI auto-deny summaries; it is updated
    /// inside `resolve`, so lagged broadcast consumers cannot under-report it.
    pub fn rejected_count(&self) -> usize {
        self.rejected_count.load(Ordering::Relaxed)
    }

    /// Subscribe to runtime events.
    pub fn subscribe(&self) -> broadcast::Receiver<ApprovalRuntimeEvent> {
        self.events.subscribe()
    }

    /// Pending sender-consent requests (tool `a2a/sender-consent`) with the
    /// card text their `Requested` event carried (F10). The approval gate
    /// uses this after a broadcast `Lagged` to re-render cards the bus
    /// dropped, so a never-expiring consent request cannot hang silently with
    /// no card on screen. Story 19.18d `P22`: the card comes from the
    /// request's stored input through the same derivation `request` used, so a
    /// re-render never re-derives copy (a bound card stays a bound card).
    pub async fn pending_sender_consent(&self) -> Vec<(RequestId, String, ToolRisk)> {
        let pending = self.pending.read().await;
        let mut consent: Vec<_> = pending
            .values()
            .filter(|record| is_sender_consent(&record.request.tool, &record.request.source))
            .map(|record| {
                (
                    record.seq,
                    (
                        record.request.id.clone(),
                        sender_consent_card(&record.request.tool, &record.request.input),
                        record.request.risk,
                    ),
                )
            })
            .collect();
        consent.sort_by_key(|(seq, _)| *seq);
        consent.into_iter().map(|(_, entry)| entry).collect()
    }

    /// Story 19.20c `A3` — the exact pending input for one tool approval.
    ///
    /// Matches the exact request id **and** `scope`; validation and acquisition
    /// happen under one pending-state lock acquisition, released before the
    /// caller formats, transports or awaits anything. Returns `None` once the
    /// request resolved, was cancelled, is a sender-consent card, or is outside
    /// `scope`.
    pub async fn pending_review(
        &self,
        id: &RequestId,
        scope: ReviewScope<'_>,
    ) -> Option<PendingReview> {
        let pending = self.pending.read().await;
        let record = pending.get(id)?;
        if !scope.admits(&record.request) {
            return None;
        }
        Some(PendingReview {
            id: record.request.id.clone(),
            source: record.request.source.clone(),
            tool: record.request.tool.clone(),
            risk: record.request.risk,
            input: Arc::clone(&record.request.input),
        })
    }

    /// Story 19.20c `A3`/`P5` — metadata (no argument values) of every pending
    /// tool approval in `scope`, in request order. Lag recovery rescans with it.
    pub async fn pending_tool_requests(&self, scope: ReviewScope<'_>) -> Vec<PendingApprovalMeta> {
        let pending = self.pending.read().await;
        let mut requests: Vec<_> = pending
            .values()
            .filter(|record| scope.admits(&record.request))
            .map(|record| {
                (
                    record.seq,
                    PendingApprovalMeta {
                        id: record.request.id.clone(),
                        source: record.request.source.clone(),
                        tool: record.request.tool.clone(),
                        risk: record.request.risk,
                    },
                )
            })
            .collect();
        requests.sort_by_key(|(seq, _)| *seq);
        requests.into_iter().map(|(_, meta)| meta).collect()
    }

    /// Rule 4 ratchet (Story 19.20c `K07`): pending-state lock acquisitions
    /// since construction.
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub fn pending_lock_acquisitions(&self) -> usize {
        self.pending.acquisitions.load(Ordering::SeqCst)
    }

    /// Request approval for a tool call.
    /// Returns `(Some(id), rx)` for slow-path, `(None, rx)` for fast-path.
    ///
    /// The fingerprint is computed at request time and stored server-side in
    /// `PendingRecord`. Fast-path returns (auto-approve / session-allow / deny)
    /// also carry the fp so `run_one` can re-match uniformly.
    pub async fn request(
        &self,
        source: ApprovalSource,
        tool: String,
        input: serde_json::Value,
        risk: ToolRisk,
        server_id: Option<&str>,
        path_hint: Option<&str>,
    ) -> (Option<RequestId>, oneshot::Receiver<ResolvedApproval>) {
        use crate::domain::models::invocation_fingerprint::InvocationFingerprint;

        // Compute the fingerprint up front for all paths.
        // Non-finite floats in the input are a hard reject (ADR-14-6-01).
        let scope = source.scope_agent_id();
        let fp = match InvocationFingerprint::of(&tool, &input, &scope) {
            Ok(fp) => fp,
            Err(_) => {
                let (tx, rx) = oneshot::channel();
                let _ = tx.send(ResolvedApproval {
                    outcome: ApprovalOutcome::Reject {
                        feedback: Some(
                            "input contains non-finite float; cannot fingerprint".into(),
                        ),
                    },
                    fp: InvocationFingerprint([0u8; 32]),
                });
                return (None, rx);
            }
        };
        if self.shut_down.load(Ordering::Acquire) {
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(ResolvedApproval {
                outcome: ApprovalOutcome::Cancel,
                fp,
            });
            return (None, rx);
        }

        // Story 10.X-AUTO: subagent auto-approve policy short-circuit.
        // Placed BEFORE the session fast-path so "deny" wins over session-allow.
        match &source {
            ApprovalSource::ForegroundSubagent { .. } | ApprovalSource::BackgroundAgent { .. } => {
                match self.subagent_auto_approve {
                    AutoApprovePolicy::Deny => {
                        tracing::info!(tool = %tool, "subagent_auto_approve=deny: rejecting subagent tool call");
                        let (tx, rx) = oneshot::channel();
                        let _ = tx.send(ResolvedApproval {
                            outcome: ApprovalOutcome::Reject {
                                feedback: Some("subagent_auto_approve=deny".into()),
                            },
                            fp,
                        });
                        return (None, rx);
                    }
                    AutoApprovePolicy::Allow => {
                        tracing::debug!(tool = %tool, "subagent_auto_approve=allow: auto-approving subagent tool call");
                        let (tx, rx) = oneshot::channel();
                        let _ = tx.send(ResolvedApproval {
                            outcome: ApprovalOutcome::Once,
                            fp,
                        });
                        return (None, rx);
                    }
                    AutoApprovePolicy::Ask => {
                        // fall through to existing behavior
                    }
                }
            }
            ApprovalSource::ForegroundTurn { .. }
            | ApprovalSource::AcpSession { .. }
            | ApprovalSource::RemotePeer { .. } => {
                // User and peer-originated requests never inherit subagent
                // auto-approval policy.
            }
        }

        let sender_consent_request = is_sender_consent(&tool, &source);
        if !sender_consent_request {
            let mut set = self.session.write().await;
            if self.shut_down.load(Ordering::Acquire) {
                let (tx, rx) = oneshot::channel();
                let _ = tx.send(ResolvedApproval {
                    outcome: ApprovalOutcome::Cancel,
                    fp,
                });
                return (None, rx);
            }
            if set.is_auto_approved(&tool, server_id, path_hint) {
                let (tx, rx) = oneshot::channel();
                let _ = tx.send(ResolvedApproval {
                    outcome: ApprovalOutcome::Once,
                    fp,
                });
                return (None, rx);
            }
        }

        let consent_card = sender_consent_request.then(|| sender_consent_card(&tool, &input));
        let server_id_owned = server_id.map(|s| s.to_string());
        let path_hint_owned = path_hint.map(|s| s.to_string());
        let mut pending = self.pending.write().await;
        if self.shut_down.load(Ordering::Acquire) {
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(ResolvedApproval {
                outcome: ApprovalOutcome::Cancel,
                fp,
            });
            return (None, rx);
        }
        let id = RequestId::new();
        let (tx, rx) = oneshot::channel();
        let request = ApprovalRequest {
            id: id.clone(),
            source: source.clone(),
            tool: tool.clone(),
            input: Arc::new(input),
            risk,
            server_id: server_id_owned,
            path_hint: path_hint_owned,
        };
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        pending.insert(
            id.clone(),
            PendingRecord {
                request,
                responder: tx,
                fp,
                seq,
            },
        );
        drop(pending);
        let _ = self.events.send(ApprovalRuntimeEvent::Requested {
            id: id.clone(),
            source,
            tool,
            risk,
            consent_card,
        });
        (Some(id), rx)
    }

    /// Return the immutable source of a still-pending request.
    ///
    /// The daemon consent shell uses this to derive the authenticated peer for
    /// durable `[a]` handling; the client never supplies journal identity.
    pub(crate) async fn pending_source(&self, id: &RequestId) -> Option<ApprovalSource> {
        self.pending
            .read()
            .await
            .get(id)
            .map(|record| record.request.source.clone())
    }

    /// Resolve a pending approval with an outcome.
    pub async fn resolve(&self, id: &RequestId, outcome: ApprovalOutcome) {
        let record = self.pending.write().await.remove(id);
        if let Some(record) = record {
            // `[a]` on the sender-consent card is persisted by the consent
            // manager as a peer-scoped journal grant. It must not leak into
            // ApprovalRuntime's tool-wide session/persistence policy.
            let session_outcome = if record.request.tool == "a2a/sender-consent"
                && matches!(&record.request.source, ApprovalSource::RemotePeer { .. })
                && matches!(
                    &outcome,
                    ApprovalOutcome::AlwaysTool { .. }
                        | ApprovalOutcome::AlwaysServer { .. }
                        | ApprovalOutcome::AlwaysAndSave { .. }
                ) {
                ApprovalOutcome::Once
            } else {
                outcome.clone()
            };
            {
                let mut session = self.session.write().await;
                match &session_outcome {
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
                    ApprovalOutcome::Reject { .. } => {
                        self.rejected_count.fetch_add(1, Ordering::Relaxed);
                    }
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
                        let _ = rec.responder.send(ResolvedApproval {
                            outcome: ApprovalOutcome::Once,
                            fp: rec.fp,
                        });
                        let _ = self.events.send(ApprovalRuntimeEvent::Resolved {
                            id: pid,
                            outcome: ApprovalOutcome::Once,
                        });
                    }
                }
            }

            let _ = record.responder.send(ResolvedApproval {
                outcome: outcome.clone(),
                fp: record.fp,
            });
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
                let _ = record.responder.send(ResolvedApproval {
                    outcome: ApprovalOutcome::Cancel,
                    fp: record.fp,
                });
                let _ = self.events.send(ApprovalRuntimeEvent::Cancelled {
                    id: id.clone(),
                    reason: reason.clone(),
                });
            }
        }
    }

    /// Story 19.20c `P5` — cancel every pending approval (daemon shutdown).
    /// Each responder receives `Cancel`, so the waiting scheduler executes
    /// nothing, and a late decision finds no record. Admission stays closed:
    /// even a turn queued behind an old request cannot create a new responder.
    pub async fn cancel_all(&self, reason: CancelReason) {
        self.shut_down.store(true, Ordering::Release);
        let records: Vec<(RequestId, PendingRecord)> = {
            let mut pending = self.pending.write().await;
            let mut drained: Vec<_> = pending.drain().collect();
            drained.sort_by_key(|(_, record)| record.seq);
            drained
        };
        for (id, record) in records {
            let _ = record.responder.send(ResolvedApproval {
                outcome: ApprovalOutcome::Cancel,
                fp: record.fp,
            });
            let _ = self.events.send(ApprovalRuntimeEvent::Cancelled {
                id,
                reason: reason.clone(),
            });
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

/// Story 19.18d `P22` — the one derivation of a sender-consent request's
/// card: the card the requester rendered, else the shared per-tool summary
/// (Story 19.20c removed the 140-character JSON preview). Both the
/// `Requested` event and the `Lagged` re-render read it, so the two can never
/// show different copy.
fn sender_consent_card(tool: &str, input: &serde_json::Value) -> String {
    input
        .get("card")
        .and_then(serde_json::Value::as_str)
        .map_or_else(
            || crate::domain::services::approval_presentation::summary(tool, input, false),
            str::to_owned,
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoOpPersistence;

    #[async_trait::async_trait]
    impl ApprovalPersistencePort for NoOpPersistence {
        async fn load(
            &self,
        ) -> Result<SessionApprovalSet, crate::domain::errors::ApprovalPersistenceError> {
            Ok(SessionApprovalSet::default())
        }
        async fn save(
            &self,
            _scope: ApprovalScope,
        ) -> Result<(), crate::domain::errors::ApprovalPersistenceError> {
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
        let (id, rx) = rt
            .request(
                ApprovalSource::ForegroundTurn {
                    conversation_id: "c1".into(),
                },
                "Read".into(),
                serde_json::json!({"file_path": "/tmp/x"}),
                ToolRisk::Safe,
                None,
                None,
            )
            .await;
        assert!(id.is_none(), "fast-path should return None id");
        assert_eq!(rx.await.unwrap().outcome, ApprovalOutcome::Once);
    }

    #[tokio::test]
    async fn slow_path_generates_unique_ids() {
        let rt = make_runtime();
        for _ in 0..100 {
            let (id, _rx) = rt
                .request(
                    ApprovalSource::ForegroundTurn {
                        conversation_id: "c1".into(),
                    },
                    "Bash".into(),
                    serde_json::json!({"command": "echo hi"}),
                    ToolRisk::Elevated,
                    None,
                    None,
                )
                .await;
            assert!(id.is_some());
        }
        assert_eq!(rt.pending.read().await.len(), 100);
    }

    #[tokio::test]
    async fn resolve_side_effects() {
        let rt = make_runtime();
        let (id, rx) = rt
            .request(
                ApprovalSource::ForegroundTurn {
                    conversation_id: "c1".into(),
                },
                "Bash".into(),
                serde_json::json!({"command": "echo hi"}),
                ToolRisk::Elevated,
                None,
                None,
            )
            .await;
        let mut events = rt.subscribe();
        rt.resolve(
            id.as_ref().unwrap(),
            ApprovalOutcome::AlwaysTool {
                tool_name: "Bash".into(),
            },
        )
        .await;
        let resolved = rx.await.unwrap();
        assert_eq!(
            resolved.outcome,
            ApprovalOutcome::AlwaysTool {
                tool_name: "Bash".into()
            }
        );
        let snapshot = rt.snapshot_session().await;
        assert!(snapshot.always_tools.contains("Bash"));
        let ev = events.recv().await.unwrap();
        assert!(matches!(ev, ApprovalRuntimeEvent::Resolved { .. }));
    }

    #[tokio::test]
    async fn cancel_by_source_drains_matching() {
        let rt = make_runtime();
        let (_id1, _rx1) = rt
            .request(
                ApprovalSource::ForegroundTurn {
                    conversation_id: "c1".into(),
                },
                "Bash".into(),
                serde_json::json!({"command": "echo hi"}),
                ToolRisk::Elevated,
                None,
                None,
            )
            .await;
        let (_id2, rx2) = rt
            .request(
                ApprovalSource::ForegroundSubagent {
                    conversation_id: "c1".into(),
                    parent_tool_call_id: "t1".into(),
                    subagent_type: "code-reviewer".into(),
                },
                "Bash".into(),
                serde_json::json!({"command": "echo hi2"}),
                ToolRisk::Elevated,
                None,
                None,
            )
            .await;
        rt.cancel_by_source(
            &ApprovalSource::ForegroundSubagent {
                conversation_id: "c1".into(),
                parent_tool_call_id: "t1".into(),
                subagent_type: "code-reviewer".into(),
            },
            CancelReason::SourceAborted,
        )
        .await;
        assert_eq!(rx2.await.unwrap().outcome, ApprovalOutcome::Cancel);
        assert!(!rt.pending.read().await.is_empty());
    }

    #[tokio::test]
    async fn subagent_deny_returns_none_and_reject() {
        let rt = ApprovalRuntime::new_with_subagent_policy(
            1024,
            Arc::new(NoOpPersistence),
            AutoApprovePolicy::Deny,
        );
        let mut events = rt.subscribe();
        let (id, rx) = rt
            .request(
                ApprovalSource::ForegroundSubagent {
                    conversation_id: "c1".into(),
                    parent_tool_call_id: "t1".into(),
                    subagent_type: "code-reviewer".into(),
                },
                "Bash".into(),
                serde_json::json!({"command": "echo hi"}),
                ToolRisk::Elevated,
                None,
                None,
            )
            .await;
        assert!(id.is_none(), "deny should return None id");
        assert_eq!(
            rx.await.unwrap().outcome,
            ApprovalOutcome::Reject {
                feedback: Some("subagent_auto_approve=deny".into()),
            }
        );
        assert!(
            matches!(
                events.try_recv(),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            ),
            "deny must NOT broadcast Requested"
        );
    }

    #[tokio::test]
    async fn subagent_allow_returns_none_and_once() {
        let rt = ApprovalRuntime::new_with_subagent_policy(
            1024,
            Arc::new(NoOpPersistence),
            AutoApprovePolicy::Allow,
        );
        let mut events = rt.subscribe();
        let (id, rx) = rt
            .request(
                ApprovalSource::ForegroundSubagent {
                    conversation_id: "c1".into(),
                    parent_tool_call_id: "t1".into(),
                    subagent_type: "code-reviewer".into(),
                },
                "Bash".into(),
                serde_json::json!({"command": "echo hi"}),
                ToolRisk::Elevated,
                None,
                None,
            )
            .await;
        assert!(id.is_none(), "allow should return None id");
        assert_eq!(rx.await.unwrap().outcome, ApprovalOutcome::Once);
        assert!(
            matches!(
                events.try_recv(),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            ),
            "allow must NOT broadcast Requested"
        );
    }

    #[tokio::test]
    async fn subagent_ask_returns_some_and_broadcasts() {
        let rt = ApprovalRuntime::new_with_subagent_policy(
            1024,
            Arc::new(NoOpPersistence),
            AutoApprovePolicy::Ask,
        );
        let mut events = rt.subscribe();
        let (id, _rx) = rt
            .request(
                ApprovalSource::ForegroundSubagent {
                    conversation_id: "c1".into(),
                    parent_tool_call_id: "t1".into(),
                    subagent_type: "code-reviewer".into(),
                },
                "Bash".into(),
                serde_json::json!({"command": "echo hi"}),
                ToolRisk::Elevated,
                None,
                None,
            )
            .await;
        assert!(id.is_some(), "ask should return Some id");
        let ev = events.recv().await.unwrap();
        assert!(
            matches!(ev, ApprovalRuntimeEvent::Requested { .. }),
            "ask must broadcast Requested"
        );
    }

    #[tokio::test]
    async fn foreground_turn_ignores_deny_policy() {
        let rt = ApprovalRuntime::new_with_subagent_policy(
            1024,
            Arc::new(NoOpPersistence),
            AutoApprovePolicy::Deny,
        );
        let mut events = rt.subscribe();
        let (id, _rx) = rt
            .request(
                ApprovalSource::ForegroundTurn {
                    conversation_id: "c1".into(),
                },
                "Bash".into(),
                serde_json::json!({"command": "echo hi"}),
                ToolRisk::Elevated,
                None,
                None,
            )
            .await;
        assert!(
            id.is_some(),
            "ForegroundTurn with deny must still return Some id"
        );
        let ev = events.recv().await.unwrap();
        assert!(
            matches!(ev, ApprovalRuntimeEvent::Requested { .. }),
            "ForegroundTurn must still broadcast Requested"
        );
    }

    #[tokio::test]
    async fn background_agent_deny_returns_none_and_reject() {
        let rt = ApprovalRuntime::new_with_subagent_policy(
            1024,
            Arc::new(NoOpPersistence),
            AutoApprovePolicy::Deny,
        );
        let mut events = rt.subscribe();
        let (id, rx) = rt
            .request(
                ApprovalSource::BackgroundAgent {
                    conversation_id: "c1".into(),
                    task_id: "task-1".into(),
                    subagent_type: "background-worker".into(),
                },
                "Bash".into(),
                serde_json::json!({"command": "echo hi"}),
                ToolRisk::Elevated,
                None,
                None,
            )
            .await;
        assert!(
            id.is_none(),
            "deny should return None id for BackgroundAgent"
        );
        assert_eq!(
            rx.await.unwrap().outcome,
            ApprovalOutcome::Reject {
                feedback: Some("subagent_auto_approve=deny".into()),
            }
        );
        assert!(
            matches!(
                events.try_recv(),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            ),
            "deny must NOT broadcast Requested for BackgroundAgent"
        );
    }

    #[tokio::test]
    async fn background_agent_allow_returns_none_and_once() {
        let rt = ApprovalRuntime::new_with_subagent_policy(
            1024,
            Arc::new(NoOpPersistence),
            AutoApprovePolicy::Allow,
        );
        let mut events = rt.subscribe();
        let (id, rx) = rt
            .request(
                ApprovalSource::BackgroundAgent {
                    conversation_id: "c1".into(),
                    task_id: "task-1".into(),
                    subagent_type: "background-worker".into(),
                },
                "Bash".into(),
                serde_json::json!({"command": "echo hi"}),
                ToolRisk::Elevated,
                None,
                None,
            )
            .await;
        assert!(
            id.is_none(),
            "allow should return None id for BackgroundAgent"
        );
        assert_eq!(rx.await.unwrap().outcome, ApprovalOutcome::Once);
        assert!(
            matches!(
                events.try_recv(),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            ),
            "allow must NOT broadcast Requested for BackgroundAgent"
        );
    }

    fn foreground(conversation: &str) -> ApprovalSource {
        ApprovalSource::ForegroundTurn {
            conversation_id: conversation.into(),
        }
    }

    fn acp(session: &str) -> ApprovalSource {
        ApprovalSource::AcpSession {
            session_id: session.into(),
            conversation_id: format!("conv-{session}"),
        }
    }

    /// Story 19.20c `M18`: `Requested` carries no argument value; the exact
    /// input (whitespace, Unicode, late suffix) is reachable only through the
    /// scoped snapshot, byte-for-byte.
    #[tokio::test]
    async fn requested_is_value_free_and_snapshot_is_exact() {
        let rt = make_runtime();
        let mut events = rt.subscribe();
        let marker = "SECRET-LATE-SUFFIX-ä漢🙂";
        let input = serde_json::json!({
            "command": format!("{}  \t {marker}", "x".repeat(200)),
        });
        let (id, _rx) = rt
            .request(
                foreground("c1"),
                "Bash".into(),
                input.clone(),
                ToolRisk::Elevated,
                None,
                None,
            )
            .await;
        let id = id.expect("slow path");
        let event = events.recv().await.unwrap();
        let wire = serde_json::to_string(&event).unwrap();
        assert!(!wire.contains("SECRET-LATE-SUFFIX"), "{wire}");
        assert!(!wire.contains("xxxx"), "{wire}");
        let review = rt
            .pending_review(&id, ReviewScope::AllTools)
            .await
            .expect("pending");
        assert_eq!(*review.input, input);
        assert_eq!(review.tool, "Bash");
    }

    /// ACP scope admits only its own session; resolution retires the snapshot.
    #[tokio::test]
    async fn snapshot_is_scoped_and_retires_with_the_request() {
        let rt = make_runtime();
        let (own, _own_rx) = rt
            .request(
                acp("s1"),
                "Bash".into(),
                serde_json::json!({"command": "ls"}),
                ToolRisk::Elevated,
                None,
                None,
            )
            .await;
        let (other, _other_rx) = rt
            .request(
                acp("s2"),
                "Bash".into(),
                serde_json::json!({"command": "rm"}),
                ToolRisk::Elevated,
                None,
                None,
            )
            .await;
        let (own, other) = (own.unwrap(), other.unwrap());
        assert!(
            rt.pending_review(&own, ReviewScope::AcpSession("s1"))
                .await
                .is_some()
        );
        assert!(
            rt.pending_review(&other, ReviewScope::AcpSession("s1"))
                .await
                .is_none()
        );
        let scoped = rt
            .pending_tool_requests(ReviewScope::AcpSession("s1"))
            .await;
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].id, own);
        rt.resolve(&own, ApprovalOutcome::Once).await;
        assert!(
            rt.pending_review(&own, ReviewScope::AllTools)
                .await
                .is_none()
        );
    }

    /// Sender-consent cards are not tool approvals: no snapshot, no rescan
    /// entry, and their card text stays on `Requested`.
    #[tokio::test]
    async fn sender_consent_is_never_a_tool_review() {
        let rt = make_runtime();
        let mut events = rt.subscribe();
        let source = ApprovalSource::RemotePeer {
            conversation_id: "c1".into(),
            peer_id: crate::domain::models::peer_identity::PeerId::from_public_key(&[7u8; 32])
                .unwrap(),
            principal: None,
        };
        let (id, _rx) = rt
            .request(
                source,
                "a2a/sender-consent".into(),
                serde_json::json!({"card": "Consent required\n[y] Allow once"}),
                ToolRisk::Elevated,
                None,
                None,
            )
            .await;
        let id = id.unwrap();
        match events.recv().await.unwrap() {
            ApprovalRuntimeEvent::Requested { consent_card, .. } => {
                assert_eq!(
                    consent_card.as_deref(),
                    Some("Consent required\n[y] Allow once")
                );
            }
            other => panic!("{other:?}"),
        }
        assert!(
            rt.pending_review(&id, ReviewScope::AllTools)
                .await
                .is_none()
        );
        assert!(
            rt.pending_tool_requests(ReviewScope::AllTools)
                .await
                .is_empty()
        );
        assert_eq!(rt.pending_sender_consent().await.len(), 1);
    }

    /// Rule 4 ratchet (`K07`/`M29`): the snapshot validates and acquires under
    /// exactly one pending-state lock acquisition — a validate-then-read split
    /// counts two.
    #[tokio::test]
    async fn pending_review_acquires_the_pending_lock_exactly_once() {
        let rt = make_runtime();
        let (id, _rx) = rt
            .request(
                foreground("c1"),
                "Write".into(),
                serde_json::json!({"file_path": "/w/a", "content": "b"}),
                ToolRisk::Elevated,
                None,
                None,
            )
            .await;
        let id = id.unwrap();
        let before = rt.pending_lock_acquisitions();
        assert!(
            rt.pending_review(&id, ReviewScope::AllTools)
                .await
                .is_some()
        );
        assert_eq!(rt.pending_lock_acquisitions() - before, 1);
        let before = rt.pending_lock_acquisitions();
        assert!(
            rt.pending_review(&RequestId("missing".into()), ReviewScope::AllTools)
                .await
                .is_none()
        );
        assert_eq!(rt.pending_lock_acquisitions() - before, 1);
    }

    /// Shutdown cancels every responder in request order; nothing stays
    /// pending, so a late decision resolves nothing.
    #[tokio::test]
    async fn cancel_all_cancels_every_responder() {
        let rt = make_runtime();
        let mut events = rt.subscribe();
        let mut receivers = Vec::new();
        for command in ["one", "two"] {
            let (_id, rx) = rt
                .request(
                    foreground("c1"),
                    "Bash".into(),
                    serde_json::json!({ "command": command }),
                    ToolRisk::Elevated,
                    None,
                    None,
                )
                .await;
            receivers.push(rx);
        }
        let first = match events.recv().await.unwrap() {
            ApprovalRuntimeEvent::Requested { id, .. } => id,
            other => panic!("{other:?}"),
        };
        let _ = events.recv().await.unwrap();
        rt.cancel_all(CancelReason::RuntimeShutdown).await;
        for rx in receivers {
            assert_eq!(rx.await.unwrap().outcome, ApprovalOutcome::Cancel);
        }
        match events.recv().await.unwrap() {
            ApprovalRuntimeEvent::Cancelled {
                id,
                reason: CancelReason::RuntimeShutdown,
            } => assert_eq!(id, first),
            other => panic!("{other:?}"),
        }
        assert!(
            rt.pending_tool_requests(ReviewScope::AllTools)
                .await
                .is_empty()
        );
        rt.resolve(&first, ApprovalOutcome::Once).await;
        assert_eq!(rt.rejected_count(), 0);
    }

    #[tokio::test]
    async fn request_after_shutdown_cancels_without_pending_or_broadcast() {
        let rt = make_runtime();
        let mut events = rt.subscribe();
        rt.cancel_all(CancelReason::RuntimeShutdown).await;
        let source = foreground("after-shutdown");
        let input = serde_json::json!({ "command": "must not run 漢🙂" });
        let expected = crate::domain::models::invocation_fingerprint::InvocationFingerprint::of(
            "Bash",
            &input,
            &source.scope_agent_id(),
        )
        .unwrap();
        let (id, decision) = rt
            .request(source, "Bash".into(), input, ToolRisk::Elevated, None, None)
            .await;
        assert!(id.is_none());
        let resolved = decision.await.unwrap();
        assert_eq!(resolved.outcome, ApprovalOutcome::Cancel);
        assert_eq!(resolved.fp, expected);
        assert!(
            rt.pending_tool_requests(ReviewScope::AllTools)
                .await
                .is_empty()
        );
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn shutdown_closes_an_admission_waiting_for_a_session_grant() {
        let rt = make_runtime();
        let mut events = rt.subscribe();
        let mut session = rt.session.write().await;
        session.always_tools.insert("Bash".into());
        let request = rt.request(
            foreground("session-shutdown-race"),
            "Bash".into(),
            serde_json::json!({ "command": "must not run" }),
            ToolRisk::Elevated,
            None,
            None,
        );
        tokio::pin!(request);
        assert!(futures::poll!(request.as_mut()).is_pending());
        rt.cancel_all(CancelReason::RuntimeShutdown).await;
        drop(session);
        let (id, decision) = request.await;
        assert!(id.is_none());
        assert_eq!(decision.await.unwrap().outcome, ApprovalOutcome::Cancel);
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn shutdown_closes_an_admission_already_waiting_for_the_pending_lock() {
        let rt = make_runtime();
        let mut events = rt.subscribe();
        let pending_guard = rt.pending.write().await;
        let request = rt.request(
            foreground("shutdown-race"),
            "Bash".into(),
            serde_json::json!({ "command": "must not run" }),
            ToolRisk::Elevated,
            None,
            None,
        );
        tokio::pin!(request);
        assert!(futures::poll!(request.as_mut()).is_pending());
        let shutdown = rt.cancel_all(CancelReason::RuntimeShutdown);
        tokio::pin!(shutdown);
        assert!(futures::poll!(shutdown.as_mut()).is_pending());
        drop(pending_guard);
        let (id, decision) = request.await;
        shutdown.await;
        assert!(id.is_none());
        assert_eq!(decision.await.unwrap().outcome, ApprovalOutcome::Cancel);
        assert!(
            rt.pending_tool_requests(ReviewScope::AllTools)
                .await
                .is_empty()
        );
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }
}
