//! Process-owned inbound turn driver, shared with the RAP collector.
use super::parked_cancel::ParkedResponseContext;
use super::*;
use crate::adapters::daemon::response_modes;
/// The owned slice of `AttachServer` an inbound peer turn needs after `start`
/// has returned.
///
/// `AttachServer` is held as `Arc<Self>` by its own callers but the port's
/// `start` takes `&self`, and the turn must outlive the call. Cloning the
/// handles it genuinely uses is honest about the coupling; a `Weak<Self>`
/// upgrade would hide it and add a failure mode nobody handles.
pub(super) struct InboundTurnContext {
    pub(super) core: Arc<DaemonCore>,
    pub(super) conversation: Arc<Mutex<Conversation>>,
    pub(super) peer_row_serial: Arc<Mutex<()>>,
    pub(super) domain_tx: mpsc::UnboundedSender<AppEvent>,
    pub(super) node_tree: crate::infrastructure::subagent::NodeTree,
    /// The daemon has ONE conversation; turns must not interleave on it.
    pub(super) turn_serial: Arc<Mutex<()>>,
    pub(super) active_channel_origin: Arc<Mutex<ChannelKind>>,
    pub(super) inbound_results:
        Arc<Mutex<std::collections::HashMap<AgentId, crate::domain::ports::StagedPeerResult>>>,
    pub(super) pending_drafts: Arc<response_modes::PendingDraftController>,
    pub(super) peer_reply_serial: Arc<Mutex<()>>,
    pub(super) registry: Arc<Mutex<ConnRegistry>>,
    pub(super) blocked_waiting: Arc<AtomicUsize>,
    pub(super) approval_gate: Arc<ApprovalGate>,
    pub(super) pending_consent_cards:
        Arc<Mutex<std::collections::HashMap<crate::domain::models::RequestId, DaemonFrame>>>,
    pub(super) peer_turn_closed: Arc<Notify>,
}

/// Tee-collector shared by the A2A front door (`InboundTurnContext::run`) and
/// the RAP delivery path. The assistant answer is committed to the shared
/// `conversation` by the attach forwarder, which only runs when a client is
/// attached — so reading the conversation afterwards would make "did the remote
/// peer get its answer?" depend on whether an operator happened to be watching.
/// Accumulating from the turn's own `ProviderChunk` stream does not.
///
/// `run_turn`'s join handle resolves `Ok` whether the turn succeeded or died,
/// because failure is reported on the event stream rather than by unwinding.
/// This is therefore the ONLY place "the turn finished" and "the turn worked"
/// are different questions — and there are three ways it can have not worked:
/// (1) an in-stream error chunk (tool loop limit, stream disconnect), (2) a
/// provider call that never produced a stream at all (`run_turn` emits an error
/// notice and returns), and (3) the catch-all: a turn that never reached a
/// final `TurnComplete` did not complete, whatever else it did or did not say.
///
/// Managed response text stays local until the operator resolves the draft.
/// Permitted tool display and terminal cleanup carry host-owned peer lineage.
pub(super) fn spawn_turn_collector(
    mut tap_rx: mpsc::UnboundedReceiver<AppEvent>,
    downstream: mpsc::UnboundedSender<AppEvent>,
    buffer_response: bool,
    node: Arc<AgentId>,
    conversation_id: String,
) -> tokio::task::JoinHandle<(String, bool, bool)> {
    tokio::spawn(async move {
        let mut answer = String::new();
        let mut errored = false;
        let mut completed = false;
        while let Some(event) = tap_rx.recv().await {
            match &event {
                AppEvent::ProviderChunk {
                    chunk: StreamChunk::Error { .. },
                    ..
                } => errored = true,
                AppEvent::SystemNotice {
                    level: crate::domain::models::NoticeLevel::Error,
                    ..
                } => errored = true,
                AppEvent::ProviderChunk {
                    chunk: StreamChunk::TurnComplete { stop_reason },
                    ..
                } if *stop_reason != StopReason::ToolUse => completed = true,
                AppEvent::ProviderChunk {
                    chunk: StreamChunk::Text { content, .. },
                    ..
                } => answer.push_str(content),
                _ => {}
            }
            match event {
                AppEvent::ProviderChunk {
                    conversation_id,
                    chunk,
                } => {
                    if !buffer_response || !matches!(chunk, StreamChunk::Text { .. }) {
                        let _ = downstream.send(AppEvent::PeerProviderChunk {
                            node: node.clone(),
                            conversation_id,
                            chunk,
                        });
                    }
                }
                AppEvent::SystemNotice {
                    conversation_id,
                    level,
                    message,
                } => {
                    let _ = downstream.send(AppEvent::PeerSystemNotice {
                        node: node.clone(),
                        conversation_id,
                        level,
                        message,
                    });
                }
                other => {
                    let _ = downstream.send(other);
                }
            }
        }
        let _ = downstream.send(AppEvent::PeerTurnClosed {
            node,
            conversation_id,
        });
        (answer, errored, completed)
    })
}

impl InboundTurnContext {
    async fn ensure_approval_gate(&self, approval: Arc<ApprovalRuntime>) {
        ensure_approval_gate_once(
            approval,
            self.registry.clone(),
            self.blocked_waiting.clone(),
            self.conversation.clone(),
            self.core.storage.clone(),
            self.domain_tx.clone(),
            self.pending_consent_cards.clone(),
            self.approval_gate.clone(),
        )
        .await;
    }

    async fn cleanup_pending_response(&self, node_id: &AgentId) {
        self.pending_drafts.abandon(node_id.as_str()).await;
        if let Err(error) = remove_pending_peer_row(
            &self.peer_row_serial,
            &self.conversation,
            &self.registry,
            self.core.storage.as_ref(),
            node_id,
        )
        .await
        {
            tracing::warn!(%error, node = %node_id, "removing failed peer response placeholder failed");
        }
    }

    /// Drive one inbound peer task to a terminal node state.
    ///
    /// This is the same origination primitive the Unix socket drives —
    /// `drive_preloaded_turn` with `TurnOrigin::RemotePeer` — not a parallel
    /// injection path. The only additions are the ones the front door owes the
    /// node: a terminal transition that distinguishes cancel from failure, and
    /// capture of the answer this task produced.
    pub(super) async fn run(
        self,
        node_id: AgentId,
        principal: PolicyPrincipal,
        cancel: CancellationToken,
        response_policy: crate::domain::ports::PeerResponsePolicy,
    ) {
        // Fail closed: only an explicit wait/auto policy takes another path;
        // draft and every future variant buffer until operator resolution.
        let managed_response = !matches!(
            response_policy.mode,
            crate::domain::models::ResponseMode::NotifyAndWait
                | crate::domain::models::ResponseMode::NotifyAndAuto
        );
        let response_row_id = format!("peer-response-{}", node_id.as_str());
        let runtime = match self.core.ensure_runtime().await {
            Ok(runtime) => runtime,
            Err(error) => {
                tracing::error!(%error, node = %node_id, "A2A inbound turn: runtime unavailable");
                if managed_response {
                    self.cleanup_pending_response(&node_id).await;
                }
                self.node_tree.set_state(&node_id, NodeState::Running).await;
                self.node_tree.set_state(&node_id, NodeState::Failed).await;
                self.node_tree.deregister(&node_id).await;
                return;
            }
        };
        self.ensure_approval_gate(runtime.approval.clone()).await;

        // Serialize on the same mutex every other daemon turn uses. Taken AFTER
        // registration so the node — and therefore `tasks/get` — is live and
        // reports `working` while the task waits its turn, rather than the
        // submitter seeing nothing until the queue drains.
        let _turn_guard = tokio::select! {
            guard = self.turn_serial.lock() => guard,
            () = cancel.cancelled() => {
                if managed_response {
                    self.cleanup_pending_response(&node_id).await;
                }
                self.node_tree.set_state(&node_id, NodeState::Running).await;
                self.node_tree
                    .set_state(&node_id, NodeState::Cancelled)
                    .await;
                self.node_tree.deregister(&node_id).await;
                return;
            }
        };
        if cancel.is_cancelled() {
            if managed_response {
                self.cleanup_pending_response(&node_id).await;
            }
            self.node_tree.set_state(&node_id, NodeState::Running).await;
            self.node_tree
                .set_state(&node_id, NodeState::Cancelled)
                .await;
            self.node_tree.deregister(&node_id).await;
            return;
        }
        *self.active_channel_origin.lock().await = ChannelKind::Terminal;

        self.node_tree.set_state(&node_id, NodeState::Running).await;
        let peer_turn_closed = self.peer_turn_closed.notified();
        tokio::pin!(peer_turn_closed);
        peer_turn_closed.as_mut().enable();

        // Tee the turn's event stream (see `spawn_turn_collector`). Every
        // non-buffered event is forwarded on to the real `domain_tx`, so the
        // daemon bus, the forwarder, and every other consumer see exactly what
        // they saw before.
        let conversation_id = self.conversation.lock().await.id.clone();
        let (tap_tx, tap_rx) = mpsc::unbounded_channel::<AppEvent>();
        let peer_node = Arc::new(node_id.clone());
        let collector = spawn_turn_collector(
            tap_rx,
            self.domain_tx.clone(),
            managed_response,
            peer_node.clone(),
            conversation_id.clone(),
        );

        let mut handle = {
            let mut conversation = self.conversation.lock().await;
            runtime.drive_preloaded_turn(
                &mut conversation,
                &tap_tx,
                TurnOrigin::RemotePeer { principal },
                cancel.clone(),
            )
        };
        drop(tap_tx);

        // `run_turn`'s cancellation token reaches tool execution but NOT a
        // provider stream that never yields, so awaiting the handle alone would
        // make `tasks/cancel` take effect only once the model replied. Racing the
        // token here — at the layer that owns the join handle — is what makes the
        // cancel actually prompt.
        let joined = tokio::select! {
            outcome = &mut handle => Some(outcome.is_err()),
            () = cancel.cancelled() => {
                handle.abort();
                let _ = handle.await;
                None
            }
        };
        let collector_result = match collector.await {
            Ok(result) => result,
            Err(error) => {
                tracing::warn!(?error, node = %node_id, "A2A inbound turn collector failed");
                (String::new(), true, false)
            }
        };
        let (answer, errored, completed) = collector_result;
        peer_turn_closed.await;
        let _reply_guard = self.peer_reply_serial.lock().await;
        let _row_guard = self.peer_row_serial.lock().await;

        let terminal = match joined {
            None => {
                if !completed {
                    // The abort drops `run_turn` before it can emit a terminal
                    // chunk. Drain its tee first, then enqueue a cancelled
                    // terminal event after every partial chunk. The forwarder
                    // treats that event as a buffer reset, never an answer.
                    // The reset must travel the daemon's own forwarder channel in stream order;
                    // InboundTurnContext holds no event_bus handle, and every other chunk of this
                    // turn tees through the same tx.
                    let reset = AppEvent::PeerProviderChunk {
                        node: peer_node.clone(),
                        conversation_id,
                        chunk: StreamChunk::TurnComplete {
                            stop_reason: StopReason::Cancelled,
                        },
                    };
                    let _ = self.domain_tx.send(reset); // CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS: 18-1b AC6b — ordered cancel reset via the daemon forwarder channel
                }
                NodeState::Cancelled
            }
            Some(join_failed) => {
                let terminal = if join_failed || errored || !completed {
                    // An unexpected join/runtime failure can leave streamed text
                    // without a terminal chunk too. Reset it through the same
                    // ordered path rather than allowing the next turn to own it.
                    if !completed {
                        // Same ordered-reset channel as the cancel path above; the forwarder
                        // consumes domain_tx.
                        let reset = AppEvent::PeerProviderChunk {
                            node: peer_node.clone(),
                            conversation_id,
                            chunk: StreamChunk::TurnComplete {
                                stop_reason: StopReason::Cancelled,
                            },
                        };
                        let _ = self.domain_tx.send(reset); // CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS: 18-1b AC6b — ordered cancel reset via the daemon forwarder channel
                    }
                    NodeState::Failed
                } else if managed_response && !answer.trim().is_empty() {
                    if !self
                        .pending_drafts
                        .complete(node_id.as_str(), answer.clone())
                        .await
                    {
                        let settled = {
                            let conversation = self.conversation.lock().await;
                            conversation
                                .peer_row_controls
                                .get(&response_row_id)
                                .is_some_and(|control| control.stage == PeerRowStage::Settled)
                        };
                        if settled {
                            return;
                        }
                        NodeState::Failed
                    } else {
                        let (row, control) = {
                            let conversation = self.conversation.lock().await;
                            (
                                conversation
                                    .messages
                                    .iter()
                                    .find(|message| message.id == response_row_id)
                                    .cloned(),
                                conversation
                                    .peer_row_controls
                                    .get(&response_row_id)
                                    .cloned(),
                            )
                        };
                        match (row, control) {
                            (Some(mut row), Some(mut control))
                                if control.stage == PeerRowStage::Drafting =>
                            {
                                row.content =
                                    format!("{}{}", response_modes::DRAFT_APPROVAL_PREFIX, answer);
                                control.stage = PeerRowStage::Ready;
                                control.node_resolvable = true;
                                match commit_peer_rows_serialized(
                                    &self.conversation,
                                    &self.registry,
                                    self.core.storage.as_ref(),
                                    vec![PeerRowMutation::Upsert {
                                        message: row,
                                        control: Some(control),
                                    }],
                                )
                                .await
                                {
                                    Ok(()) => NodeState::Waiting,
                                    Err(error) => {
                                        tracing::warn!(%error, "persisting completed peer draft failed");
                                        self.pending_drafts.abandon(node_id.as_str()).await;
                                        NodeState::Failed
                                    }
                                }
                            }
                            (_, Some(control)) if control.stage == PeerRowStage::Settled => return,
                            _ => {
                                self.pending_drafts.abandon(node_id.as_str()).await;
                                NodeState::Failed
                            }
                        }
                    }
                } else if managed_response {
                    NodeState::Failed
                } else {
                    if !answer.trim().is_empty() {
                        self.inbound_results.lock().await.insert(
                            node_id.clone(),
                            crate::domain::ports::StagedPeerResult {
                                text: answer,
                                authorship: crate::domain::models::Authorship::AgentComposed,
                                send_authorization:
                                    crate::domain::models::SendAuthorization::Automatic,
                            },
                        );
                    }
                    NodeState::Completed
                };

                terminal
            }
        };
        if managed_response && !matches!(terminal, NodeState::Completed | NodeState::Waiting) {
            self.pending_drafts.abandon(node_id.as_str()).await;
            if let Err(error) = remove_pending_peer_row_serialized(
                &self.conversation,
                &self.registry,
                self.core.storage.as_ref(),
                &node_id,
            )
            .await
            {
                tracing::warn!(%error, "removing failed peer response placeholder failed");
            }
        }
        self.node_tree.set_state(&node_id, terminal).await;
        if terminal == NodeState::Waiting {
            if let Err(error) = self
                .node_tree
                .stamp_wait_reason(
                    &node_id,
                    Some(crate::domain::models::WaitReason::AwaitingPeerResponse),
                )
                .await
            {
                tracing::error!(%error, node = %node_id, "stamping peer draft wait failed");
                self.node_tree.set_state(&node_id, NodeState::Failed).await;
                self.pending_drafts.abandon(node_id.as_str()).await;
                if let Err(remove_error) = remove_pending_peer_row_serialized(
                    &self.conversation,
                    &self.registry,
                    self.core.storage.as_ref(),
                    &node_id,
                )
                .await
                {
                    tracing::warn!(%remove_error, node = %node_id, "removing failed peer response placeholder failed");
                }
                self.node_tree.deregister(&node_id).await;
                return;
            }
            ParkedResponseContext {
                conversation: self.conversation.clone(),
                registry: self.registry.clone(),
                storage: self.core.storage.clone(),
                node_tree: self.node_tree.clone(),
                pending_drafts: self.pending_drafts.clone(),
                peer_reply_serial: self.peer_reply_serial.clone(),
                peer_row_serial: self.peer_row_serial.clone(),
            }
            .spawn(node_id.clone(), cancel.clone());
            return;
        }
        self.node_tree.deregister(&node_id).await;
    }
}
