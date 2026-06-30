use std::{collections::VecDeque, sync::Arc};

use async_trait::async_trait;
use futures::FutureExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::domain::models::{
    AgentId, AgentLaunchSpec, CapabilityFlag, CapabilityToken, NodeState, Op, OwnershipKind,
    SubagentError, TaskHandle,
};
use crate::domain::ports::{AuthorityProvider, SubagentRunner};
use crate::domain::services::sandbox_narrowing::validate_narrowing;
use crate::infrastructure::subagent::{NodeTree, SpoolMeta, SubagentSpool};

pub struct InProcessSubagentRunner {
    provider: Arc<dyn crate::domain::ports::StreamingProvider>,
    storage: Arc<dyn crate::domain::ports::StoragePort>,
    security: Arc<dyn crate::domain::ports::SecurityPort>,
    tools: Arc<dyn crate::domain::ports::ToolSetPort>,
    approval: Arc<crate::domain::services::approval_runtime::ApprovalRuntime>,
    scheduler: Arc<crate::domain::services::tool_scheduler::ToolScheduler>,
    event_bus: Arc<crate::infrastructure::runtime::event_bus::EventBus>,
    registry: Arc<NodeTree>,
    parent_sandbox: Arc<tokio::sync::RwLock<crate::domain::models::SandboxPolicy>>,
    spool: Arc<SubagentSpool>,
    authority: Arc<dyn AuthorityProvider>,
    root_authority: CapabilityToken,
}

impl InProcessSubagentRunner {
    pub fn new(
        provider: Arc<dyn crate::domain::ports::StreamingProvider>,
        storage: Arc<dyn crate::domain::ports::StoragePort>,
        security: Arc<dyn crate::domain::ports::SecurityPort>,
        tools: Arc<dyn crate::domain::ports::ToolSetPort>,
        approval: Arc<crate::domain::services::approval_runtime::ApprovalRuntime>,
        scheduler: Arc<crate::domain::services::tool_scheduler::ToolScheduler>,
        event_bus: Arc<crate::infrastructure::runtime::event_bus::EventBus>,
        registry: Arc<NodeTree>,
        parent_sandbox: Arc<tokio::sync::RwLock<crate::domain::models::SandboxPolicy>>,
        spool: Arc<SubagentSpool>,
        authority: Arc<dyn AuthorityProvider>,
        root_authority: CapabilityToken,
    ) -> Self {
        Self {
            provider,
            storage,
            security,
            tools,
            approval,
            scheduler,
            event_bus,
            registry,
            parent_sandbox,
            spool,
            authority,
            root_authority,
        }
    }

    /// Deregister an agent from the registry (used by perf tests to avoid NFR15 limit).
    pub async fn deregister(&self, agent_id: &AgentId) {
        self.registry.deregister(agent_id).await;
    }

    /// Access the underlying registry (for integration tests and observers).
    pub fn registry(&self) -> Arc<NodeTree> {
        self.registry.clone()
    }
}

#[async_trait]
impl SubagentRunner for InProcessSubagentRunner {
    async fn launch(
        &self,
        spec: AgentLaunchSpec,
        cancel: CancellationToken,
    ) -> Result<TaskHandle, SubagentError> {
        // 1. Validate sandbox narrowing BEFORE spawn
        if let Some(ref child_policy) = spec.sandbox_override {
            let parent_policy = self.parent_sandbox.read().await.clone();
            validate_narrowing(&parent_policy, child_policy)?;
        }

        // 2. Create channels
        let (command_tx, command_rx) = mpsc::channel::<Op>(512);
        let (status_tx, status_rx) = mpsc::channel::<NodeState>(512);
        let (bridge_tx, bridge_rx) = mpsc::channel::<NodeState>(64);
        // DF-310: Structured yield channel for fork-join result contract.
        // The child emits a JSON-serialized SpokeYield{summary,detail} before
        // its terminal NodeState so the executor's collect_terminal can drain it.
        let (yield_tx, yield_rx) = mpsc::channel::<String>(4);

        // 3. Derive child cancellation token
        let child_cancel = cancel.child_token();
        let subagent_type = String::from("in-process"); // overridden by caller via TaskHandle

        // 4. Generate IDs
        let agent_id = AgentId::new();
        let task_id = nanoid::nanoid!(12);
        let started_at_ms = chrono::Utc::now().timestamp_millis();
        let child_token = self
            .authority
            .delegate(
                &self.root_authority,
                CapabilityToken::r1_child_request(agent_id.clone()),
            )
            .await
            .map_err(|err| {
                SubagentError::Internal(format!("authority delegation failed: {err}"))
            })?;

        // 5. Construct ChildState (before spawn so it can be cloned into closure)
        let child_state = Arc::new(crate::adapters::subagent::ChildState::new(
            spec.effective_model.clone(),
            spec.tools_allow.clone(),
        ));
        let metrics_rx_for_register = child_state.metrics.subscribe();
        let mut metrics_rx_for_bridge = child_state.metrics.subscribe();
        let (parent_disconnect_tx, parent_disconnect_rx) = tokio::sync::mpsc::unbounded_channel();

        // 6. Spawn child task
        let provider = self.provider.clone();
        let scheduler = self.scheduler.clone();
        let approval = self.approval.clone();
        let event_bus = self.event_bus.clone();
        let spool = self.spool.clone();
        let child_agent_id = agent_id.clone();
        let child_task_id = task_id.clone();
        let child_cancel_for_spawn = child_cancel.clone();
        let status_tx_for_panic = status_tx.clone();
        let subagent_type_for_spawn = subagent_type.clone();
        let child_state_for_spawn = child_state.clone();
        let bridge_tx_for_spawn = bridge_tx.clone();
        let bridge_tx_for_panic = bridge_tx.clone();

        let tools = self.tools.clone();
        let security = self.security.clone();
        let authority_for_spawn = self.authority.clone();
        let child_token_for_spawn = child_token.clone();
        let _handle = tokio::spawn(async move {
            let result = std::panic::AssertUnwindSafe(run_child(
                spec,
                provider,
                scheduler,
                approval,
                event_bus,
                spool,
                tools,
                security,
                status_tx,
                bridge_tx_for_spawn,
                command_rx,
                child_cancel_for_spawn.clone(),
                OwnershipKind::Owned, // R1: all in-process nodes are Owned; 14.2 sources this from the agent def
                parent_disconnect_rx,
                child_agent_id,
                child_task_id,
                subagent_type_for_spawn.clone(),
                started_at_ms,
                child_state_for_spawn,
                child_token_for_spawn,
                authority_for_spawn,
                yield_tx,
            ))
            .catch_unwind()
            .await;

            if let Err(panic_payload) = result {
                let msg = format!("{:?}", panic_payload);
                tracing::error!(%msg, "Subagent task panicked");
                let _ = status_tx_for_panic.send(NodeState::Failed).await;
                let _ = bridge_tx_for_panic.send(NodeState::Failed).await;
                child_cancel_for_spawn.cancel();
            }
        });

        // 7. Register with registry
        let (watch_tx, _watch_rx) = tokio::sync::watch::channel(NodeState::Created);
        let reg_handle = crate::infrastructure::subagent::registry::AgentHandle {
            agent_id: agent_id.clone(),
            token: child_token.id,
            command_tx: command_tx.clone(),
            // The child task's REAL cancellation token — stored so cascade_kill
            // can interrupt the task at any await point (not an orphan token).
            cancel_token: child_cancel.clone(),
            depth: 0, // overwritten by registry::register()
            subagent_type: subagent_type.clone(),
            spawned_at: 0,
            status: watch_tx,
            metrics: metrics_rx_for_register,
        };
        self.registry
            .register(agent_id.clone(), AgentId::root(), reg_handle)
            .await?;

        // Read spawned_at from registry for TaskHandle
        let spawned_at = self
            .registry
            .list()
            .await
            .into_iter()
            .find(|e| e.agent_id == agent_id)
            .map(|e| e.spawned_at)
            .unwrap_or(0);

        // 8. Spawn status bridge task (mirrors mpsc → registry watch)
        let registry = self.registry.clone();
        let agent_id_for_bridge = agent_id.clone();
        let authority_for_bridge = self.authority.clone();
        let child_token_id_for_bridge = child_token.id;
        tokio::spawn(async move {
            let mut rx = bridge_rx;
            while let Some(s) = rx.recv().await {
                // Apply + broadcast happen inside set_state, gated on FSM
                // acceptance so the watch never exposes a rejected transition.
                registry.set_state(&agent_id_for_bridge, s).await;
                if matches!(
                    s,
                    NodeState::Completed | NodeState::Failed | NodeState::Cancelled
                ) {
                    // AC4: settle the child's reservation on ANY terminal (idempotent)
                    // so the parent reclaims reserved − consumed. Cancelled-via-cascade
                    // also settles via revoke_scope; the `settled` latch collapses the
                    // double-fire to a single credit.
                    let _ = authority_for_bridge
                        .settle(&child_token_id_for_bridge)
                        .await;
                    registry.deregister(&agent_id_for_bridge).await;
                    break;
                }
            }
            // Safety net: if bridge_rx closed without a terminal state (e.g.
            // panic where bridge_tx.send also failed), ensure the registry
            // entry is cleaned up.
            let entries = registry.list().await;
            if entries.iter().any(|e| e.agent_id == agent_id_for_bridge) {
                tracing::warn!(agent_id = %agent_id_for_bridge.0, "Bridge task exiting without terminal state — force deregistering");
                registry.deregister(&agent_id_for_bridge).await;
            }
        });

        // 9. Spawn metrics bridge task (ChildState watch → NodeTree live fields)
        let registry = self.registry.clone();
        let agent_id_for_metrics = agent_id.clone();
        tokio::spawn(async move {
            let initial_metrics = metrics_rx_for_bridge.borrow().clone();
            registry
                .set_metrics(&agent_id_for_metrics, initial_metrics)
                .await;
            registry.emit_status_updated(&agent_id_for_metrics).await;
            while metrics_rx_for_bridge.changed().await.is_ok() {
                let metrics = metrics_rx_for_bridge.borrow().clone();
                registry.set_metrics(&agent_id_for_metrics, metrics).await;
                registry.emit_status_updated(&agent_id_for_metrics).await;
            }
        });

        Ok(TaskHandle {
            agent_id,
            status_rx,
            command_tx,
            cancel: child_cancel,
            task_id,
            subagent_type,
            spawned_at,
            parent_disconnect: parent_disconnect_tx,
            // DF-310 (Story 14.3b): yield_rx is now LIVE — run_child emits a
            // JSON-serialized SpokeYield{summary,detail} before the terminal
            // NodeState on Completed/cancel-salvage paths. The executor's
            // collect_terminal drains this channel to populate the ResultStore.
            yield_rx: Some(yield_rx),
        })
    }
}

/// DF-310: Build and emit a structured `SpokeYield` through the yield channel
/// BEFORE the terminal `emit_status`. The executor's `collect_terminal` drains
/// `yield_rx` to populate the `ResultStore`.
///
/// `terminal` selects the path:
/// - `Completed` → summary from `spoke_summary(accumulated_text)`, detail = full text
/// - `Cancelled` → salvage partial output via `salvage_on_cancel`
/// - `Failed` / other → no yield (contract does not validate failed outcomes)
async fn emit_yield(terminal: NodeState, accumulated_text: &str, yield_tx: &mpsc::Sender<String>) {
    use crate::infrastructure::orchestrator::{SPOKE_SUMMARY_MAX_BYTES, SpokeYield, spoke_summary};

    match terminal {
        NodeState::Completed => {
            let summary = spoke_summary(accumulated_text, SPOKE_SUMMARY_MAX_BYTES).to_string();
            let detail = accumulated_text.to_string();
            let sy = SpokeYield { summary, detail };
            if let Ok(json) = serde_json::to_string(&sy) {
                let _ = yield_tx.send(json).await;
            }
        }
        NodeState::Cancelled => {
            // Salvage whatever partial output exists — the outcome stays Cancelled.
            // We emit the raw text so the executor's salvage_on_cancel can extract it.
            if !accumulated_text.is_empty() {
                let sy = SpokeYield {
                    summary: spoke_summary(accumulated_text, SPOKE_SUMMARY_MAX_BYTES).to_string(),
                    detail: accumulated_text.to_string(),
                };
                if let Ok(json) = serde_json::to_string(&sy) {
                    let _ = yield_tx.send(json).await;
                }
            }
        }
        _ => {
            // Failed / other terminals: contract does not validate failed outcomes.
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_child(
    spec: AgentLaunchSpec,
    provider: Arc<dyn crate::domain::ports::StreamingProvider>,
    scheduler: Arc<crate::domain::services::tool_scheduler::ToolScheduler>,
    _approval: Arc<crate::domain::services::approval_runtime::ApprovalRuntime>,
    _event_bus: Arc<crate::infrastructure::runtime::event_bus::EventBus>,
    spool: Arc<SubagentSpool>,
    tools: Arc<dyn crate::domain::ports::ToolSetPort>,
    _security: Arc<dyn crate::domain::ports::SecurityPort>,
    status_tx: mpsc::Sender<NodeState>,
    bridge_tx: mpsc::Sender<NodeState>,
    mut command_rx: mpsc::Receiver<Op>,
    cancel: CancellationToken,
    ownership: OwnershipKind,
    mut parent_disconnect_rx: tokio::sync::mpsc::UnboundedReceiver<()>,
    agent_id: AgentId,
    task_id: String,
    subagent_type: String,
    started_at_ms: i64,
    child_state: Arc<crate::adapters::subagent::ChildState>,
    child_token: CapabilityToken,
    authority: Arc<dyn AuthorityProvider>,
    yield_tx: mpsc::Sender<String>,
) {
    use crate::domain::models::tool_call::ApprovalSource;
    use crate::domain::models::{
        AbandonmentAction, CompletionOptions, Message, MessageRole, StopReason, StreamChunk,
        ToolCallInfo, ToolCallRequest, ToolResultMessage, ToolUseMessage, abandonment_action,
    };
    use futures::StreamExt;

    // Helper: emit status to both channels, update ChildState watch sender, and
    // persist the latest sidecar snapshot (`SpoolMeta`) for inspector/recovery.
    async fn emit_status(
        s: NodeState,
        status_tx: &mpsc::Sender<NodeState>,
        bridge_tx: &mpsc::Sender<NodeState>,
        child_state: &crate::adapters::subagent::ChildState,
        spool: &SubagentSpool,
        task_id: &str,
        subagent_type: &str,
        agent_id: &AgentId,
        started_at_ms: i64,
    ) {
        if let Err(e) = status_tx.send(s).await {
            tracing::warn!(error = %e, "status_tx send failed");
        }
        if let Err(e) = bridge_tx.send(s).await {
            tracing::warn!(error = %e, "bridge_tx send failed");
        }
        let _ = child_state.status.send_replace(s);

        let metrics = child_state.current_metrics();
        let meta = SpoolMeta {
            status: s,
            tokens_in: metrics.tokens_in,
            tokens_out: metrics.tokens_out,
            started_at: started_at_ms,
            ended_at: if s.is_terminal() {
                Some(chrono::Utc::now().timestamp_millis())
            } else {
                None
            },
            subagent_type: subagent_type.to_string(),
            agent_id: agent_id.0.clone(),
        };
        if let Err(e) = spool.write_meta(task_id, &meta).await {
            tracing::warn!(task_id = %task_id, error = %e, "Spool meta write failed");
        }
    }

    const ABANDONMENT_MAX_RETRIES: u8 = 3;
    const ABANDONMENT_RETRY_BACKOFF_MS: u64 = 100;

    async fn handle_abandonment_disconnect(
        retry_count: &mut u8,
        status_tx: &mpsc::Sender<NodeState>,
        bridge_tx: &mpsc::Sender<NodeState>,
        child_state: &crate::adapters::subagent::ChildState,
        spool: &SubagentSpool,
        task_id: &str,
        subagent_type: &str,
        agent_id: &AgentId,
        started_at_ms: i64,
        ownership: OwnershipKind,
    ) -> bool {
        match abandonment_action(ownership, true, *retry_count, ABANDONMENT_MAX_RETRIES) {
            AbandonmentAction::Continue | AbandonmentAction::Ignore => false,
            AbandonmentAction::Retry => {
                *retry_count = retry_count.saturating_add(1);
                emit_status(
                    NodeState::Waiting,
                    status_tx,
                    bridge_tx,
                    child_state,
                    spool,
                    task_id,
                    subagent_type,
                    agent_id,
                    started_at_ms,
                )
                .await;
                tokio::time::sleep(tokio::time::Duration::from_millis(
                    ABANDONMENT_RETRY_BACKOFF_MS,
                ))
                .await;
                false
            }
            AbandonmentAction::SelfDestruct => {
                emit_status(
                    NodeState::Cancelled,
                    status_tx,
                    bridge_tx,
                    child_state,
                    spool,
                    task_id,
                    subagent_type,
                    agent_id,
                    started_at_ms,
                )
                .await;
                true
            }
        }
    }

    // Emit Running
    emit_status(
        NodeState::Running,
        &status_tx,
        &bridge_tx,
        &child_state,
        &spool,
        &task_id,
        &subagent_type,
        &agent_id,
        started_at_ms,
    )
    .await;

    // Build initial messages from the prompt
    let mut messages = vec![Message {
        role: MessageRole::User,
        content: spec.prompt.clone(),
        images: vec![],
        tool_results: vec![],
        tool_uses: vec![],
        context_prefix: None,
        reasoning_content: None,
    }];
    let mut abandonment_retry_count: u8 = 0;
    let mut parked_queue: VecDeque<crate::domain::models::AgentDelivery> = VecDeque::new();
    const PARKED_QUEUE_CAP: usize = 64;

    let max_iterations = 10;

    for _iteration in 0..max_iterations {
        // Check cancellation first
        if cancel.is_cancelled() {
            emit_status(
                NodeState::Cancelled,
                &status_tx,
                &bridge_tx,
                &child_state,
                &spool,
                &task_id,
                &subagent_type,
                &agent_id,
                started_at_ms,
            )
            .await;
            return;
        }

        // Wait if paused
        while child_state
            .paused
            .load(std::sync::atomic::Ordering::Acquire)
        {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    emit_status(NodeState::Cancelled, &status_tx, &bridge_tx, &child_state, &spool, &task_id, &subagent_type, &agent_id, started_at_ms).await;
                    return;
                }
                maybe_disconnect = parent_disconnect_rx.recv() => {
                    if maybe_disconnect.is_none() {
                        if handle_abandonment_disconnect(&mut abandonment_retry_count, &status_tx, &bridge_tx, &child_state, &spool, &task_id, &subagent_type, &agent_id, started_at_ms, ownership).await {
                            return;
                        }
                    }
                }
                maybe_op = command_rx.recv() => {
                    match maybe_op {
                        Some(op) => match op {
                            Op::Resume => {
                                child_state.paused.store(false, std::sync::atomic::Ordering::Release);
                                emit_status(NodeState::Running, &status_tx, &bridge_tx, &child_state, &spool, &task_id, &subagent_type, &agent_id, started_at_ms).await;
                                while let Some(delivery) = parked_queue.pop_front() {
                                    messages.push(Message {
                                        role: MessageRole::User,
                                        content: delivery.envelope.body.content,
                                        images: vec![],
                                        tool_results: vec![],
                                        tool_uses: vec![],
                                        context_prefix: None,
                                        reasoning_content: None,
                                    });
                                }
                            }
                            Op::Kill => {
                                emit_status(NodeState::Cancelled, &status_tx, &bridge_tx, &child_state, &spool, &task_id, &subagent_type, &agent_id, started_at_ms).await;
                                return;
                            }
                            Op::Deliver(delivery) => {
                                if parked_queue.len() < PARKED_QUEUE_CAP {
                                    parked_queue.push_back(delivery);
                                } else {
                                    tracing::warn!(agent_id = %agent_id.0, "parked agent message queue full; refusing message");
                                }
                            }
                            _ => {}
                        },
                        None => {
                            emit_status(NodeState::Completed, &status_tx, &bridge_tx, &child_state, &spool, &task_id, &subagent_type, &agent_id, started_at_ms).await;
                            return;
                        }
                    }
                }
            }
        }

        // Drain any ready owner commands without blocking. Uses the same
        // selectable receiver as the streaming loop; no residual try_recv path.
        loop {
            match parent_disconnect_rx.recv().now_or_never() {
                Some(None) => {
                    if handle_abandonment_disconnect(
                        &mut abandonment_retry_count,
                        &status_tx,
                        &bridge_tx,
                        &child_state,
                        &spool,
                        &task_id,
                        &subagent_type,
                        &agent_id,
                        started_at_ms,
                        ownership,
                    )
                    .await
                    {
                        return;
                    }
                    continue;
                }
                Some(Some(_)) => {}
                None => {}
            }
            let Some(maybe_op) = command_rx.recv().now_or_never() else {
                break;
            };
            let Some(op) = maybe_op else {
                emit_status(
                    NodeState::Completed,
                    &status_tx,
                    &bridge_tx,
                    &child_state,
                    &spool,
                    &task_id,
                    &subagent_type,
                    &agent_id,
                    started_at_ms,
                )
                .await;
                return;
            };
            match op {
                Op::Kill => {
                    emit_status(
                        NodeState::Cancelled,
                        &status_tx,
                        &bridge_tx,
                        &child_state,
                        &spool,
                        &task_id,
                        &subagent_type,
                        &agent_id,
                        started_at_ms,
                    )
                    .await;
                    return;
                }
                Op::Pause => {
                    child_state
                        .paused
                        .store(true, std::sync::atomic::Ordering::Release);
                    emit_status(
                        NodeState::Suspended,
                        &status_tx,
                        &bridge_tx,
                        &child_state,
                        &spool,
                        &task_id,
                        &subagent_type,
                        &agent_id,
                        started_at_ms,
                    )
                    .await;
                    break;
                }
                Op::ChangeModel(new_model) => {
                    child_state
                        .effective_model
                        .store(Arc::new(new_model.clone()));
                    child_state.update_metrics(|m| m.effective_model = new_model);
                }
                Op::UpdateTools(allowlist) => {
                    let policy = crate::domain::models::ToolPolicy::Allowlist {
                        tools: allowlist.into_iter().collect(),
                    };
                    let summary =
                        crate::adapters::subagent::child_state::tool_policy_summary(&policy);
                    child_state.tools_allow.store(Arc::new(policy));
                    child_state.update_metrics(|m| m.tools_summary = summary);
                }
                Op::ReportFull => {
                    let current = *child_state.status.borrow();
                    emit_status(
                        current,
                        &status_tx,
                        &bridge_tx,
                        &child_state,
                        &spool,
                        &task_id,
                        &subagent_type,
                        &agent_id,
                        started_at_ms,
                    )
                    .await;
                }
                Op::Resume => {
                    child_state
                        .paused
                        .store(false, std::sync::atomic::Ordering::Release);
                    emit_status(
                        NodeState::Running,
                        &status_tx,
                        &bridge_tx,
                        &child_state,
                        &spool,
                        &task_id,
                        &subagent_type,
                        &agent_id,
                        started_at_ms,
                    )
                    .await;
                }
                Op::Deliver(delivery) => match delivery.mode {
                    crate::domain::models::DeliveryMode::Queue => {
                        if parked_queue.len() < PARKED_QUEUE_CAP {
                            parked_queue.push_back(delivery);
                        } else {
                            tracing::warn!(agent_id = %agent_id.0, "parked agent message queue full; refusing message");
                        }
                    }
                    crate::domain::models::DeliveryMode::Aside
                    | crate::domain::models::DeliveryMode::Wake => {
                        messages.push(Message {
                            role: MessageRole::User,
                            content: delivery.envelope.body.content,
                            images: vec![],
                            tool_results: vec![],
                            tool_uses: vec![],
                            context_prefix: None,
                            reasoning_content: None,
                        });
                    }
                    crate::domain::models::DeliveryMode::Refuse => {}
                },
            }
        }
        if child_state
            .paused
            .load(std::sync::atomic::Ordering::Acquire)
        {
            continue; // Go back to pause wait loop
        }

        // Build completion options from ChildState
        let model = (*child_state.effective_model.load_full()).clone();
        // P9 fix: filter available tools by the current ToolPolicy from ChildState
        let all_tools = tools.available_tools();
        let policy = child_state.tools_allow.load_full();
        let filtered_tools = match (*policy).clone() {
            crate::domain::models::ToolPolicy::Allowlist { tools: allowed } => all_tools
                .into_iter()
                .filter(|t| allowed.contains(&t.name))
                .collect(),
            crate::domain::models::ToolPolicy::Denylist { tools: denied } => all_tools
                .into_iter()
                .filter(|t| !denied.contains(&t.name))
                .collect(),
            crate::domain::models::ToolPolicy::InheritFromParent => all_tools,
        };
        let options = CompletionOptions {
            model,
            max_tokens: 4096,
            system_prompt: String::new(),
            temperature: None,
            tools: filtered_tools,
        };

        // Stream completion
        let stream_result = provider.stream_completion(messages.clone(), options).await;
        let mut stream = match stream_result {
            Ok(s) => s,
            Err(e) => {
                let msg = format!("Provider error: {}", e);
                if let Err(spool_err) = spool.append(&task_id, msg.as_bytes()).await {
                    tracing::warn!(task_id = %task_id, error = %spool_err, "Spool append failed for provider error");
                }
                emit_status(
                    NodeState::Failed,
                    &status_tx,
                    &bridge_tx,
                    &child_state,
                    &spool,
                    &task_id,
                    &subagent_type,
                    &agent_id,
                    started_at_ms,
                )
                .await;
                return;
            }
        };

        let mut accumulated_text = String::new();
        let mut tool_calls: Vec<ToolCallInfo> = Vec::new();
        let mut received_turn_complete = false;
        let mut stop_reason = StopReason::EndTurn;

        loop {
            let chunk = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    emit_yield(NodeState::Cancelled, &accumulated_text, &yield_tx).await;
                    emit_status(NodeState::Cancelled, &status_tx, &bridge_tx, &child_state, &spool, &task_id, &subagent_type, &agent_id, started_at_ms).await;
                    return;
                }
                maybe_disconnect = parent_disconnect_rx.recv() => {
                    if maybe_disconnect.is_none() {
                        if handle_abandonment_disconnect(&mut abandonment_retry_count, &status_tx, &bridge_tx, &child_state, &spool, &task_id, &subagent_type, &agent_id, started_at_ms, ownership).await {
                            return;
                        }
                    }
                    continue;
                }
                maybe_op = command_rx.recv() => {
                    match maybe_op {
                        Some(Op::Kill) => {
                            emit_yield(NodeState::Cancelled, &accumulated_text, &yield_tx).await;
                            emit_status(NodeState::Cancelled, &status_tx, &bridge_tx, &child_state, &spool, &task_id, &subagent_type, &agent_id, started_at_ms).await;
                            return;
                        }
                        Some(Op::Pause) => {
                            child_state.paused.store(true, std::sync::atomic::Ordering::Release);
                            emit_status(NodeState::Suspended, &status_tx, &bridge_tx, &child_state, &spool, &task_id, &subagent_type, &agent_id, started_at_ms).await;
                            break;
                        }
                        Some(Op::ChangeModel(new_model)) => {
                            child_state.effective_model.store(Arc::new(new_model.clone()));
                            child_state.update_metrics(|m| m.effective_model = new_model);
                            continue;
                        }
                        Some(Op::UpdateTools(allowlist)) => {
                            let policy = crate::domain::models::ToolPolicy::Allowlist {
                                tools: allowlist.into_iter().collect(),
                            };
                            let summary = crate::adapters::subagent::child_state::tool_policy_summary(&policy);
                            child_state.tools_allow.store(Arc::new(policy));
                            child_state.update_metrics(|m| m.tools_summary = summary);
                            continue;
                        }
                        Some(Op::ReportFull) => {
                            let current = *child_state.status.borrow();
                            emit_status(current, &status_tx, &bridge_tx, &child_state, &spool, &task_id, &subagent_type, &agent_id, started_at_ms).await;
                            continue;
                        }
                        Some(Op::Resume) => {
                            child_state.paused.store(false, std::sync::atomic::Ordering::Release);
                            emit_status(NodeState::Running, &status_tx, &bridge_tx, &child_state, &spool, &task_id, &subagent_type, &agent_id, started_at_ms).await;
                            continue;
                        }
                        Some(Op::Deliver(delivery)) => {
                            match delivery.mode {
                                crate::domain::models::DeliveryMode::Queue => {
                                    if parked_queue.len() < PARKED_QUEUE_CAP {
                                        parked_queue.push_back(delivery);
                                    } else {
                                        tracing::warn!(agent_id = %agent_id.0, "parked agent message queue full; refusing message");
                                    }
                                }
                                crate::domain::models::DeliveryMode::Aside
                                | crate::domain::models::DeliveryMode::Wake => {
                                    messages.push(Message {
                                        role: MessageRole::User,
                                        content: delivery.envelope.body.content,
                                        images: vec![],
                                        tool_results: vec![],
                                        tool_uses: vec![],
                                        context_prefix: None,
                                        reasoning_content: None,
                                    });
                                }
                                crate::domain::models::DeliveryMode::Refuse => {}
                            }
                            continue;
                        }
                        None => {
                            emit_status(NodeState::Completed, &status_tx, &bridge_tx, &child_state, &spool, &task_id, &subagent_type, &agent_id, started_at_ms).await;
                            return;
                        }
                    }
                }
                maybe_chunk = stream.next() => {
                    match maybe_chunk {
                        Some(c) => c,
                        None => break,
                    }
                }
            };

            match &chunk {
                StreamChunk::TurnComplete { stop_reason: sr } => {
                    received_turn_complete = true;
                    stop_reason = sr.clone();
                }
                StreamChunk::Usage { usage, .. } => {
                    child_state.update_metrics(|m| {
                        m.tokens_in = usage.input_tokens;
                        m.tokens_out = usage.output_tokens;
                    });
                }
                StreamChunk::ToolUse { id, name, input } => {
                    tool_calls.push(ToolCallInfo {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                        result: None,
                        started_at_ms: None,
                        completed_at_ms: None,
                        status: None,
                    });
                }
                StreamChunk::Text { content, .. } => {
                    accumulated_text.push_str(content);
                    if let Err(e) = spool.append(&task_id, content.as_bytes()).await {
                        tracing::warn!(task_id = %task_id, error = %e, "Spool append failed for text chunk");
                    }
                }
                StreamChunk::Thinking { content, .. } => {
                    if let Err(e) = spool.append(&task_id, content.as_bytes()).await {
                        tracing::warn!(task_id = %task_id, error = %e, "Spool append failed for thinking chunk");
                    }
                }
                StreamChunk::Error { content } => {
                    if let Err(e) = spool
                        .append(&task_id, format!("ERROR: {}", content).as_bytes())
                        .await
                    {
                        tracing::warn!(task_id = %task_id, error = %e, "Spool append failed for error chunk");
                    }
                }
                _ => {}
            }
        }

        // If the streaming loop exited because of Op::Pause, re-enter the
        // outer for-loop which hits the `while paused` wait block at the top.
        // Do NOT fall through to the Failed path — the stream was intentionally
        // interrupted, not abnormally ended.
        if child_state
            .paused
            .load(std::sync::atomic::Ordering::Acquire)
        {
            continue;
        }

        if !received_turn_complete {
            tracing::warn!(agent_id = %agent_id.0, "Provider stream ended without TurnComplete");
            emit_status(
                NodeState::Failed,
                &status_tx,
                &bridge_tx,
                &child_state,
                &spool,
                &task_id,
                &subagent_type,
                &agent_id,
                started_at_ms,
            )
            .await;
            return;
        }

        child_state.update_metrics(|m| m.turns = m.turns.saturating_add(1));
        match stop_reason {
            StopReason::ToolUse => {
                if tool_calls.is_empty() {
                    emit_yield(NodeState::Completed, &accumulated_text, &yield_tx).await;
                    emit_status(
                        NodeState::Completed,
                        &status_tx,
                        &bridge_tx,
                        &child_state,
                        &spool,
                        &task_id,
                        &subagent_type,
                        &agent_id,
                        started_at_ms,
                    )
                    .await;
                    return;
                }

                // Build assistant message with tool uses
                let tool_use_msgs: Vec<ToolUseMessage> = tool_calls
                    .iter()
                    .map(|tc| ToolUseMessage {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        input: tc.input.clone(),
                    })
                    .collect();
                messages.push(Message {
                    role: MessageRole::Assistant,
                    content: std::mem::take(&mut accumulated_text),
                    images: vec![],
                    tool_results: vec![],
                    tool_uses: tool_use_msgs,
                    context_prefix: None,
                    reasoning_content: None,
                });

                // Dispatch tool calls through scheduler with ForegroundSubagent source
                let requests: Vec<ToolCallRequest> = tool_calls
                    .drain(..)
                    .map(|tc| ToolCallRequest {
                        id: tc.id,
                        tool_name: tc.name,
                        input: tc.input,
                    })
                    .collect();

                // AC9 point-of-use authority gate (the security keystone): validate the
                // child's delegated token before dispatching its tool calls. A revoked /
                // expired / use-exhausted / budget-exhausted token is denied its next
                // gated action. R1 children carry all flags so this is decisive on
                // revocation/TTL/use-count/budget; per-tool flag classification is an
                // R2 refinement (the validate() check itself enforces the held flag set).
                let gate = authority
                    .validate(&child_token, &CapabilityFlag::ReadFs, &agent_id)
                    .await;
                // AC4/AC9 budget-spend: charge one use at the point of use
                // (uses consumed at invoke are not refunded). validate() is the
                // check; spend_use() is the commit.
                let gate = match gate {
                    Ok(()) => authority.spend_use(&child_token.id).await,
                    Err(e) => Err(e),
                };
                if let Err(err) = gate {
                    let blocked: Vec<ToolResultMessage> = requests
                        .iter()
                        .map(|r| ToolResultMessage {
                            tool_use_id: r.id.clone(),
                            content: format!("Blocked by authority: {err}"),
                            is_error: true,
                        })
                        .collect();
                    for tr in &blocked {
                        if let Err(e) = spool
                            .append(
                                &task_id,
                                format!("\n[tool result]: {}\n", tr.content).as_bytes(),
                            )
                            .await
                        {
                            tracing::warn!(task_id = %task_id, error = %e, "Spool append failed for authority denial");
                        }
                    }
                    messages.push(Message {
                        role: MessageRole::User,
                        content: String::new(),
                        images: vec![],
                        tool_results: blocked,
                        tool_uses: vec![],
                        context_prefix: None,
                        reasoning_content: None,
                    });
                    accumulated_text.clear();
                    continue;
                }
                let source = ApprovalSource::ForegroundSubagent {
                    conversation_id: agent_id.0.clone(),
                    parent_tool_call_id: task_id.clone(),
                    subagent_type: "in-process".to_string(),
                };

                let terminal = scheduler
                    .clone()
                    .schedule(source, requests, cancel.clone(), None)
                    .await;

                let mut tool_result_messages: Vec<ToolResultMessage> = Vec::new();
                for call in terminal {
                    let (id, content, is_error) = match call {
                        crate::domain::models::ToolCall::Success { id, result, .. } => {
                            (id, result.output, result.is_error)
                        }
                        crate::domain::models::ToolCall::Error { id, error, .. } => {
                            (id, error, true)
                        }
                        crate::domain::models::ToolCall::Cancelled { id, reason, .. } => {
                            (id, format!("Tool execution cancelled: {}", reason), true)
                        }
                        _ => continue,
                    };
                    tool_result_messages.push(ToolResultMessage {
                        tool_use_id: id,
                        content: content.clone(),
                        is_error,
                    });
                    if let Err(e) = spool
                        .append(
                            &task_id,
                            format!("\n[tool result]: {}\n", content).as_bytes(),
                        )
                        .await
                    {
                        tracing::warn!(task_id = %task_id, error = %e, "Spool append failed for tool result");
                    }
                }

                // Append tool results as user message
                messages.push(Message {
                    role: MessageRole::User,
                    content: String::new(),
                    images: vec![],
                    tool_results: tool_result_messages,
                    tool_uses: vec![],
                    context_prefix: None,
                    reasoning_content: None,
                });

                // Loop back for next turn
                continue;
            }
            StopReason::EndTurn | StopReason::MaxTokens => {
                // P1 fix: text was already appended per-chunk during streaming; no redundant write
                emit_yield(NodeState::Completed, &accumulated_text, &yield_tx).await;
                emit_status(
                    NodeState::Completed,
                    &status_tx,
                    &bridge_tx,
                    &child_state,
                    &spool,
                    &task_id,
                    &subagent_type,
                    &agent_id,
                    started_at_ms,
                )
                .await;
                return;
            }
            StopReason::Cancelled => {
                emit_yield(NodeState::Cancelled, &accumulated_text, &yield_tx).await;
                emit_status(
                    NodeState::Cancelled,
                    &status_tx,
                    &bridge_tx,
                    &child_state,
                    &spool,
                    &task_id,
                    &subagent_type,
                    &agent_id,
                    started_at_ms,
                )
                .await;
                return;
            }
        }
    }

    // P5 fix: Max iterations reached — emit Failed, not Completed
    tracing::warn!(agent_id = %agent_id.0, "Subagent reached max tool iterations");
    let _ = spool
        .append(&task_id, b"WARNING: max tool iterations reached\n")
        .await;
    emit_status(
        NodeState::Failed,
        &status_tx,
        &bridge_tx,
        &child_state,
        &spool,
        &task_id,
        &subagent_type,
        &agent_id,
        started_at_ms,
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::filesystem::FileSystemStorage;
    use crate::adapters::noop::{NoOpApprovalPersistence, NoOpProvider};
    use crate::adapters::sandbox::NoOpSandbox;
    use crate::adapters::security_adapter::SecurityAdapter;
    use crate::adapters::toolset_adapter::ToolSetAdapter;
    use crate::domain::models::{CompletionOptions, Message, ModelDescriptor, StreamChunk};
    use crate::domain::ports::StreamingProvider;
    use crate::domain::services::approval_runtime::ApprovalRuntime;
    use crate::domain::services::tool_scheduler::ToolScheduler;
    use crate::infrastructure::runtime::event_bus::EventBus;
    use arc_swap::ArcSwap;
    use futures::{StreamExt, stream::BoxStream};
    use std::path::PathBuf;

    struct HangingProvider;

    #[async_trait::async_trait]
    impl StreamingProvider for HangingProvider {
        async fn stream_completion(
            &self,
            _messages: Vec<Message>,
            _options: CompletionOptions,
        ) -> Result<BoxStream<'static, StreamChunk>, crate::domain::errors::ProviderError> {
            let chunks = vec![StreamChunk::Text {
                content: "working...".into(),
                parent_tool_use_id: None,
            }];
            let stream = futures::stream::iter(chunks).chain(futures::stream::pending());
            Ok(Box::pin(stream))
        }

        async fn abort(&self) -> Result<(), crate::domain::errors::ProviderError> {
            Ok(())
        }

        fn provider_id(&self) -> String {
            "hanging".into()
        }

        fn list_models(&self) -> Vec<ModelDescriptor> {
            vec![]
        }

        async fn health_check(&self) -> Result<(), crate::domain::errors::ProviderError> {
            Ok(())
        }
        async fn connectivity_probe(
            &self,
        ) -> Result<crate::domain::ports::ProbeOutcome, crate::domain::errors::ProviderError>
        {
            Ok(crate::domain::ports::ProbeOutcome {
                latency: std::time::Duration::ZERO,
            })
        }
    }

    struct UsageOnceProvider;

    #[async_trait::async_trait]
    impl StreamingProvider for UsageOnceProvider {
        async fn stream_completion(
            &self,
            _messages: Vec<Message>,
            _options: CompletionOptions,
        ) -> Result<BoxStream<'static, StreamChunk>, crate::domain::errors::ProviderError> {
            let chunks = vec![
                StreamChunk::Usage {
                    usage: crate::domain::models::UsageInfo {
                        input_tokens: 12,
                        output_tokens: 34,
                        cache_creation_input_tokens: None,
                        cache_read_input_tokens: None,
                        reasoning_tokens: None,
                    },
                    session_id: None,
                },
                StreamChunk::Text {
                    content: "done".into(),
                    parent_tool_use_id: None,
                },
                StreamChunk::TurnComplete {
                    stop_reason: crate::domain::models::StopReason::EndTurn,
                },
            ];
            Ok(Box::pin(futures::stream::iter(chunks)))
        }

        async fn abort(&self) -> Result<(), crate::domain::errors::ProviderError> {
            Ok(())
        }

        fn provider_id(&self) -> String {
            "usage-once".into()
        }

        fn list_models(&self) -> Vec<ModelDescriptor> {
            vec![]
        }

        async fn health_check(&self) -> Result<(), crate::domain::errors::ProviderError> {
            Ok(())
        }
        async fn connectivity_probe(
            &self,
        ) -> Result<crate::domain::ports::ProbeOutcome, crate::domain::errors::ProviderError>
        {
            Ok(crate::domain::ports::ProbeOutcome {
                latency: std::time::Duration::ZERO,
            })
        }
    }

    fn authority_pair() -> (
        Arc<dyn crate::domain::ports::AuthorityProvider>,
        crate::domain::models::CapabilityToken,
    ) {
        let root = crate::domain::models::CapabilityToken::r1_root(AgentId::root());
        let ledger =
            Arc::new(crate::domain::services::authority_ledger::AuthorityLedger::new(root.clone()));
        (
            Arc::new(crate::adapters::authority::InProcessAuthorityProvider::new(
                ledger,
            )) as Arc<dyn crate::domain::ports::AuthorityProvider>,
            root,
        )
    }

    async fn make_runner() -> (InProcessSubagentRunner, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let provider = Arc::new(NoOpProvider) as Arc<dyn StreamingProvider>;
        let storage = Arc::new(FileSystemStorage::new(tmp.path().to_path_buf()))
            as Arc<dyn crate::domain::ports::StoragePort>;
        let security = Arc::new(SecurityAdapter::new(PathBuf::from(".")))
            as Arc<dyn crate::domain::ports::SecurityPort>;
        let sandbox = Arc::new(ArcSwap::from_pointee(
            Arc::new(NoOpSandbox) as Arc<dyn crate::domain::ports::SandboxManager>
        ));
        let tools = Arc::new(ToolSetAdapter::new(
            PathBuf::from("."),
            storage.clone(),
            sandbox,
            Arc::new(tokio::sync::RwLock::new(
                crate::domain::models::SandboxPolicy::Permissive,
            )),
        )) as Arc<dyn crate::domain::ports::ToolSetPort>;
        let approval = ApprovalRuntime::new(1024, Arc::new(NoOpApprovalPersistence));
        let scheduler = ToolScheduler::new(security.clone(), tools.clone(), approval.clone(), 1024);
        let (event_bus, _event_rx) = EventBus::new(1024);
        let event_bus = Arc::new(event_bus);
        let registry = Arc::new(NodeTree::new());
        let parent_sandbox = Arc::new(tokio::sync::RwLock::new(
            crate::domain::models::SandboxPolicy::Permissive,
        ));
        let spool = Arc::new(SubagentSpool::new(tmp.path().join("spool")).await.unwrap());
        let (authority, root_authority) = authority_pair();

        let runner = InProcessSubagentRunner::new(
            provider,
            storage,
            security,
            tools,
            approval,
            scheduler,
            event_bus,
            registry,
            parent_sandbox,
            spool,
            authority,
            root_authority,
        );
        (runner, tmp)
    }

    async fn make_hanging_runner() -> (InProcessSubagentRunner, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let provider = Arc::new(HangingProvider) as Arc<dyn StreamingProvider>;
        let storage = Arc::new(FileSystemStorage::new(tmp.path().to_path_buf()))
            as Arc<dyn crate::domain::ports::StoragePort>;
        let security = Arc::new(SecurityAdapter::new(PathBuf::from(".")))
            as Arc<dyn crate::domain::ports::SecurityPort>;
        let sandbox = Arc::new(ArcSwap::from_pointee(
            Arc::new(NoOpSandbox) as Arc<dyn crate::domain::ports::SandboxManager>
        ));
        let tools = Arc::new(ToolSetAdapter::new(
            PathBuf::from("."),
            storage.clone(),
            sandbox,
            Arc::new(tokio::sync::RwLock::new(
                crate::domain::models::SandboxPolicy::Permissive,
            )),
        )) as Arc<dyn crate::domain::ports::ToolSetPort>;
        let approval = ApprovalRuntime::new(1024, Arc::new(NoOpApprovalPersistence));
        let scheduler = ToolScheduler::new(security.clone(), tools.clone(), approval.clone(), 1024);
        let (event_bus, _event_rx) = EventBus::new(1024);
        let event_bus = Arc::new(event_bus);
        let registry = Arc::new(NodeTree::new());
        let parent_sandbox = Arc::new(tokio::sync::RwLock::new(
            crate::domain::models::SandboxPolicy::Permissive,
        ));
        let spool = Arc::new(SubagentSpool::new(tmp.path().join("spool")).await.unwrap());
        let (authority, root_authority) = authority_pair();

        let runner = InProcessSubagentRunner::new(
            provider,
            storage,
            security,
            tools,
            approval,
            scheduler,
            event_bus,
            registry,
            parent_sandbox,
            spool,
            authority,
            root_authority,
        );
        (runner, tmp)
    }

    async fn make_usage_runner() -> (InProcessSubagentRunner, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let provider = Arc::new(UsageOnceProvider) as Arc<dyn StreamingProvider>;
        let storage = Arc::new(FileSystemStorage::new(tmp.path().to_path_buf()))
            as Arc<dyn crate::domain::ports::StoragePort>;
        let security = Arc::new(SecurityAdapter::new(PathBuf::from(".")))
            as Arc<dyn crate::domain::ports::SecurityPort>;
        let sandbox = Arc::new(ArcSwap::from_pointee(
            Arc::new(NoOpSandbox) as Arc<dyn crate::domain::ports::SandboxManager>
        ));
        let tools = Arc::new(ToolSetAdapter::new(
            PathBuf::from("."),
            storage.clone(),
            sandbox,
            Arc::new(tokio::sync::RwLock::new(
                crate::domain::models::SandboxPolicy::Permissive,
            )),
        )) as Arc<dyn crate::domain::ports::ToolSetPort>;
        let approval = ApprovalRuntime::new(1024, Arc::new(NoOpApprovalPersistence));
        let scheduler = ToolScheduler::new(security.clone(), tools.clone(), approval.clone(), 1024);
        let (event_bus, _event_rx) = EventBus::new(1024);
        let event_bus = Arc::new(event_bus);
        let registry = Arc::new(NodeTree::new());
        let parent_sandbox = Arc::new(tokio::sync::RwLock::new(
            crate::domain::models::SandboxPolicy::Permissive,
        ));
        let spool = Arc::new(SubagentSpool::new(tmp.path().join("spool")).await.unwrap());
        let (authority, root_authority) = authority_pair();

        let runner = InProcessSubagentRunner::new(
            provider,
            storage,
            security,
            tools,
            approval,
            scheduler,
            event_bus,
            registry,
            parent_sandbox,
            spool,
            authority,
            root_authority,
        );
        (runner, tmp)
    }

    #[tokio::test]
    async fn happy_path_launch() {
        let (runner, _tmp) = make_runner().await;
        let spec = AgentLaunchSpec {
            prompt: String::from("hello"),
            effective_model: String::from("test-model"),
            tier: crate::domain::models::ModelTier::CheapAgentic,
            tools_allow: crate::domain::models::ToolPolicy::InheritFromParent,
            parent_ctx_tokens: 0,
            sandbox_override: None,
            parent_trace: None,
        };
        let cancel = CancellationToken::new();
        let handle = runner.launch(spec, cancel.clone()).await.unwrap();
        assert!(!handle.task_id.is_empty());
        // Let it run briefly then kill
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        handle.cancel.cancel();
        // Drain status channel
        let mut statuses = Vec::new();
        let mut rx = handle.status_rx;
        while let Ok(s) = rx.try_recv() {
            statuses.push(s);
        }
        assert!(statuses.iter().any(|s| matches!(s, NodeState::Running)));
    }

    #[tokio::test]
    async fn cancel_kills_child() {
        let (runner, _tmp) = make_runner().await;
        let spec = AgentLaunchSpec {
            prompt: String::from("hello"),
            effective_model: String::from("test-model"),
            tier: crate::domain::models::ModelTier::CheapAgentic,
            tools_allow: crate::domain::models::ToolPolicy::InheritFromParent,
            parent_ctx_tokens: 0,
            sandbox_override: None,
            parent_trace: None,
        };
        let cancel = CancellationToken::new();
        let handle = runner.launch(spec, cancel.clone()).await.unwrap();
        handle.cancel.cancel();
        // Wait for status
        let mut rx = handle.status_rx;
        let status = tokio::time::timeout(tokio::time::Duration::from_millis(500), rx.recv()).await;
        assert!(status.is_ok());
        // Should see Killed or Completed
    }

    // ── 14.4 Fix: Pause/Resume/Kill streaming-loop regression tests ────
    // These replace the old 50ms-snapshot op_pause_sets_atomic / op_resume_clears_atomic
    // tests with deterministic status-sequence assertions through the production
    // run_child select path.

    // T1: Pause during streaming yields Suspended, not Failed
    #[tokio::test]
    async fn t1_pause_during_streaming_yields_suspended_not_failed() {
        let (runner, _tmp) = make_hanging_runner().await;
        let spec = AgentLaunchSpec {
            prompt: String::from("hello"),
            effective_model: String::from("test-model"),
            tier: crate::domain::models::ModelTier::CheapAgentic,
            tools_allow: crate::domain::models::ToolPolicy::InheritFromParent,
            parent_ctx_tokens: 0,
            sandbox_override: None,
            parent_trace: None,
        };
        let cancel = CancellationToken::new();
        let handle = runner.launch(spec, cancel.clone()).await.unwrap();
        let mut status_rx = handle.status_rx;

        // Wait for Running — child is now streaming from HangingProvider
        let saw_running = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(s) = status_rx.recv().await {
                if matches!(s, NodeState::Running) {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        assert!(saw_running, "child never reached Running");

        // Let the child consume the single chunk and park on pending()
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Send Pause — hits the streaming select's command_rx arm
        let _ = handle.command_tx.send(Op::Pause).await;

        // Collect statuses until terminal or timeout
        let mut statuses = Vec::new();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(s) = status_rx.recv().await {
                let is_terminal = matches!(
                    s,
                    NodeState::Completed | NodeState::Failed | NodeState::Cancelled
                );
                statuses.push(s);
                if is_terminal {
                    break;
                }
            }
        })
        .await;

        assert!(
            statuses.contains(&NodeState::Suspended),
            "Pause during streaming must emit Suspended; got: {statuses:?}"
        );
        assert!(
            !statuses.contains(&NodeState::Failed),
            "Pause during streaming must NOT emit Failed; got: {statuses:?}"
        );

        // Child must still be addressable (not deregistered)
        let entries = runner.registry.list().await;
        assert!(
            entries.iter().any(|e| e.agent_id == handle.agent_id),
            "paused child must remain in registry"
        );

        handle.cancel.cancel();
    }

    // T2: Resume after streaming pause re-enters Running
    #[tokio::test]
    async fn t2_resume_after_streaming_pause_reenters_running() {
        let (runner, _tmp) = make_hanging_runner().await;
        let spec = AgentLaunchSpec {
            prompt: String::from("hello"),
            effective_model: String::from("test-model"),
            tier: crate::domain::models::ModelTier::CheapAgentic,
            tools_allow: crate::domain::models::ToolPolicy::InheritFromParent,
            parent_ctx_tokens: 0,
            sandbox_override: None,
            parent_trace: None,
        };
        let cancel = CancellationToken::new();
        let handle = runner.launch(spec, cancel.clone()).await.unwrap();
        let mut status_rx = handle.status_rx;

        // Wait for Running
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(s) = status_rx.recv().await {
                if matches!(s, NodeState::Running) {
                    break;
                }
            }
        })
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Pause
        let _ = handle.command_tx.send(Op::Pause).await;
        let saw_suspended = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(s) = status_rx.recv().await {
                if matches!(s, NodeState::Suspended) {
                    return true;
                }
                if matches!(s, NodeState::Failed) {
                    panic!("Pause produced Failed instead of Suspended");
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        assert!(saw_suspended, "child never reached Suspended");

        // Resume
        let _ = handle.command_tx.send(Op::Resume).await;
        let saw_running_again = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(s) = status_rx.recv().await {
                if matches!(s, NodeState::Running) {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        assert!(
            saw_running_again,
            "child never re-entered Running after Resume"
        );

        handle.cancel.cancel();
    }

    // T3: Deliver while suspended queues until resume
    #[tokio::test]
    async fn t3_deliver_while_suspended_queues_until_resume() {
        let (runner, _tmp) = make_hanging_runner().await;
        let spec = AgentLaunchSpec {
            prompt: String::from("hello"),
            effective_model: String::from("test-model"),
            tier: crate::domain::models::ModelTier::CheapAgentic,
            tools_allow: crate::domain::models::ToolPolicy::InheritFromParent,
            parent_ctx_tokens: 0,
            sandbox_override: None,
            parent_trace: None,
        };
        let cancel = CancellationToken::new();
        let handle = runner.launch(spec, cancel.clone()).await.unwrap();
        let mut status_rx = handle.status_rx;

        // Wait for Running
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(s) = status_rx.recv().await {
                if matches!(s, NodeState::Running) {
                    break;
                }
            }
        })
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Pause
        let _ = handle.command_tx.send(Op::Pause).await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(s) = status_rx.recv().await {
                if matches!(s, NodeState::Suspended) {
                    break;
                }
            }
        })
        .await;

        // Deliver two messages while suspended
        use crate::domain::models::{
            AgentDelivery, AgentMessage, CorrelationId, DeliveryMode, Envelope, MessageHeader,
            MessageKind,
        };
        let make_delivery = |content: &str| {
            let header = MessageHeader {
                sender: AgentId("parent".into()),
                recipient: AgentId("child".into()),
                correlation_id: CorrelationId::new("corr-1"),
                kind: MessageKind::PeerMessage,
                sequence: None,
            };
            AgentDelivery::new(
                Envelope {
                    header,
                    body: AgentMessage::new(content),
                },
                DeliveryMode::Queue,
            )
        };
        let _ = handle
            .command_tx
            .send(Op::Deliver(make_delivery("msg1")))
            .await;
        let _ = handle
            .command_tx
            .send(Op::Deliver(make_delivery("msg2")))
            .await;

        // Resume — messages drain into context
        let _ = handle.command_tx.send(Op::Resume).await;
        let saw_running = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(s) = status_rx.recv().await {
                if matches!(s, NodeState::Running) {
                    return true;
                }
                if matches!(s, NodeState::Failed) {
                    panic!("Resume after queued deliveries produced Failed");
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        assert!(
            saw_running,
            "child never re-entered Running after Resume with queued messages"
        );

        handle.cancel.cancel();
    }

    // T4: Pause then kill yields Cancelled, not Failed
    #[tokio::test]
    async fn t4_pause_then_kill_yields_cancelled_not_failed() {
        let (runner, _tmp) = make_hanging_runner().await;
        let spec = AgentLaunchSpec {
            prompt: String::from("hello"),
            effective_model: String::from("test-model"),
            tier: crate::domain::models::ModelTier::CheapAgentic,
            tools_allow: crate::domain::models::ToolPolicy::InheritFromParent,
            parent_ctx_tokens: 0,
            sandbox_override: None,
            parent_trace: None,
        };
        let cancel = CancellationToken::new();
        let handle = runner.launch(spec, cancel.clone()).await.unwrap();
        let mut status_rx = handle.status_rx;

        // Wait for Running
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(s) = status_rx.recv().await {
                if matches!(s, NodeState::Running) {
                    break;
                }
            }
        })
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Pause
        let _ = handle.command_tx.send(Op::Pause).await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(s) = status_rx.recv().await {
                if matches!(s, NodeState::Suspended) {
                    break;
                }
            }
        })
        .await;

        // Kill while suspended
        let _ = handle.command_tx.send(Op::Kill).await;
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match status_rx.recv().await {
                    Some(s @ (NodeState::Completed | NodeState::Failed | NodeState::Cancelled)) => {
                        break s;
                    }
                    Some(_) => continue,
                    None => break NodeState::Failed,
                }
            }
        })
        .await
        .expect("terminal state within timeout");

        assert_eq!(
            terminal,
            NodeState::Cancelled,
            "Kill while Suspended must yield Cancelled, not Failed"
        );
    }

    // T5: Provider abnormal end (no Pause) still yields Failed — positive control
    #[tokio::test]
    async fn t5_provider_abnormal_end_without_pause_still_fails() {
        struct AbnormalEndProvider;

        #[async_trait::async_trait]
        impl StreamingProvider for AbnormalEndProvider {
            async fn stream_completion(
                &self,
                _messages: Vec<Message>,
                _options: CompletionOptions,
            ) -> Result<BoxStream<'static, StreamChunk>, crate::domain::errors::ProviderError>
            {
                let chunks = vec![StreamChunk::Text {
                    content: "partial...".into(),
                    parent_tool_use_id: None,
                }];
                // Stream ends after one chunk — no TurnComplete
                Ok(Box::pin(futures::stream::iter(chunks)))
            }

            async fn abort(&self) -> Result<(), crate::domain::errors::ProviderError> {
                Ok(())
            }

            fn provider_id(&self) -> String {
                "abnormal-end".into()
            }

            fn list_models(&self) -> Vec<ModelDescriptor> {
                vec![]
            }

            async fn health_check(&self) -> Result<(), crate::domain::errors::ProviderError> {
                Ok(())
            }
            async fn connectivity_probe(
                &self,
            ) -> Result<crate::domain::ports::ProbeOutcome, crate::domain::errors::ProviderError>
            {
                Ok(crate::domain::ports::ProbeOutcome {
                    latency: std::time::Duration::ZERO,
                })
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let provider = Arc::new(AbnormalEndProvider) as Arc<dyn StreamingProvider>;
        let storage = Arc::new(crate::adapters::filesystem::FileSystemStorage::new(
            tmp.path().to_path_buf(),
        )) as Arc<dyn crate::domain::ports::StoragePort>;
        let security = Arc::new(crate::adapters::security_adapter::SecurityAdapter::new(
            PathBuf::from("."),
        )) as Arc<dyn crate::domain::ports::SecurityPort>;
        let sandbox = Arc::new(ArcSwap::from_pointee(
            Arc::new(crate::adapters::sandbox::NoOpSandbox)
                as Arc<dyn crate::domain::ports::SandboxManager>,
        ));
        let tools = Arc::new(crate::adapters::toolset_adapter::ToolSetAdapter::new(
            PathBuf::from("."),
            storage.clone(),
            sandbox,
            Arc::new(tokio::sync::RwLock::new(
                crate::domain::models::SandboxPolicy::Permissive,
            )),
        )) as Arc<dyn crate::domain::ports::ToolSetPort>;
        let approval = crate::domain::services::approval_runtime::ApprovalRuntime::new(
            1024,
            Arc::new(crate::adapters::noop::NoOpApprovalPersistence),
        );
        let scheduler = crate::domain::services::tool_scheduler::ToolScheduler::new(
            security.clone(),
            tools.clone(),
            approval.clone(),
            1024,
        );
        let (event_bus, _event_rx) = crate::infrastructure::runtime::event_bus::EventBus::new(1024);
        let event_bus = Arc::new(event_bus);
        let registry = Arc::new(NodeTree::new());
        let parent_sandbox = Arc::new(tokio::sync::RwLock::new(
            crate::domain::models::SandboxPolicy::Permissive,
        ));
        let spool = Arc::new(SubagentSpool::new(tmp.path().join("spool")).await.unwrap());
        let (authority, root_authority) = authority_pair();

        let runner = InProcessSubagentRunner::new(
            provider,
            storage,
            security,
            tools,
            approval,
            scheduler,
            event_bus,
            registry,
            parent_sandbox,
            spool,
            authority,
            root_authority,
        );

        let spec = AgentLaunchSpec {
            prompt: String::from("hello"),
            effective_model: String::from("test-model"),
            tier: crate::domain::models::ModelTier::CheapAgentic,
            tools_allow: crate::domain::models::ToolPolicy::InheritFromParent,
            parent_ctx_tokens: 0,
            sandbox_override: None,
            parent_trace: None,
        };
        let cancel = CancellationToken::new();
        let handle = runner.launch(spec, cancel).await.unwrap();
        let mut status_rx = handle.status_rx;

        let terminal = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match status_rx.recv().await {
                    Some(s @ (NodeState::Completed | NodeState::Failed | NodeState::Cancelled)) => {
                        break s;
                    }
                    Some(_) => continue,
                    None => break NodeState::Failed,
                }
            }
        })
        .await
        .expect("terminal state within timeout");

        assert_eq!(
            terminal,
            NodeState::Failed,
            "Provider stream ending without TurnComplete (and no Pause) must yield Failed"
        );
    }

    #[tokio::test]
    async fn op_change_model_swaps() {
        let (runner, _tmp) = make_hanging_runner().await;
        let spec = AgentLaunchSpec {
            prompt: String::from("hello"),
            effective_model: String::from("test-model"),
            tier: crate::domain::models::ModelTier::CheapAgentic,
            tools_allow: crate::domain::models::ToolPolicy::InheritFromParent,
            parent_ctx_tokens: 0,
            sandbox_override: None,
            parent_trace: None,
        };
        let cancel = CancellationToken::new();
        let handle = runner.launch(spec, cancel.clone()).await.unwrap();

        let _ = handle.command_tx.send(Op::ChangeModel("opus".into())).await;
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let entries = runner.registry.list().await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].effective_model, "opus");

        handle.cancel.cancel();
    }

    #[tokio::test]
    async fn op_update_tools_swaps() {
        let (runner, _tmp) = make_hanging_runner().await;
        let spec = AgentLaunchSpec {
            prompt: String::from("hello"),
            effective_model: String::from("test-model"),
            tier: crate::domain::models::ModelTier::CheapAgentic,
            tools_allow: crate::domain::models::ToolPolicy::InheritFromParent,
            parent_ctx_tokens: 0,
            sandbox_override: None,
            parent_trace: None,
        };
        let cancel = CancellationToken::new();
        let handle = runner.launch(spec, cancel.clone()).await.unwrap();

        let _ = handle
            .command_tx
            .send(Op::UpdateTools(vec!["bash".into()]))
            .await;
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        let entries = runner.registry.list().await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].tools_summary, "allow: bash");

        handle.cancel.cancel();
    }

    #[tokio::test]
    async fn usage_chunks_persist_spool_meta_and_tokens() {
        let (runner, tmp) = make_usage_runner().await;
        let spec = AgentLaunchSpec {
            prompt: String::from("hello"),
            effective_model: String::from("test-model"),
            tier: crate::domain::models::ModelTier::CheapAgentic,
            tools_allow: crate::domain::models::ToolPolicy::InheritFromParent,
            parent_ctx_tokens: 0,
            sandbox_override: None,
            parent_trace: None,
        };
        let cancel = CancellationToken::new();
        let handle = runner.launch(spec, cancel.clone()).await.unwrap();

        let mut rx = handle.status_rx;
        let terminal = tokio::time::timeout(tokio::time::Duration::from_millis(500), async {
            loop {
                match rx.recv().await {
                    Some(s) if s.is_terminal() => return s,
                    Some(_) => continue,
                    None => return NodeState::Failed,
                }
            }
        })
        .await
        .expect("terminal status within timeout");
        assert_eq!(terminal, NodeState::Completed);

        // The spool persists the meta sidecar async (write-temp + rename,
        // `spool.rs:107`) on the terminal status; under parallel test load the
        // rename can lag the status-rx observation, so a single read races.
        // Poll until the sidecar reflects the terminal `Completed` status
        // (deterministic — no wall-clock flake; a real missing-write bug still
        // fails via the timeout).
        let meta_path = tmp
            .path()
            .join("spool")
            .join(format!("{}.meta", handle.task_id));
        let meta: SpoolMeta =
            tokio::time::timeout(tokio::time::Duration::from_millis(500), async {
                loop {
                    if let Ok(meta_json) = tokio::fs::read_to_string(&meta_path).await {
                        if let Ok(parsed) = serde_json::from_str::<SpoolMeta>(&meta_json) {
                            if parsed.status == NodeState::Completed {
                                return parsed;
                            }
                        }
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("meta sidecar written with Completed status within timeout");
        assert_eq!(meta.tokens_in, 12);
        assert_eq!(meta.tokens_out, 34);
        assert_eq!(meta.subagent_type, "in-process");
    }

    #[tokio::test(start_paused = true)]
    async fn owner_disconnect_retries_then_cancels_owned_node() {
        // Deterministic: virtual time is paused. The child does 3 × 100ms
        // backoff sleeps before self-destructing; we advance the clock in steps
        // and drain every status it emits until it reaches a terminal state —
        // no wall-clock dependency, no CI flake.
        let (runner, _tmp) = make_hanging_runner().await;
        let spec = AgentLaunchSpec {
            prompt: String::from("hello"),
            effective_model: String::from("test-model"),
            tier: crate::domain::models::ModelTier::CheapAgentic,
            tools_allow: crate::domain::models::ToolPolicy::InheritFromParent,
            parent_ctx_tokens: 0,
            sandbox_override: None,
            parent_trace: None,
        };
        let cancel = CancellationToken::new();
        let handle = runner.launch(spec, cancel.clone()).await.unwrap();

        let crate::domain::models::TaskHandle {
            mut status_rx,
            parent_disconnect,
            ..
        } = handle;
        drop(parent_disconnect);

        let mut waiting_seen = false;
        let result = tokio::time::timeout(
            tokio::time::Duration::from_secs(10), // virtual budget; real wall-clock ~0
            async {
                loop {
                    // Park on recv(): with virtual time paused and the child
                    // parked on its 100ms backoff sleep, the runtime auto-
                    // advances to the next timer, wakes the child, and delivers
                    // the status here — no manual advance, no wall-clock sleep,
                    // deterministic under parallel test load.
                    match status_rx.recv().await {
                        Some(NodeState::Waiting) => waiting_seen = true,
                        Some(s) if s.is_terminal() => return s,
                        Some(_) => {}
                        None => panic!("status channel closed without terminal state"),
                    }
                }
            },
        )
        .await
        .expect("abandonment did not reach terminal under virtual time");

        assert!(
            waiting_seen,
            "disconnect retry path must emit Waiting before self-destruct"
        );
        assert_eq!(result, NodeState::Cancelled);
    }

    #[tokio::test]
    async fn cascade_kill_interrupts_actively_streaming_child() {
        // Regression guard for re-review Patch 1 (orphan cancel-token fix).
        // A child parked on `stream.next()` — HangingProvider yields one chunk
        // then hangs on `pending()` — can ONLY be interrupted via its REAL
        // cancellation token; `Op::Kill` is buffered and never selected during
        // streaming. Before the fix, cascade_kill cancelled an orphan token and
        // timed out to `Partial`; after the fix it cancels the child's real
        // token and the cascade succeeds. (The existing cascade tests use fake
        // reactors parked on `cmd_rx`, which never exercise this path — that is
        // exactly the coverage gap this test closes for AC4/AC10.)
        let (runner, _tmp) = make_hanging_runner().await;
        let spec = AgentLaunchSpec {
            prompt: String::from("stream then hang"),
            effective_model: String::from("test-model"),
            tier: crate::domain::models::ModelTier::CheapAgentic,
            tools_allow: crate::domain::models::ToolPolicy::InheritFromParent,
            parent_ctx_tokens: 0,
            sandbox_override: None,
            parent_trace: None,
        };
        let cancel = CancellationToken::new();
        let handle = runner.launch(spec, cancel).await.unwrap();
        let agent_id = handle.agent_id.clone();
        let mut status_rx = handle.status_rx;

        // Wait for Running, then let the child consume the single chunk and
        // park on the hanging `stream.next()` tail.
        let mut saw_running = false;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(s) = status_rx.recv().await {
                if matches!(s, NodeState::Running) {
                    saw_running = true;
                    break;
                }
            }
        })
        .await;
        assert!(saw_running, "child never reached Running");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // cascade_kill must interrupt the streaming child via its real cancel
        // token and reach terminal — NOT time out to Partial.
        let tree = runner.registry();
        let result = tree
            .cascade_kill(&agent_id, std::time::Duration::from_secs(3))
            .await;
        assert!(
            result.is_ok(),
            "cascade_kill of a streaming child must succeed via the real cancel-token interrupt, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn op_report_full_emits_status() {
        let (runner, _tmp) = make_hanging_runner().await;
        let spec = AgentLaunchSpec {
            prompt: String::from("hello"),
            effective_model: String::from("test-model"),
            tier: crate::domain::models::ModelTier::CheapAgentic,
            tools_allow: crate::domain::models::ToolPolicy::InheritFromParent,
            parent_ctx_tokens: 0,
            sandbox_override: None,
            parent_trace: None,
        };
        let cancel = CancellationToken::new();
        let handle = runner.launch(spec, cancel.clone()).await.unwrap();

        // Send ReportFull
        let _ = handle.command_tx.send(Op::ReportFull).await;

        // Should receive a status on status_rx within 50ms
        let mut rx = handle.status_rx;
        let result = tokio::time::timeout(tokio::time::Duration::from_millis(50), rx.recv()).await;
        assert!(result.is_ok());
        let status = result.unwrap();
        assert!(status.is_some());

        handle.cancel.cancel();
    }

    /// AC9 keystone probe: blocks the first stream until the test revokes the
    /// child token, then emits a tool call so `run_child` reaches its authority
    /// gate; ends cleanly on the second stream.
    struct GateProbeProvider {
        fire: std::sync::Arc<tokio::sync::Notify>,
        calls: std::sync::atomic::AtomicU32,
    }

    #[async_trait::async_trait]
    impl StreamingProvider for GateProbeProvider {
        async fn stream_completion(
            &self,
            _messages: Vec<Message>,
            _options: CompletionOptions,
        ) -> Result<BoxStream<'static, StreamChunk>, crate::domain::errors::ProviderError> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let chunks: Vec<StreamChunk> = if n == 0 {
                // First turn: block until the test has revoked the child token,
                // then emit a tool call so run_child reaches its authority gate.
                self.fire.notified().await;
                vec![
                    StreamChunk::ToolUse {
                        id: "probe".into(),
                        name: "probe_tool".into(),
                        input: serde_json::json!({}),
                    },
                    StreamChunk::TurnComplete {
                        stop_reason: crate::domain::models::StopReason::ToolUse,
                    },
                ]
            } else {
                vec![
                    StreamChunk::Text {
                        content: "done".into(),
                        parent_tool_use_id: None,
                    },
                    StreamChunk::TurnComplete {
                        stop_reason: crate::domain::models::StopReason::EndTurn,
                    },
                ]
            };
            Ok(Box::pin(futures::stream::iter(chunks)))
        }
        async fn abort(&self) -> Result<(), crate::domain::errors::ProviderError> {
            Ok(())
        }
        fn provider_id(&self) -> String {
            "gate-probe".into()
        }
        fn list_models(&self) -> Vec<ModelDescriptor> {
            vec![]
        }
        async fn health_check(&self) -> Result<(), crate::domain::errors::ProviderError> {
            Ok(())
        }
        async fn connectivity_probe(
            &self,
        ) -> Result<crate::domain::ports::ProbeOutcome, crate::domain::errors::ProviderError>
        {
            Ok(crate::domain::ports::ProbeOutcome {
                latency: std::time::Duration::ZERO,
            })
        }
    }

    /// AC9 keystone, end-to-end (post-review party-mode closure): a child whose
    /// delegated token is revoked is DENIED its next tool dispatch by the
    /// `run_child` gate. The defeat the review found lived in the *caller*
    /// (validated root, not the child), so a ledger unit test cannot catch it —
    /// this drives the real gate.
    #[tokio::test(flavor = "current_thread")]
    async fn ac9_revoked_child_token_denies_next_tool_dispatch() {
        let tmp = tempfile::tempdir().unwrap();
        let fire = std::sync::Arc::new(tokio::sync::Notify::new());
        let provider = std::sync::Arc::new(GateProbeProvider {
            fire: fire.clone(),
            calls: std::sync::atomic::AtomicU32::new(0),
        }) as std::sync::Arc<dyn StreamingProvider>;

        // Shared ledger (test revokes) + shared spool (test reads the denial).
        let root = crate::domain::models::CapabilityToken::r1_root(AgentId::root());
        let ledger = std::sync::Arc::new(
            crate::domain::services::authority_ledger::AuthorityLedger::new(root.clone()),
        );
        let registry = std::sync::Arc::new(NodeTree::new());
        let authority: std::sync::Arc<dyn AuthorityProvider> = std::sync::Arc::new(
            crate::adapters::authority::InProcessAuthorityProvider::new(ledger.clone()),
        );
        let storage = std::sync::Arc::new(FileSystemStorage::new(tmp.path().to_path_buf()))
            as std::sync::Arc<dyn crate::domain::ports::StoragePort>;
        let security = std::sync::Arc::new(SecurityAdapter::new(PathBuf::from(".")))
            as std::sync::Arc<dyn crate::domain::ports::SecurityPort>;
        let sandbox = std::sync::Arc::new(ArcSwap::from_pointee(std::sync::Arc::new(NoOpSandbox)
            as std::sync::Arc<dyn crate::domain::ports::SandboxManager>));
        let tools = std::sync::Arc::new(ToolSetAdapter::new(
            PathBuf::from("."),
            storage.clone(),
            sandbox,
            std::sync::Arc::new(tokio::sync::RwLock::new(
                crate::domain::models::SandboxPolicy::Permissive,
            )),
        )) as std::sync::Arc<dyn crate::domain::ports::ToolSetPort>;
        let approval = ApprovalRuntime::new(1024, std::sync::Arc::new(NoOpApprovalPersistence));
        let scheduler = ToolScheduler::new(security.clone(), tools.clone(), approval.clone(), 1024);
        let (event_bus, _event_rx) = EventBus::new(1024);
        let event_bus = std::sync::Arc::new(event_bus);
        let parent_sandbox = std::sync::Arc::new(tokio::sync::RwLock::new(
            crate::domain::models::SandboxPolicy::Permissive,
        ));
        let spool =
            std::sync::Arc::new(SubagentSpool::new(tmp.path().join("spool")).await.unwrap());
        let runner = InProcessSubagentRunner::new(
            provider,
            storage,
            security,
            tools,
            approval,
            scheduler,
            event_bus,
            registry.clone(),
            parent_sandbox,
            spool.clone(),
            authority,
            root,
        );

        let spec = AgentLaunchSpec {
            prompt: String::from("probe"),
            effective_model: String::from("test-model"),
            tier: crate::domain::models::ModelTier::CheapAgentic,
            tools_allow: crate::domain::models::ToolPolicy::InheritFromParent,
            parent_ctx_tokens: 0,
            sandbox_override: None,
            parent_trace: None,
        };
        let handle = runner.launch(spec, CancellationToken::new()).await.unwrap();
        let task_id = handle.task_id.clone();

        // Revoke the child's delegated token BEFORE unblocking the provider, so
        // the run_child gate observes a revoked token on its first tool dispatch.
        ledger.revoke_scope(&handle.agent_id).unwrap();
        fire.notify_one();

        // Wait for terminal status.
        let mut rx = handle.status_rx;
        let _ = tokio::time::timeout(tokio::time::Duration::from_millis(2000), async {
            loop {
                match rx.recv().await {
                    Some(s @ (NodeState::Completed | NodeState::Failed | NodeState::Cancelled)) => {
                        break s;
                    }
                    Some(_) => continue,
                    None => break NodeState::Failed,
                }
            }
        })
        .await;

        // The keystone proof: the denied dispatch wrote "Blocked by authority"
        // to the spool, and the tool itself was never dispatched.
        let content = spool.read_full(&task_id).await.unwrap_or_default();
        assert!(
            content.contains("Blocked by authority"),
            "AC9 keystone: a revoked child must be denied its next tool dispatch; spool was: {content}"
        );
    }

    /// AC2 keystone [K] (DF-310): a real in-process Completed child driven through
    /// the **production `InProcessSubagentRunner`** (NOT `FakeRunner`) produces a
    /// non-Empty structured yield. Kill-criterion: reverting `yield_rx: Some(yield_rx)`
    /// back to `yield_rx: None` MUST turn this test RED (the recv returns None).
    #[tokio::test]
    async fn ac2_production_runner_completed_child_yields_non_empty() {
        use crate::infrastructure::orchestrator::SpokeYield;

        // UsageOnceProvider emits: Usage{} → Text{"done"} → TurnComplete{EndTurn}
        // This hits the PRIMARY Completed path (EndTurn/MaxTokens → line 959 in
        // run_child). accumulated_text will be "done".
        let tmp = tempfile::tempdir().unwrap();
        let provider = Arc::new(UsageOnceProvider) as Arc<dyn StreamingProvider>;
        let storage = Arc::new(FileSystemStorage::new(tmp.path().to_path_buf()))
            as Arc<dyn crate::domain::ports::StoragePort>;
        let security = Arc::new(SecurityAdapter::new(PathBuf::from(".")))
            as Arc<dyn crate::domain::ports::SecurityPort>;
        let sandbox = Arc::new(ArcSwap::from_pointee(
            Arc::new(NoOpSandbox) as Arc<dyn crate::domain::ports::SandboxManager>
        ));
        let tools = Arc::new(ToolSetAdapter::new(
            PathBuf::from("."),
            storage.clone(),
            sandbox,
            Arc::new(tokio::sync::RwLock::new(
                crate::domain::models::SandboxPolicy::Permissive,
            )),
        )) as Arc<dyn crate::domain::ports::ToolSetPort>;
        let approval = ApprovalRuntime::new(1024, Arc::new(NoOpApprovalPersistence));
        let scheduler = ToolScheduler::new(security.clone(), tools.clone(), approval.clone(), 1024);
        let (event_bus, _event_rx) = EventBus::new(1024);
        let event_bus = Arc::new(event_bus);
        let registry = Arc::new(NodeTree::new());
        let parent_sandbox = Arc::new(tokio::sync::RwLock::new(
            crate::domain::models::SandboxPolicy::Permissive,
        ));
        let spool = Arc::new(SubagentSpool::new(tmp.path().join("spool")).await.unwrap());
        let (authority, root_authority) = authority_pair();

        let runner = InProcessSubagentRunner::new(
            provider,
            storage,
            security,
            tools,
            approval,
            scheduler,
            event_bus,
            registry,
            parent_sandbox,
            spool,
            authority,
            root_authority,
        );
        let spec = AgentLaunchSpec {
            prompt: "test child".to_string(),
            effective_model: "test-model".to_string(),
            tier: crate::domain::models::ModelTier::Flagship,
            tools_allow: crate::domain::models::ToolPolicy::InheritFromParent,
            parent_ctx_tokens: 0,
            sandbox_override: None,
            parent_trace: None,
        };
        let cancel = CancellationToken::new();
        let mut handle = runner
            .launch(spec, cancel)
            .await
            .expect("launch should succeed");

        // Wait for the child to reach a terminal state.
        let mut status_rx = handle.status_rx;
        let terminal = tokio::time::timeout(tokio::time::Duration::from_secs(5), async {
            loop {
                match status_rx.recv().await {
                    Some(s @ (NodeState::Completed | NodeState::Failed | NodeState::Cancelled)) => {
                        break s;
                    }
                    Some(_) => continue,
                    None => break NodeState::Failed,
                }
            }
        })
        .await
        .expect("child must reach terminal within 5s");

        assert_eq!(
            terminal,
            NodeState::Completed,
            "child must complete (EndTurn)"
        );

        // DF-310 keystone: yield_rx must contain a valid, non-empty SpokeYield.
        // If yield_rx were still None (the old code), this unwrap fails.
        let mut yield_rx = handle
            .yield_rx
            .take()
            .expect("AC2 kill-criterion: yield_rx must be Some — reverting to None breaks this");
        let raw = yield_rx.recv().await.expect(
            "AC2 kill-criterion: yield channel must contain a message — emit_yield was not called",
        );

        let parsed: SpokeYield = serde_json::from_str(&raw).expect("yield must be valid JSON");
        assert!(
            !parsed.summary.is_empty(),
            "AC2: summary must not be empty; got empty from production runner"
        );
        // UsageOnceProvider emits "done" as its text content
        assert!(
            parsed.summary.contains("done"),
            "AC2: summary should contain the child's output text 'done'; got: {}",
            parsed.summary
        );
        assert!(
            !parsed.detail.is_empty(),
            "AC2: detail must not be empty for a Completed child"
        );
    }
}
