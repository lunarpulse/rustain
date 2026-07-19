//! A2A delegation runtime: materialize a `RemotePeer` node, drive its task
//! lifecycle to terminal, project each hop onto `NodeState`, and journal the
//! room events — durable-first.
//!
//! Ruling 5 (Story 17.4b): no new port, no `NodeHandle::Remote`. The node is
//! `NodeHandle::Local` (so `LocalMessageBus` admission and `cascade_kill` work
//! unmodified) with a driver that owns the HTTP client — exactly the RAP
//! peer-delivery precedent. `Op::Kill` and the `CancellationToken` reach the
//! driver via the node's cancel token; the poll loop turns that into a real
//! `tasks/cancel` (Task 4).
//!
//! R-E: this file uses `NodeTree::try_set_state` exclusively — never the silent
//! `set_state` shim (enforced by `a2a_adapter_never_calls_the_silent_set_state_shim`).
//!
//! R-D admission/content split: `TrustTier` is threaded in as an *admission*
//! signal only and is stamped into observability; it NEVER decides what happens
//! to returned content. `delegate` returns byte-identical content for both tiers
//! given the same transcript (proven by `both_trust_tiers_return_identical_content`).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::domain::events::AppEvent;
use crate::domain::models::{
    A2aPeerSpec, AgentId, AgentMetrics, CapabilityTokenId, ContentHash, CorrelationId, MessageKind,
    NodeState, Op, PeerId, RapTaskState, RefuseReason, RejectReason, RoomEvent, SubagentEnvelope,
    SubagentEvent, TrustTier,
};
use crate::infrastructure::subagent::node_journal::NodeJournal;
use crate::infrastructure::subagent::{AgentHandle, MailboxBudget, NodeTree};

use super::client::A2aClientAdapter;
use super::error::A2aError;
use super::jsonrpc::JsonRpcRequest;
use super::lifecycle::{
    A2aTaskTransport, LifecycleOutcome, PollConfig, TaskSnapshot, poll_from_snapshot,
};

/// A live JSON-RPC transport bound to one resolved endpoint. Generates a
/// monotonic correlation id per call.
pub struct TaskClient {
    client: Arc<A2aClientAdapter>,
    endpoint: String,
    next_id: AtomicU64,
}

impl TaskClient {
    pub fn new(client: Arc<A2aClientAdapter>, endpoint: String) -> Self {
        Self {
            client,
            endpoint,
            next_id: AtomicU64::new(1),
        }
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }
}

#[async_trait]
impl A2aTaskTransport for TaskClient {
    async fn message_send(
        &self,
        message: serde_json::Value,
    ) -> Result<serde_json::Value, A2aError> {
        let request = JsonRpcRequest::new(self.next_id(), "message/send", message);
        self.client.post_jsonrpc(&self.endpoint, &request).await
    }

    async fn tasks_get(&self, task_id: &str) -> Result<serde_json::Value, A2aError> {
        let request = JsonRpcRequest::new(
            self.next_id(),
            "tasks/get",
            serde_json::json!({ "id": task_id }),
        );
        self.client.post_jsonrpc(&self.endpoint, &request).await
    }

    async fn tasks_cancel(&self, task_id: &str) -> Result<serde_json::Value, A2aError> {
        let request = JsonRpcRequest::new(
            self.next_id(),
            "tasks/cancel",
            serde_json::json!({ "id": task_id }),
        );
        self.client.post_jsonrpc(&self.endpoint, &request).await
    }
}

/// Typed failure surface of a delegation attempt.
#[derive(Debug)]
#[non_exhaustive]
pub enum DelegationError {
    /// The card resolved no usable JSON-RPC endpoint.
    Endpoint(A2aError),
    /// The peer transport failed or spoke an unrecognized dialect.
    Transport(A2aError),
    /// The peer node could not be materialized in the tree.
    Register(String),
    /// A node lifecycle transition failed or could not be persisted.
    State(String),
    /// The owned A2A driver task failed before producing an outcome.
    Driver(String),
    /// The peer/task terminated as failed/rejected.
    Refused { reason: String },
    /// The peer asked for input/auth — multi-turn is not supported (R-C). The
    /// peer's task was already cancelled; `task_id`/`context_id` are journaled.
    InputRequired {
        task_id: String,
        context_id: Option<String>,
    },
    /// The delegation was cancelled by the owner.
    Cancelled,
}

impl std::fmt::Display for DelegationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Endpoint(error) => write!(f, "no reachable A2A endpoint: {error}"),
            Self::Transport(error) => write!(f, "A2A transport failure: {error}"),
            Self::Register(reason) => write!(f, "could not materialize peer node: {reason}"),
            Self::State(reason) => write!(f, "A2A node state failure: {reason}"),
            Self::Driver(reason) => write!(f, "A2A driver task failure: {reason}"),
            Self::Refused { reason } => write!(f, "remote task refused/failed: {reason}"),
            Self::InputRequired { task_id, .. } => write!(
                f,
                "remote agent requested input; multi-turn delegation is not supported \
                 (task {task_id} cancelled)"
            ),
            Self::Cancelled => write!(f, "delegation cancelled by owner"),
        }
    }
}

/// Shared A2A delegation runtime. Injected by the composition root with the
/// live node tree, the durable journal, and the domain event sink.
#[derive(Clone)]
pub struct A2aDelegationRuntime {
    node_tree: NodeTree,
    journal: Option<Arc<NodeJournal>>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
}

impl A2aDelegationRuntime {
    pub fn new(
        node_tree: NodeTree,
        journal: Option<Arc<NodeJournal>>,
        event_tx: mpsc::UnboundedSender<AppEvent>,
    ) -> Self {
        Self {
            node_tree,
            journal,
            event_tx,
        }
    }

    /// Delegate one task to a discovered peer and drive it to terminal.
    ///
    /// `trust` is admission-only (R-D): it is stamped into observability but
    /// never decides content handling. On success the returned value is the raw
    /// remote task result; the peer node is left in a terminal `NodeState` for
    /// the panel.
    pub async fn delegate(
        &self,
        spec: &A2aPeerSpec,
        trust: TrustTier,
        parent_tool_call_id: &str,
        transport: Arc<dyn A2aTaskTransport>,
        message: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<serde_json::Value, DelegationError> {
        let runtime = self.clone();
        let spec = spec.clone();
        let parent_tool_call_id = parent_tool_call_id.to_owned();
        tokio::spawn(async move {
            runtime
                .delegate_inner(spec, trust, parent_tool_call_id, transport, message, cancel)
                .await
        })
        .await
        .map_err(|error| DelegationError::Driver(error.to_string()))?
    }

    async fn delegate_inner(
        &self,
        spec: A2aPeerSpec,
        trust: TrustTier,
        parent_tool_call_id: String,
        transport: Arc<dyn A2aTaskTransport>,
        message: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<serde_json::Value, DelegationError> {
        let peer = peer_id_for(&spec);
        // Admission signal only — stamped into the dispatch log, never used to
        // branch on content (R-D).
        tracing::info!(peer = %spec.id, ?trust, "dispatching A2A delegation");

        // The owned driver task sends first so cancellation of the calling turn
        // cannot drop an in-flight response before its peer-assigned task id is
        // available for remote cleanup and durable node materialization.
        let first = TaskSnapshot::from_result(
            transport
                .message_send(message)
                .await
                .map_err(DelegationError::Transport)?,
        )
        .map_err(DelegationError::Transport)?;
        let node_id = mint_node_id(&spec.id, &first.id);
        tracing::info!(node = %node_id, ?trust, "A2A task dispatched");

        let (node_cancel, command_rx) = self.materialize(&node_id).await?;
        self.node_tree
            .try_set_state(&node_id, NodeState::Running)
            .await
            .map_err(|error| DelegationError::State(error.to_string()))?;

        self.run_live_driver(
            &node_id,
            &peer,
            &parent_tool_call_id,
            transport.as_ref(),
            first,
            cancel,
            node_cancel,
            command_rx,
        )
        .await
    }

    /// Re-drive a peer node that a restart recovered as `Suspended` (Task 8 /
    /// AC5). The node id carries the peer + remote task id, so reconciliation
    /// re-issues `tasks/get` against the peer — NOT memory — and drives
    /// `Suspended -> Running -> terminal`. An unreachable peer leaves the node
    /// `Suspended`, never speculatively `Running` (NFR70(c)).
    pub async fn reconcile_suspended(
        &self,
        spec: &A2aPeerSpec,
        node_id: &AgentId,
        transport: &dyn A2aTaskTransport,
        cancel: CancellationToken,
    ) -> Result<(), DelegationError> {
        let Some((_peer_str, task_id)) = parse_a2a_node_id(node_id) else {
            return Ok(());
        };
        let peer = peer_id_for(spec);
        let value = match transport.tasks_get(&task_id).await {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(
                    node = %node_id,
                    %error,
                    "A2A peer unreachable on restart; leaving node Suspended"
                );
                return Ok(());
            }
        };
        let snapshot = TaskSnapshot::from_result(value).map_err(DelegationError::Transport)?;
        // Suspended -> Running (legal), then drive to terminal via the shared loop.
        self.node_tree
            .try_set_state(node_id, NodeState::Running)
            .await
            .map_err(|error| DelegationError::State(error.to_string()))?;
        self.run_lifecycle(
            node_id,
            &peer,
            "restart-reconcile",
            transport,
            snapshot,
            cancel,
        )
        .await
        .map(|_| ())
    }

    /// Link the owner token and the registered node's local control plane to
    /// one lifecycle token while keeping the `Op::Kill` receiver alive.
    async fn run_live_driver(
        &self,
        node_id: &AgentId,
        peer: &PeerId,
        parent_tool_call_id: &str,
        transport: &dyn A2aTaskTransport,
        first: TaskSnapshot,
        owner_cancel: CancellationToken,
        node_cancel: CancellationToken,
        mut command_rx: mpsc::Receiver<Op>,
    ) -> Result<serde_json::Value, DelegationError> {
        let lifecycle_cancel = CancellationToken::new();

        let token_signal = lifecycle_cancel.clone();
        let token_relay = tokio::spawn(async move {
            tokio::select! {
                () = owner_cancel.cancelled() => {}
                () = node_cancel.cancelled() => {}
            }
            token_signal.cancel();
        });

        let command_signal = lifecycle_cancel.clone();
        let command_relay = tokio::spawn(async move {
            while let Some(command) = command_rx.recv().await {
                if matches!(command, Op::Kill) {
                    command_signal.cancel();
                    break;
                }
            }
        });

        let result = self
            .run_lifecycle(
                node_id,
                peer,
                parent_tool_call_id,
                transport,
                first,
                lifecycle_cancel,
            )
            .await;
        token_relay.abort();
        command_relay.abort();
        result
    }

    /// Shared drive-to-terminal loop: live `NodeState` projection + poll +
    /// outcome handling. Used by both fresh delegation and restart reconciliation.
    async fn run_lifecycle(
        &self,
        node_id: &AgentId,
        peer: &PeerId,
        parent_tool_call_id: &str,
        transport: &dyn A2aTaskTransport,
        first: TaskSnapshot,
        cancel: CancellationToken,
    ) -> Result<serde_json::Value, DelegationError> {
        // Live NodeState projection: the poll loop's sync callback forwards each
        // legal RAP hop; a concurrent task applies it via `try_set_state`.
        let (proj_tx, mut proj_rx) = mpsc::unbounded_channel::<RapTaskState>();
        let proj_tree = self.node_tree.clone();
        let proj_node = node_id.clone();
        let projector = tokio::spawn(async move {
            while let Some(rap) = proj_rx.recv().await {
                if let Some(state) = project_rap_to_node_state(rap) {
                    proj_tree
                        .try_set_state(&proj_node, state)
                        .await
                        .map_err(|error| error.to_string())?;
                }
            }
            Ok::<(), String>(())
        });

        let config = PollConfig::default();
        let outcome = poll_from_snapshot(transport, first, &config, &cancel, |rap| {
            let _ = proj_tx.send(rap);
        })
        .await;
        drop(proj_tx);
        projector
            .await
            .map_err(|error| DelegationError::Driver(error.to_string()))?
            .map_err(DelegationError::State)?;

        match outcome {
            Ok(LifecycleOutcome::Terminal { state, task }) => match state {
                RapTaskState::Completed => {
                    // Remote content enters local context: taint it (never trust a
                    // peer's answer) and record the accepted envelope durably.
                    self.node_tree.mark_tainted(node_id).await;
                    self.emit_room(RoomEvent::RemoteEnvelopeAccepted {
                        peer: peer.clone(),
                        node: node_id.clone(),
                        content_hash: content_hash(&task.result),
                    })
                    .await;
                    Ok(task.result)
                }
                RapTaskState::Canceled => Err(DelegationError::Cancelled),
                _ => {
                    let reason = format!("peer reported terminal state {}", state.as_str());
                    self.reject(node_id, peer, parent_tool_call_id, &task.id, &reason)
                        .await?;
                    Err(DelegationError::Refused { reason })
                }
            },
            Ok(LifecycleOutcome::InputRequired { task }) => {
                // R-C: the poll loop already sent tasks/cancel to the peer. Close
                // locally as a named `Failed`, journal taskId + contextId so
                // re-delegation is possible, and surface the refusal to the user.
                self.node_tree
                    .try_set_state(node_id, NodeState::Failed)
                    .await
                    .map_err(|error| DelegationError::State(error.to_string()))?;
                let detail = format!(
                    "remote agent requested input (task {}, context {:?}); multi-turn not supported",
                    task.id, task.context_id
                );
                self.emit_room(RoomEvent::RemoteEnvelopeRejected {
                    peer: peer.clone(),
                    reason: RejectReason::Policy { detail },
                })
                .await;
                self.emit_refused(node_id, parent_tool_call_id, &task.id, RefuseReason::Policy);
                Err(DelegationError::InputRequired {
                    task_id: task.id,
                    context_id: task.context_id,
                })
            }
            Err(error) => {
                let reason = error.to_string();
                self.reject(node_id, peer, parent_tool_call_id, "unknown", &reason)
                    .await?;
                Err(DelegationError::Transport(error))
            }
        }
    }

    async fn materialize(
        &self,
        node_id: &AgentId,
    ) -> Result<(CancellationToken, mpsc::Receiver<Op>), DelegationError> {
        let (command_tx, command_rx) = mpsc::channel(1);
        let cancel_token = CancellationToken::new();
        let (status_tx, _) = watch::channel(NodeState::Created);
        let (_, metrics_rx) = watch::channel(AgentMetrics::default());
        self.node_tree
            .register_peer(
                node_id.clone(),
                AgentHandle {
                    agent_id: node_id.clone(),
                    token: CapabilityTokenId::nil(),
                    command_tx,
                    cancel_token: cancel_token.clone(),
                    depth: 0,
                    subagent_type: "a2a-peer".into(),
                    spawned_at: 0,
                    status: status_tx,
                    metrics: metrics_rx,
                    isolated: false,
                    mailbox_budget: MailboxBudget::new(),
                },
            )
            .await
            .map_err(|error| DelegationError::Register(error.to_string()))?;
        Ok((cancel_token, command_rx))
    }

    async fn reject(
        &self,
        node_id: &AgentId,
        peer: &PeerId,
        parent_tool_call_id: &str,
        task_id: &str,
        reason: &str,
    ) -> Result<(), DelegationError> {
        self.node_tree
            .try_set_state(node_id, NodeState::Failed)
            .await
            .map_err(|error| DelegationError::State(error.to_string()))?;
        self.emit_room(RoomEvent::RemoteEnvelopeRejected {
            peer: peer.clone(),
            reason: RejectReason::Policy {
                detail: reason.to_owned(),
            },
        })
        .await;
        self.emit_refused(node_id, parent_tool_call_id, task_id, RefuseReason::Policy);
        Ok(())
    }

    /// Durable-first room emission: append to the journal, then publish on the
    /// bus. Never the reverse (Ruling 6).
    async fn emit_room(&self, event: RoomEvent) {
        // Build the bus event before sending (the peer-delivery precedent) so the
        // send site routes a variable, not an inline `AppEvent::` — consistent
        // with `rap/peer_delivery.rs`'s `domain_tx.send(receipt)` bridge.
        let bus_event = AppEvent::DomainEvent(event.clone().into());
        if let Some(journal) = &self.journal {
            match journal.append_room(event).await {
                Ok(_) => {
                    let _ = self.event_tx.send(bus_event);
                }
                Err(error) => {
                    tracing::error!(%error, "failed to journal A2A room event; dropping bus emit");
                }
            }
        } else {
            let _ = self.event_tx.send(bus_event);
        }
    }

    fn emit_refused(
        &self,
        node_id: &AgentId,
        parent_tool_call_id: &str,
        correlation: &str,
        reason: RefuseReason,
    ) {
        let envelope = SubagentEnvelope::new(
            parent_tool_call_id.to_owned(),
            node_id.clone(),
            MessageKind::PeerMessage,
            SubagentEvent::MessageRefused {
                correlation_id: CorrelationId::new(correlation),
                reason,
            },
        );
        let receipt = AppEvent::Subagent(envelope);
        let _ = self.event_tx.send(receipt);
    }
}

/// Mint a peer node id that durably and reversibly encodes `(peer, remote task
/// id)` as three safe `AgentId` path segments. Both values cross configuration
/// or network trust boundaries, so neither may be interpolated into an
/// `AgentId` before encoding.
fn mint_node_id(peer: &str, task_id: &str) -> AgentId {
    let peer = URL_SAFE_NO_PAD.encode(peer.as_bytes());
    let task = URL_SAFE_NO_PAD.encode(task_id.as_bytes());
    AgentId::from_validated(format!("a2a/p-{peer}/t-{task}"))
}

/// Recover `(peer, task_id)` from a node id minted by [`mint_node_id`]. Returns
/// `None` for malformed, empty, or non-A2A node ids.
fn parse_a2a_node_id(node_id: &AgentId) -> Option<(String, String)> {
    let rest = node_id.as_str().strip_prefix("a2a/")?;
    let (peer, task) = rest.split_once('/')?;
    if task.contains('/') {
        return None;
    }
    let peer = peer.strip_prefix("p-")?;
    let task = task.strip_prefix("t-")?;
    let peer = String::from_utf8(URL_SAFE_NO_PAD.decode(peer).ok()?).ok()?;
    let task = String::from_utf8(URL_SAFE_NO_PAD.decode(task).ok()?).ok()?;
    if peer.is_empty() || task.is_empty() {
        return None;
    }
    Some((peer, task))
}

/// Project a RAP task state onto the coarse node lifecycle. `input-required` /
/// `auth-required` are handled by the delegation outcome (R-C), not projected as
/// a live hop.
fn project_rap_to_node_state(state: RapTaskState) -> Option<NodeState> {
    match state {
        RapTaskState::Submitted | RapTaskState::Working => Some(NodeState::Running),
        RapTaskState::Completed => Some(NodeState::Completed),
        RapTaskState::Failed | RapTaskState::Rejected => Some(NodeState::Failed),
        RapTaskState::Canceled => Some(NodeState::Cancelled),
        RapTaskState::InputRequired | RapTaskState::AuthRequired => None,
    }
}

/// Derive an observability `PeerId`. A `Verified` peer's pinned Ed25519 key is
/// its real identity; an `Unverified` peer has no crypto identity, so a stable
/// (non-cryptographic) id is derived from the operator-configured peer id purely
/// so room events have a consistent key.
fn peer_id_for(spec: &A2aPeerSpec) -> PeerId {
    if let Some(pinned) = spec.pinned_key.as_ref()
        && let Ok(bytes) = URL_SAFE_NO_PAD.decode(pinned.x.as_bytes())
        && bytes.len() == 32
        && let Ok(peer_id) = PeerId::from_public_key(&bytes)
    {
        return peer_id;
    }
    let digest = Sha256::digest(spec.id.as_bytes());
    PeerId::from_public_key(&digest).expect("sha256 digest is exactly 32 bytes")
}

fn content_hash(value: &serde_json::Value) -> ContentHash {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    ContentHash::from_bytes(Sha256::digest(&bytes).into())
}

/// Build an A2A `message/send` params object from the tool input. The JSON-RPC
/// binding uses `kind`-tagged parts.
pub fn build_message(input: &serde_json::Value) -> serde_json::Value {
    let text = input
        .get("message")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| input.to_string());
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    serde_json::json!({
        "message": {
            "kind": "message",
            "messageId": format!("rustain-{nanos}"),
            "role": "user",
            "parts": [{ "kind": "text", "text": text }]
        }
    })
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::time::Duration;

    use parking_lot::Mutex;

    use super::*;
    use crate::domain::models::{JournalRecord, RedactedUrl};

    fn spec(id: &str, verified: bool) -> A2aPeerSpec {
        use crate::domain::models::{A2aPeerSource, PinnedKey, PinnedKeyAlgorithm};
        A2aPeerSpec {
            id: id.to_owned(),
            url: RedactedUrl::new("https://peer.example/a2a".to_owned()),
            pinned_key: verified.then(|| {
                PinnedKey::new(
                    PinnedKeyAlgorithm::EdDsa,
                    URL_SAFE_NO_PAD.encode([7u8; 32]),
                    None,
                )
            }),
            source: A2aPeerSource::Workspace,
        }
    }

    struct Scripted {
        script: Mutex<VecDeque<serde_json::Value>>,
        cancels: Mutex<Vec<String>>,
    }

    impl Scripted {
        fn new(states: &[&str]) -> Self {
            let script = states
                .iter()
                .map(|state| {
                    serde_json::json!({
                        "kind": "task",
                        "id": "task-1",
                        "contextId": "ctx-1",
                        "status": {"state": state},
                        "artifacts": [{"parts": [{"kind": "text", "text": "result"}]}]
                    })
                })
                .collect();
            Self {
                script: Mutex::new(script),
                cancels: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl A2aTaskTransport for Scripted {
        async fn message_send(
            &self,
            _message: serde_json::Value,
        ) -> Result<serde_json::Value, A2aError> {
            Ok(self.script.lock().pop_front().expect("script exhausted"))
        }
        async fn tasks_get(&self, _task_id: &str) -> Result<serde_json::Value, A2aError> {
            Ok(self.script.lock().pop_front().expect("script exhausted"))
        }
        async fn tasks_cancel(&self, task_id: &str) -> Result<serde_json::Value, A2aError> {
            self.cancels.lock().push(task_id.to_owned());
            Ok(serde_json::json!({"kind":"task","id":task_id,"status":{"state":"canceled"}}))
        }
    }

    fn runtime() -> (
        A2aDelegationRuntime,
        NodeTree,
        mpsc::UnboundedReceiver<AppEvent>,
    ) {
        let tree = NodeTree::new();
        let (tx, rx) = mpsc::unbounded_channel();
        (A2aDelegationRuntime::new(tree.clone(), None, tx), tree, rx)
    }
    async fn wait_for_peer_node(tree: &NodeTree) -> AgentId {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(node) = tree
                    .list()
                    .await
                    .into_iter()
                    .find(|entry| entry.subagent_type == "a2a-peer")
                {
                    break node.agent_id;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("peer node materialized")
    }

    #[tokio::test]
    async fn completed_delegation_taints_node_and_emits_accepted() {
        let (rt, tree, mut rx) = runtime();
        let transport = Arc::new(Scripted::new(&["submitted", "working", "completed"]));
        let result = rt
            .delegate(
                &spec("planets", false),
                TrustTier::Unverified,
                "call-1",
                transport.clone(),
                serde_json::json!({}),
                CancellationToken::new(),
            )
            .await
            .expect("completed delegation returns content");
        assert_eq!(result["status"]["state"], "completed");

        let entry = tree
            .list()
            .await
            .into_iter()
            .find(|e| e.subagent_type == "a2a-peer")
            .expect("peer node materialized");
        assert_eq!(entry.current_status, NodeState::Completed);
        assert_eq!(entry.ownership, crate::domain::models::OwnershipKind::Peer);

        // A RemoteEnvelopeAccepted DomainEvent was emitted on the bus.
        let mut saw_accepted = false;
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::DomainEvent(payload) = event
                && format!("{payload:?}").contains("RemoteEnvelopeAccepted")
            {
                saw_accepted = true;
            }
        }
        assert!(saw_accepted, "RemoteEnvelopeAccepted must be emitted");
    }
    #[tokio::test]
    async fn dropped_caller_still_cancels_remote_task_and_terminalizes_node() {
        let (rt, tree, _rx) = runtime();
        let transport = Arc::new(Scripted::new(&["working", "working"]));
        let cancel = CancellationToken::new();
        let runner = tokio::spawn({
            let rt = rt.clone();
            let transport = transport.clone();
            let cancel = cancel.clone();
            async move {
                rt.delegate(
                    &spec("planets", false),
                    TrustTier::Unverified,
                    "call-cancel",
                    transport,
                    serde_json::json!({}),
                    cancel,
                )
                .await
            }
        });

        let node_id = wait_for_peer_node(&tree).await;
        cancel.cancel();
        runner.abort();

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if transport.cancels.lock().as_slice() == ["task-1"]
                    && status_of(&tree.list().await, &node_id) == NodeState::Cancelled
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached driver must finish remote and local cancellation");
    }

    #[tokio::test]
    async fn cascade_kill_reaches_driver_and_cancels_remote_task() {
        let (rt, tree, _rx) = runtime();
        let transport = Arc::new(Scripted::new(&["working", "working"]));
        let delegate = tokio::spawn({
            let transport = transport.clone();
            async move {
                rt.delegate(
                    &spec("planets", false),
                    TrustTier::Unverified,
                    "call-kill",
                    transport,
                    serde_json::json!({}),
                    CancellationToken::new(),
                )
                .await
            }
        });

        let node_id = wait_for_peer_node(&tree).await;
        let killed = tree
            .cascade_kill(&node_id, Duration::from_secs(1))
            .await
            .expect("cooperative A2A kill succeeds");
        assert!(killed.contains(&node_id));
        assert_eq!(transport.cancels.lock().as_slice(), ["task-1"]);
        let result = delegate.await.expect("delegate task joins");
        assert!(matches!(result, Err(DelegationError::Cancelled)));
    }

    #[tokio::test]
    async fn both_trust_tiers_return_identical_content() {
        // R-D keystone: TrustTier gates admission, never content. The same
        // transcript must yield byte-identical content for Verified and Unverified.
        // Distinct runtimes so the deterministic (peer, task-id) node id does not
        // collide across the two runs.
        let (rt_v, _tv, _rxv) = runtime();
        let verified = rt_v
            .delegate(
                &spec("planets", true),
                TrustTier::Verified,
                "call-v",
                Arc::new(Scripted::new(&["working", "completed"])),
                serde_json::json!({}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let (rt_u, _tu, _rxu) = runtime();
        let unverified = rt_u
            .delegate(
                &spec("planets", false),
                TrustTier::Unverified,
                "call-u",
                Arc::new(Scripted::new(&["working", "completed"])),
                serde_json::json!({}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(verified, unverified);
    }

    #[tokio::test]
    async fn input_required_cancels_peer_journals_and_refuses() {
        let tree = NodeTree::new();
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(NodeJournal::open_workspace(dir.path()).await.unwrap());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let rt = A2aDelegationRuntime::new(tree.clone(), Some(journal.clone()), tx);
        let transport = Arc::new(Scripted::new(&["working", "input-required"]));

        let error = rt
            .delegate(
                &spec("planets", false),
                TrustTier::Unverified,
                "call-ir",
                transport.clone(),
                serde_json::json!({}),
                CancellationToken::new(),
            )
            .await
            .expect_err("input-required is a refusal, not a success");
        assert!(matches!(error, DelegationError::InputRequired { .. }));
        assert_eq!(
            transport.cancels.lock().as_slice(),
            ["task-1"],
            "R-C: the peer's task must be cancelled"
        );

        let node = tree
            .list()
            .await
            .into_iter()
            .find(|e| e.subagent_type == "a2a-peer")
            .unwrap();
        assert_eq!(node.current_status, NodeState::Failed);

        // taskId/contextId journaled as a durable rejection.
        let records = journal.load().await.unwrap();
        let saw_rejected = records.iter().any(|entry| {
            matches!(
                &entry.record,
                JournalRecord::Room(RoomEvent::RemoteEnvelopeRejected { .. })
            )
        });
        assert!(saw_rejected, "RemoteEnvelopeRejected must be journaled");

        // A MessageRefused receipt reached the bus.
        let mut saw_refused = false;
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::Subagent(env) = event
                && matches!(env.event, SubagentEvent::MessageRefused { .. })
            {
                saw_refused = true;
            }
        }
        assert!(saw_refused, "MessageRefused receipt must be emitted");
    }

    #[tokio::test]
    async fn rejected_state_projects_failed_and_refuses() {
        let (rt, tree, _rx) = runtime();
        let error = rt
            .delegate(
                &spec("planets", false),
                TrustTier::Unverified,
                "call-r",
                Arc::new(Scripted::new(&["working", "rejected"])),
                serde_json::json!({}),
                CancellationToken::new(),
            )
            .await
            .expect_err("rejected is a refusal");
        assert!(matches!(error, DelegationError::Refused { .. }));
        let node = tree
            .list()
            .await
            .into_iter()
            .find(|e| e.subagent_type == "a2a-peer")
            .unwrap();
        assert_eq!(node.current_status, NodeState::Failed);
    }
    #[tokio::test]
    async fn illegal_projected_state_transition_is_returned() {
        let (rt, _tree, _rx) = runtime();
        let node_id = mint_node_id("planets", "task-1");
        let _control = rt.materialize(&node_id).await.unwrap();
        let peer = peer_id_for(&spec("planets", false));
        let transport = Scripted::new(&["completed"]);
        let first =
            TaskSnapshot::from_result(transport.message_send(serde_json::json!({})).await.unwrap())
                .unwrap();

        let error = rt
            .run_lifecycle(
                &node_id,
                &peer,
                "call-illegal",
                &transport,
                first,
                CancellationToken::new(),
            )
            .await
            .expect_err("Created -> Completed must fail loudly");
        assert!(matches!(error, DelegationError::State(_)));
    }

    /// A transport whose `tasks_get` always fails — a peer unreachable after a
    /// restart. `reconcile_suspended` must leave the node `Suspended`.
    struct UnreachableTransport;

    #[async_trait]
    impl A2aTaskTransport for UnreachableTransport {
        async fn message_send(
            &self,
            _message: serde_json::Value,
        ) -> Result<serde_json::Value, A2aError> {
            Err(A2aError::Request("unreachable".to_owned()))
        }
        async fn tasks_get(&self, _task_id: &str) -> Result<serde_json::Value, A2aError> {
            Err(A2aError::Request("unreachable".to_owned()))
        }
        async fn tasks_cancel(&self, _task_id: &str) -> Result<serde_json::Value, A2aError> {
            Err(A2aError::Request("unreachable".to_owned()))
        }
    }

    async fn register_suspended(tree: &NodeTree, node_id: &AgentId) {
        let (command_tx, _rx) = mpsc::channel(1);
        let (status_tx, _) = watch::channel(NodeState::Created);
        let (_, metrics_rx) = watch::channel(AgentMetrics::default());
        tree.register_peer(
            node_id.clone(),
            AgentHandle {
                agent_id: node_id.clone(),
                token: CapabilityTokenId::nil(),
                command_tx,
                cancel_token: CancellationToken::new(),
                depth: 0,
                subagent_type: "a2a-peer".into(),
                spawned_at: 0,
                status: status_tx,
                metrics: metrics_rx,
                isolated: false,
                mailbox_budget: MailboxBudget::new(),
            },
        )
        .await
        .unwrap();
        // Created -> Running -> Suspended, as NodeRecovery leaves an in-flight
        // remote node after a crash restart.
        tree.try_set_state(node_id, NodeState::Running)
            .await
            .unwrap();
        tree.try_set_state(node_id, NodeState::Suspended)
            .await
            .unwrap();
    }

    fn status_of(
        entries: &[crate::infrastructure::subagent::RegistryEntry],
        node: &AgentId,
    ) -> NodeState {
        entries
            .iter()
            .find(|entry| &entry.agent_id == node)
            .map(|entry| entry.current_status)
            .expect("node present")
    }

    #[test]
    fn node_id_round_trips_untrusted_peer_and_task_values() {
        for (peer, task) in [
            ("planets", "task-1"),
            ("org/planets", "root"),
            ("root", "segment//with/slashes"),
        ] {
            let node = mint_node_id(peer, task);
            assert_eq!(
                parse_a2a_node_id(&node),
                Some((peer.to_owned(), task.to_owned()))
            );
        }
        assert_eq!(
            parse_a2a_node_id(&AgentId::from_validated("subagent/x")),
            None
        );
    }

    #[tokio::test]
    async fn reconcile_drives_a_suspended_node_to_terminal() {
        // AC5: after a restart the node recovered as Suspended is reconciled by
        // re-issuing tasks/get and reaches terminal via Suspended -> Running ->
        // Completed (every hop legal).
        let (rt, tree, _rx) = runtime();
        let node_id = mint_node_id("planets", "task-1");
        register_suspended(&tree, &node_id).await;
        assert_eq!(
            status_of(&tree.list().await, &node_id),
            NodeState::Suspended
        );

        let transport = Scripted::new(&["working", "completed"]);
        rt.reconcile_suspended(
            &spec("planets", false),
            &node_id,
            &transport,
            CancellationToken::new(),
        )
        .await
        .expect("reconciliation succeeds");
        assert_eq!(
            status_of(&tree.list().await, &node_id),
            NodeState::Completed
        );
    }

    #[tokio::test]
    async fn reconcile_leaves_node_suspended_when_peer_is_unreachable() {
        // AC5 / NFR70(c): an unreachable peer must NOT flip the node speculatively
        // back to Running — it stays Suspended (honest and observable).
        let (rt, tree, _rx) = runtime();
        let node_id = mint_node_id("planets", "task-1");
        register_suspended(&tree, &node_id).await;
        rt.reconcile_suspended(
            &spec("planets", false),
            &node_id,
            &UnreachableTransport,
            CancellationToken::new(),
        )
        .await
        .expect("unreachable is not an error, it is a deferral");
        assert_eq!(
            status_of(&tree.list().await, &node_id),
            NodeState::Suspended,
            "an unreachable peer must never speculatively resurrect Running"
        );
    }
}
