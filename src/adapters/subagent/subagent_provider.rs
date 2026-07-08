use async_trait::async_trait;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::domain::models::{
    AgentId, AgentLaunchSpec, Capability, CapabilityError, CapabilityFlag, CapabilityId,
    CapabilityToken, ProviderCapabilities, ToolResult, TraceContext, TransportKind,
};
use crate::domain::ports::{AuthorityProvider, CapabilityProvider};
use crate::domain::services::launch_spec_builder::LaunchSpecBuilder;

/// Story 10.7 — per-turn context for subagent dispatch.
#[derive(Clone, Debug)]
pub struct TaskToolContext {
    pub conversation_id: String,
    pub parent_ctx_tokens: u32,
    pub parent_trace: Option<TraceContext>,
}

pub struct SubagentProvider {
    runner: Arc<dyn crate::domain::ports::SubagentRunner>,
    registry: Arc<crate::infrastructure::subagent::NodeTree>,
    agent_registry: Arc<tokio::sync::RwLock<crate::adapters::agent_registry::AgentRegistry>>,
    model_router: Arc<dyn crate::domain::ports::ProviderInfoPort>,
    spool: Arc<crate::infrastructure::subagent::SubagentSpool>,
    /// Story 10.7 — track running tasks by user-provided task_id for resume semantics.
    running_tasks: Arc<tokio::sync::RwLock<std::collections::HashMap<String, RunningTask>>>,
    /// Story 10.7 — optional usage ledger for per-call TokenUsage recording (AC-10-7-12).
    ledger: Arc<tokio::sync::RwLock<Option<Arc<dyn crate::domain::ports::UsageLedgerPort>>>>,
    authority: Arc<tokio::sync::RwLock<Option<(Arc<dyn AuthorityProvider>, CapabilityToken)>>>,
}

struct RunningTask {
    agent_id: crate::domain::models::AgentId,
    generated_task_id: String,
}

impl SubagentProvider {
    pub fn registry(&self) -> &Arc<crate::infrastructure::subagent::NodeTree> {
        &self.registry
    }

    pub fn spool(&self) -> &Arc<crate::infrastructure::subagent::SubagentSpool> {
        &self.spool
    }

    pub fn runner(&self) -> &Arc<dyn crate::domain::ports::SubagentRunner> {
        &self.runner
    }

    pub fn agent_registry(
        &self,
    ) -> &Arc<tokio::sync::RwLock<crate::adapters::agent_registry::AgentRegistry>> {
        &self.agent_registry
    }

    pub fn new(
        runner: Arc<dyn crate::domain::ports::SubagentRunner>,
        registry: Arc<crate::infrastructure::subagent::NodeTree>,
        agent_registry: Arc<tokio::sync::RwLock<crate::adapters::agent_registry::AgentRegistry>>,
        model_router: Arc<dyn crate::domain::ports::ProviderInfoPort>,
        spool: Arc<crate::infrastructure::subagent::SubagentSpool>,
    ) -> Self {
        Self {
            runner,
            registry,
            agent_registry,
            model_router,
            spool,
            running_tasks: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            ledger: Arc::new(tokio::sync::RwLock::new(None)),
            authority: Arc::new(tokio::sync::RwLock::new(None)),
        }
    }

    /// Story 10.7 — wire the usage ledger for per-call TokenUsage recording (AC-10-7-12).
    pub async fn set_ledger(&self, ledger: Arc<dyn crate::domain::ports::UsageLedgerPort>) {
        *self.ledger.write().await = Some(ledger);
    }

    pub async fn set_authority(
        &self,
        authority: Arc<dyn AuthorityProvider>,
        root_authority: CapabilityToken,
    ) {
        *self.authority.write().await = Some((authority, root_authority));
    }
}

#[async_trait]
impl CapabilityProvider for SubagentProvider {
    fn protocol(&self) -> &str {
        "subagent"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_streaming: false,
            supports_list_changed: true,
            supports_native_retrieval: None,
            max_tool_count: Some(10),
            transport_kind: TransportKind::InProcess,
        }
    }

    async fn discover(&self) -> Result<Vec<Capability>, CapabilityError> {
        // Story 10.7 — advertise the generic `task` tool + `read_task_output` escape hatch.
        let mut caps = vec![
            Capability {
                id: CapabilityId {
                    protocol: "subagent".into(),
                    server: String::new(),
                    tool: "task".into(),
                },
                name: "task".into(),
                description: "Dispatch an isolated subagent task and return the final result text (bounded 8 KB tail).".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "description": { "type": "string", "description": "Short description of the task" },
                        "prompt": { "type": "string", "description": "Full prompt to send to the subagent" },
                        "subagent_type": { "type": "string", "description": "Agent definition name (from .claude/agents/); omit for default worker" },
                        "task_id": { "type": "string", "description": "Optional session id for resuming a prior task" },
                        "tier_hint": { "type": "string", "description": "Optional model tier hint (e.g. 'cheap', 'flagship')" }
                    },
                    "required": ["description", "prompt"]
                }),
                parallel_safe: true,
            },
            Capability {
                id: CapabilityId {
                    protocol: "subagent".into(),
                    server: String::new(),
                    tool: "read_task_output".into(),
                },
                name: "read_task_output".into(),
                description: "Read a byte range from a task's spool file. Use when the 8 KB tail was truncated.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task_id": { "type": "string", "description": "Task id returned by the task tool" },
                        "range": { "type": "string", "description": "Byte range in the form 'start-end' or 'start-' (optional, defaults to full file)" }
                    },
                    "required": ["task_id"]
                }),
                parallel_safe: true,
            },
        ];

        // Also advertise discovered custom agents from AgentRegistry.
        let guard = self.agent_registry.read().await;
        let agents = guard.agents();
        for agent in agents.iter().cloned() {
            caps.push(Capability {
                id: CapabilityId {
                    protocol: "subagent".into(),
                    server: String::new(),
                    tool: agent.name.clone(),
                },
                name: agent.name,
                description: agent.description,
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "prompt": { "type": "string" }
                    },
                    "required": ["prompt"]
                }),
                parallel_safe: false,
            });
        }

        Ok(caps)
    }

    async fn invoke(
        &self,
        id: &CapabilityId,
        input: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<ToolResult, CapabilityError> {
        match id.tool.as_str() {
            "task" => self.invoke_task(input, cancel).await,
            "read_task_output" => self.invoke_read_task_output(input, cancel).await,
            // Legacy per-agent capabilities (discovered agents)
            _ => self.invoke_legacy_agent(id, input, cancel).await,
        }
    }
}

impl SubagentProvider {
    async fn invoke_task(
        &self,
        mut input: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<ToolResult, CapabilityError> {
        if let Err(err) = self
            .validate_authority(CapabilityFlag::Spawn, &AgentId::root())
            .await
        {
            return Ok(ToolResult {
                tool_use_id: String::new(),
                content: format!("Subagent launch rejected by authority: {err}"),
                is_error: true,
            });
        }
        // D1 fix: extract per-turn context from input JSON (injected by CompositeToolsetAdapter)
        let parent_ctx_tokens = input
            .get("__parent_ctx_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let parent_trace: Option<TraceContext> = input
            .get("__parent_trace")
            .and_then(|v| serde_json::from_value(v.clone()).ok());
        let conversation_id = input
            .get("__conversation_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // Remove internal fields before they pollute the tool payload
        let _ = input.as_object_mut().map(|m| {
            m.remove("__parent_ctx_tokens");
            m.remove("__parent_trace");
            m.remove("__conversation_id");
        });

        // 1. Parse input
        let description = input
            .get("description")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                CapabilityError::InvocationFailed("subagent".into(), "Missing 'description'".into())
            })?;
        let prompt = input
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                CapabilityError::InvocationFailed("subagent".into(), "Missing 'prompt'".into())
            })?;
        let subagent_type = input.get("subagent_type").and_then(|v| v.as_str());
        let task_id_opt = input
            .get("task_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let tier_hint = input.get("tier_hint").and_then(|v| v.as_str());
        let _ = description;

        // 2. Resolve agent definition
        let agent_def = if let Some(stype) = subagent_type {
            let guard = self.agent_registry.read().await;
            guard.find(stype).cloned().unwrap_or_else(|| {
                tracing::warn!(subagent_type = %stype, "Agent definition not found — falling back to default worker");
                crate::domain::models::agent::AgentDef::default_worker()
            })
        } else {
            crate::domain::models::agent::AgentDef::default_worker()
        };

        // 3. Resolve model via tier router (Story 7.1c)
        let (effective_model, tier) = resolve_model(self.model_router.as_ref(), tier_hint).await;

        // 4. Budget guard (P3 fix: uses per-invocation context, not shared state)
        if parent_ctx_tokens > 0 {
            let context_window = self
                .model_router
                .get_model(&self.model_router.active_delegate_id(), &effective_model)
                .map(|m| m.context_window)
                .unwrap_or(200_000);
            let parent_remaining = context_window.saturating_sub(parent_ctx_tokens);
            if parent_ctx_tokens > parent_remaining / 2 {
                return Ok(ToolResult {
                    tool_use_id: String::new(),
                    content: format!(
                        "Subagent launch rejected: parent context ({} tokens) exceeds 50% of remaining budget ({} tokens).",
                        parent_ctx_tokens, parent_remaining
                    ),
                    is_error: true,
                });
            }
        }

        // 5. Build launch spec
        let spec = LaunchSpecBuilder::from_task_tool(
            prompt,
            &agent_def,
            &effective_model,
            tier,
            parent_ctx_tokens,
            parent_trace,
        );

        // 6. Resume by task_id if applicable (P2 fix: cleanup on resume; P7 fix: key by user task_id)
        if let Some(ref task_id) = task_id_opt {
            let running_guard = self.running_tasks.read().await;
            if let Some(running) = running_guard.get(task_id) {
                let agent_id = running.agent_id.clone();
                let generated_task_id = running.generated_task_id.clone();
                drop(running_guard);

                let mut rx = match self.registry.status_rx(&agent_id).await {
                    Some(rx) => rx,
                    None => {
                        // P2 fix: clean up stale entry
                        self.running_tasks.write().await.remove(task_id);
                        return Ok(ToolResult {
                            tool_use_id: String::new(),
                            content: format!(
                                "Task '{}' is tracked but no longer in registry — it may have been evicted",
                                task_id
                            ),
                            is_error: true,
                        });
                    }
                };

                let last_status = loop {
                    if rx.changed().await.is_err() {
                        break crate::domain::models::NodeState::Failed;
                    }
                    let status = *rx.borrow();
                    if matches!(
                        status,
                        crate::domain::models::NodeState::Completed
                            | crate::domain::models::NodeState::Failed
                            | crate::domain::models::NodeState::Cancelled
                    ) {
                        break status;
                    }
                };

                let is_error = !matches!(last_status, crate::domain::models::NodeState::Completed);
                let content = match self.spool.read_tail(&generated_task_id, 8192).await {
                    Ok(text) if !text.is_empty() => text,
                    Ok(_) => {
                        if is_error {
                            format!("Subagent terminated with status: {:?}", last_status)
                        } else {
                            "(subagent completed with no output)".into()
                        }
                    }
                    Err(e) => {
                        tracing::warn!(task_id = %generated_task_id, error = %e, "Failed to read spool tail during resume");
                        if is_error {
                            format!("Subagent failed and spool tail unreadable: {}", e)
                        } else {
                            format!("Subagent completed but spool tail unreadable: {}", e)
                        }
                    }
                };

                // P2 fix: clean up running task entry on resume completion
                self.running_tasks.write().await.remove(task_id);

                // D2: check for structured JSON bypass
                let final_content =
                    apply_structured_json_bypass(&content, &generated_task_id, &self.spool).await;

                return Ok(ToolResult {
                    tool_use_id: String::new(),
                    content: final_content,
                    is_error,
                });
            }
        }

        // 7. Launch
        let handle = match self.runner.launch(spec, cancel.clone()).await {
            Ok(h) => h,
            Err(e) => {
                return Ok(map_subagent_error(e));
            }
        };

        // Track running task keyed by user-provided task_id if available (P7 fix)
        {
            let mut running_guard = self.running_tasks.write().await;
            let key = task_id_opt.unwrap_or_else(|| handle.task_id.clone());
            running_guard.insert(
                key,
                RunningTask {
                    agent_id: handle.agent_id.clone(),
                    generated_task_id: handle.task_id.clone(),
                },
            );
        }

        // 8. Await terminal status
        let mut rx = handle.status_rx;
        let mut last_status;
        loop {
            match rx.recv().await {
                Some(s @ crate::domain::models::NodeState::Completed)
                | Some(s @ crate::domain::models::NodeState::Failed)
                | Some(s @ crate::domain::models::NodeState::Cancelled) => {
                    last_status = Some(s);
                    break;
                }
                Some(_) => continue,
                None => {
                    return Ok(ToolResult {
                        tool_use_id: String::new(),
                        content: "Subagent channel closed unexpectedly".into(),
                        is_error: true,
                    });
                }
            }
        }

        while let Ok(s) = rx.try_recv() {
            last_status = Some(s);
        }

        let is_error = matches!(
            last_status,
            Some(crate::domain::models::NodeState::Failed)
                | Some(crate::domain::models::NodeState::Cancelled)
        );

        // 9. Read spool tail
        let content = match self.spool.read_tail(&handle.task_id, 8192).await {
            Ok(text) if !text.is_empty() => text,
            Ok(_) => {
                if is_error {
                    format!(
                        "Subagent terminated with status: {:?}",
                        last_status.unwrap_or(crate::domain::models::NodeState::Failed)
                    )
                } else {
                    "(subagent completed with no output)".into()
                }
            }
            Err(e) => {
                tracing::warn!(task_id = %handle.task_id, error = %e, "Failed to read spool tail");
                if is_error {
                    format!("Subagent failed and spool tail unreadable: {}", e)
                } else {
                    format!("Subagent completed but spool tail unreadable: {}", e)
                }
            }
        };

        // Clean up running task tracking
        {
            let mut running_guard = self.running_tasks.write().await;
            running_guard.retain(|_, v| v.generated_task_id != handle.task_id);
        }

        // D2: structured JSON bypass — if the spool tail contains structured JSON,
        // return a pointer instead of the truncated tail
        let final_content =
            apply_structured_json_bypass(&content, &handle.task_id, &self.spool).await;

        // Record per-call TokenUsage to ledger (P3 fix: uses conversation_id from input)
        {
            let ledger_guard = self.ledger.read().await;
            if let Some(ref ledger) = *ledger_guard {
                if !conversation_id.is_empty() {
                    let tokens_in = prompt.len() as u32 / 4;
                    let tokens_out = content.len() as u32 / 4;
                    let entry = crate::domain::models::usage::UsageLedgerEntry {
                        timestamp_ms: chrono::Utc::now().timestamp_millis(),
                        session_id: conversation_id.clone(),
                        conversation_id: conversation_id.clone(),
                        provider_id: "subagent".into(),
                        model: effective_model.clone(),
                        tier,
                        step_kind: Some(crate::domain::models::StepKind::Plan),
                        escalation_reason: crate::domain::models::router::EscalationReason::None,
                        usage: crate::domain::models::usage::TokenUsage {
                            tokens_in,
                            tokens_out,
                            parent_ctx: parent_ctx_tokens,
                            cache_creation_tokens: None,
                            cache_read_tokens: None,
                            reasoning_tokens: None,
                        },
                    };
                    let _ = ledger.append(entry).await;
                }
            }
        }

        Ok(ToolResult {
            tool_use_id: String::new(),
            content: final_content,
            is_error,
        })
    }

    async fn invoke_read_task_output(
        &self,
        input: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<ToolResult, CapabilityError> {
        let task_id = input
            .get("task_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                CapabilityError::InvocationFailed("subagent".into(), "Missing 'task_id'".into())
            })?;
        if let Err(err) = self
            .validate_authority(CapabilityFlag::ReadFs, &AgentId::root())
            .await
        {
            return Ok(ToolResult {
                tool_use_id: String::new(),
                content: format!("Task output read rejected by authority: {err}"),
                is_error: true,
            });
        }

        let range = input.get("range").and_then(|v| v.as_str());

        // P11 fix: distinguish not-found from empty by checking file existence first
        let content = if let Some(range_str) = range {
            let (offset, len) = parse_byte_range(range_str)?;
            if cancel.is_cancelled() {
                return Ok(ToolResult {
                    tool_use_id: String::new(),
                    content: "Read cancelled".into(),
                    is_error: true,
                });
            }
            match self.spool.pread(task_id, offset, len).await {
                Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(ToolResult {
                        tool_use_id: String::new(),
                        content: format!("Task '{}' not found (no spool file)", task_id),
                        is_error: true,
                    });
                }
                Err(e) => {
                    return Ok(ToolResult {
                        tool_use_id: String::new(),
                        content: format!("Failed to read task output: {}", e),
                        is_error: true,
                    });
                }
            }
        } else {
            if cancel.is_cancelled() {
                return Ok(ToolResult {
                    tool_use_id: String::new(),
                    content: "Read cancelled".into(),
                    is_error: true,
                });
            }
            match self.spool.read_full(task_id).await {
                Ok(text) => text,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(ToolResult {
                        tool_use_id: String::new(),
                        content: format!("Task '{}' not found (no spool file)", task_id),
                        is_error: true,
                    });
                }
                Err(e) => {
                    return Ok(ToolResult {
                        tool_use_id: String::new(),
                        content: format!("Failed to read task output: {}", e),
                        is_error: true,
                    });
                }
            }
        };

        Ok(ToolResult {
            tool_use_id: String::new(),
            content,
            is_error: false,
        })
    }

    async fn validate_authority(
        &self,
        want: CapabilityFlag,
        scope: &AgentId,
    ) -> Result<(), crate::domain::ports::AuthorityError> {
        let authority = self.authority.read().await.clone();
        if let Some((authority, token)) = authority {
            authority.validate(&token, &want, scope).await
        } else {
            // Fail-closed (P6): admitting when unconfigured is a security hole.
            // Production always binds authority via set_authority at startup.
            Err(crate::domain::ports::AuthorityError::Denied { flag: want })
        }
    }

    async fn invoke_legacy_agent(
        &self,
        id: &CapabilityId,
        input: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<ToolResult, CapabilityError> {
        // Legacy per-agent dispatch: treat as a task with subagent_type = agent name
        let mut task_input = input;
        if task_input.get("subagent_type").is_none() {
            task_input["subagent_type"] = serde_json::json!(id.tool);
        }
        // Legacy agent schemas advertise only `prompt` (see discover(), ~:143-160);
        // supply a default `description` so the shared `invoke_task` validator
        // passes. The agent name is a faithful short label for the task.
        if task_input.get("description").is_none() {
            task_input["description"] = serde_json::json!(id.tool);
        }
        self.invoke_task(task_input, cancel).await
    }
}

/// Story 10.7 — resolve effective model and tier via the Story 7.1c router.
/// P4 fix: filter candidate models by tier-appropriate naming conventions.
async fn resolve_model(
    router: &dyn crate::domain::ports::ProviderInfoPort,
    tier_hint: Option<&str>,
) -> (String, crate::domain::models::ModelTier) {
    let target_tier = match tier_hint {
        Some("cheap" | "CheapAgentic") => crate::domain::models::ModelTier::CheapAgentic,
        Some("flagship" | "Flagship") => crate::domain::models::ModelTier::Flagship,
        _ => crate::domain::models::ModelTier::CheapAgentic,
    };

    let active_provider = router.active_delegate_id();
    let models = router.list_models_by_provider(&active_provider);

    let is_tier_match = |model_id: &str| -> bool {
        let lower = model_id.to_lowercase();
        match target_tier {
            crate::domain::models::ModelTier::Flagship => {
                lower.contains("opus") || lower.contains("flagship") || lower.contains("gpt-4")
            }
            crate::domain::models::ModelTier::CheapAgentic => {
                lower.contains("sonnet")
                    || lower.contains("haiku")
                    || lower.contains("cheap")
                    || lower.contains("mini")
                    || lower.contains("flash")
            }
        }
    };

    if !models.is_empty() {
        if let Some(matching) = models.iter().find(|m| is_tier_match(&m.model_id)) {
            return (matching.model_id.clone(), target_tier);
        }
        return (models[0].model_id.clone(), target_tier);
    }

    for provider in router.list_providers() {
        let provider_models = router.list_models_by_provider(&provider.provider_id);
        if let Some(matching) = provider_models.iter().find(|m| is_tier_match(&m.model_id)) {
            return (matching.model_id.clone(), target_tier);
        }
        if let Some(first) = provider_models.first() {
            return (first.model_id.clone(), target_tier);
        }
    }

    let default_model = "claude-sonnet-4-20250514".to_string();
    (default_model, target_tier)
}

/// Map SubagentError variants to an actionable ToolResult.
fn map_subagent_error(e: crate::domain::models::SubagentError) -> ToolResult {
    let content = match e {
        crate::domain::models::SubagentError::SpawnLimitExceeded {
            kind,
            limit,
            attempted,
        } => {
            format!(
                "Spawn limit exceeded: {:?} limit={}, attempted={}",
                kind, limit, attempted
            )
        }
        crate::domain::models::SubagentError::PolicyWidensParent { .. } => {
            "Subagent sandbox policy would widen parent policy".into()
        }
        crate::domain::models::SubagentError::ParentContextBudgetExceeded { .. } => {
            "Parent context budget exceeded".into()
        }
        crate::domain::models::SubagentError::Panicked(ref msg) => {
            format!("Subagent panicked: {}", msg)
        }
        crate::domain::models::SubagentError::Cancelled => "Subagent cancelled".into(),
        crate::domain::models::SubagentError::Internal(ref msg) => {
            format!("Subagent internal error: {}", msg)
        }
    };
    ToolResult {
        tool_use_id: String::new(),
        content,
        is_error: true,
    }
}

/// D2 (AC-10-7-10): Detect structured JSON in the spool tail and return a pointer
/// instead of the truncated content when structured JSON is detected.
async fn apply_structured_json_bypass(
    tail_content: &str,
    task_id: &str,
    spool: &crate::infrastructure::subagent::SubagentSpool,
) -> String {
    let trimmed = tail_content.trim();
    if !trimmed.starts_with('{') && !trimmed.starts_with('[') {
        return tail_content.to_string();
    }
    if serde_json::from_str::<serde_json::Value>(trimmed).is_err() {
        return tail_content.to_string();
    }
    let full_size = match spool.read_full(task_id).await {
        Ok(full) => full.len(),
        Err(_) => return tail_content.to_string(),
    };
    if full_size > 8192 {
        format!(
            "📄 Structured output ({} bytes) — use read_task_output(task_id='{}') to retrieve full payload.",
            full_size, task_id
        )
    } else {
        tail_content.to_string()
    }
}

/// P8 fix: Parse a byte range string using HTTP byte-range semantics.
/// "0-1024" means bytes at offsets 0 through 1024 inclusive (1025 bytes).
/// "1024-" means from offset 1024 to end (defaults to 64 KB).
fn parse_byte_range(s: &str) -> Result<(u64, usize), CapabilityError> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 2 {
        return Err(CapabilityError::InvocationFailed(
            "subagent".into(),
            format!(
                "Invalid range format: '{}' (expected 'start-end' or 'start-')",
                s
            ),
        ));
    }
    let start: u64 = parts[0].parse().map_err(|_| {
        CapabilityError::InvocationFailed(
            "subagent".into(),
            format!("Invalid range start: '{}'", parts[0]),
        )
    })?;
    let len: usize = if parts[1].is_empty() {
        64 * 1024
    } else {
        let end: u64 = parts[1].parse().map_err(|_| {
            CapabilityError::InvocationFailed(
                "subagent".into(),
                format!("Invalid range end: '{}'", parts[1]),
            )
        })?;
        if end < start {
            return Err(CapabilityError::InvocationFailed(
                "subagent".into(),
                format!("Range end ({}) must be >= start ({})", end, start),
            ));
        }
        (end - start + 1) as usize
    };
    Ok((start, len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_byte_range_closed() {
        let (offset, len) = parse_byte_range("0-1024").unwrap();
        assert_eq!(offset, 0);
        assert_eq!(len, 1025); // P8: 0-1024 inclusive = 1025 bytes
    }

    #[test]
    fn parse_byte_range_open() {
        let (offset, len) = parse_byte_range("1024-").unwrap();
        assert_eq!(offset, 1024);
        assert_eq!(len, 64 * 1024);
    }

    #[test]
    fn parse_byte_range_invalid() {
        assert!(parse_byte_range("abc").is_err());
    }

    #[test]
    fn parse_byte_range_end_before_start() {
        assert!(parse_byte_range("100-50").is_err());
    }
}
