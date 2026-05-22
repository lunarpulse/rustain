//! Composite toolset adapter — composes builtin tools with MCP-discovered
//! tools (Story 9.1 + 9.2).
//!
//! `available_tools()` projects MCP `cached_tools` into canonical
//! `mcp__<server>__<tool>` names per ADR-06-08. `execute()` routes
//! `mcp__`-prefixed names to the appropriate `McpClientAdapter::call_tool`.
//!
//! Story 9.3a adds the `CapabilityRegistry` as an internal field (Flag 1)
//! with an `McpProvider` wrapping each `McpClientAdapter` behind the
//! `CapabilityProvider` trait.
//!
//! Story 9.3b adds `BuiltinProvider` + `SkillsProvider` registration,
//! `ToolDescriptor` domain type, full `CatalogDelta` body, and
//! `emit_catalog_delta` Phase A no-op.

use async_trait::async_trait;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Global counter for synthetic tool_use_id generation in the composite adapter.
static COMPOSITE_TOOL_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

use crate::domain::errors::{ToolError, TransitionError};
use crate::domain::events::{AppEvent, ToolProgressEvent};
use crate::domain::models::{
    CheckpointId, HealthSummary, McpConnectionState, McpHealthRow, McpServerSpec, ToolDefinition,
    ToolResult, TransitionState, capability_registry::CapabilityRegistry,
    capability_registry::RegisterHandle, capability_registry::RegistryError,
};
use crate::domain::ports::ToolSetPort;
use crate::domain::services::swap_tier::SwapTier;

/// Composes a builtin `ToolSetAdapter` with zero or more `McpClientAdapter`s.
pub struct CompositeToolsetAdapter {
    builtin: Arc<dyn ToolSetPort>,
    mcp_clients: Vec<Arc<crate::adapters::mcp::client::McpClientAdapter>>,
    server_specs: Vec<McpServerSpec>,
    include_builtin: bool,
    /// Story 9.3a — internal registry of all capabilities (Flag 1).
    /// Stored as `Arc` so `RegisterHandle` Weak refs remain valid across
    /// repopulations (fixes review finding: temporary Arc invalidated Weak refs).
    capability_registry: Arc<CapabilityRegistry>,
    /// Handles keeping discovered capabilities alive.
    subscription_handles: TokioMutex<Vec<RegisterHandle>>,
    /// Story 9.3b — optional skill activator for `SkillsProvider` registration.
    skill_activator: Option<Arc<crate::adapters::skill_activation::SkillActivator>>,
    /// Story 9.3b — previous catalog snapshot for delta computation.
    prev_catalog: TokioMutex<Vec<crate::domain::models::tool_descriptor::ToolDescriptor>>,
    /// Story 9.3b — monotonic version counter for catalog deltas.
    catalog_version: AtomicU64,
}

impl CompositeToolsetAdapter {
    pub fn new(
        builtin: Arc<dyn ToolSetPort>,
        mcp_clients: Vec<Arc<crate::adapters::mcp::client::McpClientAdapter>>,
        server_specs: Vec<McpServerSpec>,
        include_builtin: bool,
        event_tx: Option<mpsc::UnboundedSender<AppEvent>>,
        skill_activator: Option<Arc<crate::adapters::skill_activation::SkillActivator>>,
    ) -> Self {
        let capability_registry = Arc::new(CapabilityRegistry::new(event_tx));
        // P-15: Validate no duplicate server IDs
        let mut seen = std::collections::HashSet::new();
        for client in &mcp_clients {
            let id = client.server_id();
            if !seen.insert(id) {
                tracing::error!(server_id = %id, "Duplicate MCP server ID detected — routing will be nondeterministic");
            }
        }
        Self {
            builtin,
            mcp_clients,
            server_specs,
            include_builtin,
            capability_registry,
            subscription_handles: TokioMutex::new(Vec::new()),
            skill_activator,
            prev_catalog: TokioMutex::new(Vec::new()),
            catalog_version: AtomicU64::new(0),
        }
    }

    /// Story 9.3a — access the capability registry.
    pub fn capability_registry(&self) -> &Arc<CapabilityRegistry> {
        &self.capability_registry
    }

    /// Story 9.3b — read the current catalog version (for tests).
    pub fn catalog_version(&self) -> u64 {
        self.catalog_version.load(Ordering::Relaxed)
    }

    /// Discover and register all capabilities into the registry.
    ///
    /// Called post-construction from startup or after MCP catalog changes.
    /// Idempotent — re-registration of the same id emits
    /// `CapabilityEvent::Updated`, not duplicates.
    ///
    /// Story 9.3b: registers builtin → skills → MCP, then emits catalog delta.
    pub async fn populate_registry(&self) -> Result<(), RegistryError> {
        let mut handles = Vec::new();
        let mut errors: Vec<String> = Vec::new();

        // 1. Builtin (when enabled per Story 9.2 AC-8 [tools.config].include_builtin)
        if self.include_builtin {
            let provider = crate::adapters::builtin::BuiltinProvider::new(self.builtin.clone());
            match self
                .capability_registry
                .discover_and_register_all(&provider, "builtin")
                .await
            {
                Ok(h) => handles.extend(h),
                Err(e) => errors.push(format!("builtin: {e}")),
            }
        }

        // 2. Skills (always — `[skills].disabled` is honored inside SkillRegistry::discover)
        if let Some(skill_activator) = self.skill_activator.as_ref() {
            let provider =
                crate::adapters::skill_provider::SkillsProvider::new(skill_activator.clone());
            match self
                .capability_registry
                .discover_and_register_all(&provider, "skill")
                .await
            {
                Ok(h) => handles.extend(h),
                Err(e) => errors.push(format!("skill: {e}")),
            }
        }

        // 3. MCP (existing 9.3a logic, unchanged structure)
        for client in &self.mcp_clients {
            let provider = crate::adapters::mcp::mcp_provider::McpProvider::new(client.clone());
            let provider_id = format!("mcp:{}", client.server_id());
            match self
                .capability_registry
                .discover_and_register_all(&provider, &provider_id)
                .await
            {
                Ok(h) => handles.extend(h),
                Err(e) => errors.push(format!("mcp:{provider_id}: {e}")),
            }
        }

        // Replace stored handles
        {
            let mut guard = self.subscription_handles.lock().await;
            *guard = handles;
        }

        // Story 9.3b AC-6 — emit delta after all providers registered
        self.emit_catalog_delta().await?;

        if !errors.is_empty() {
            let msg = errors.join("; ");
            tracing::warn!(%msg, "populate_registry partial failures");
            return Err(RegistryError::DiscoverFailed(
                crate::domain::models::capability::CapabilityError::Discover(msg),
            ));
        }

        Ok(())
    }

    /// Story 9.3b AC-9-3b-6 RELAXED Phase A — compute and emit catalog delta.
    ///
    /// Phase A: internal computation only. The delta is computed against
    /// `self.prev_catalog`, the version counter increments, and the new
    /// snapshot replaces `prev_catalog` — but the result is NOT broadcast.
    /// Returns `Ok(())` always.
    ///
    /// Phase B (Story 9.7 — DEFERRED): the broadcast sender + owned-task
    /// debounce wiring lands. The method body changes; the signature
    /// stays stable.
    pub async fn emit_catalog_delta(&self) -> Result<(), RegistryError> {
        let snapshot: Vec<crate::domain::models::tool_descriptor::ToolDescriptor> = self
            .capability_registry
            .snapshot()
            .iter()
            .map(crate::domain::models::tool_descriptor::ToolDescriptor::from)
            .collect();
        let version = self.catalog_version.fetch_add(1, Ordering::Relaxed) + 1;

        let prev_ids: std::collections::BTreeSet<crate::domain::models::tool_descriptor::ToolId> = {
            let prev = self.prev_catalog.lock().await;
            prev.iter().map(|t| t.id.clone()).collect()
        };
        let new_ids: std::collections::BTreeSet<crate::domain::models::tool_descriptor::ToolId> =
            snapshot.iter().map(|t| t.id.clone()).collect();

        let added: Vec<crate::domain::models::tool_descriptor::ToolDescriptor> = snapshot
            .iter()
            .filter(|t| !prev_ids.contains(&t.id))
            .cloned()
            .collect();
        let removed: Vec<crate::domain::models::tool_descriptor::ToolId> =
            prev_ids.difference(&new_ids).cloned().collect();

        let delta = crate::domain::models::CatalogDelta {
            added,
            removed,
            version,
        };
        // Update prev_catalog AFTER computing the delta
        {
            let mut prev = self.prev_catalog.lock().await;
            *prev = snapshot;
        }

        // PHASE A: no broadcast. Trace at debug level so Phase B wiring
        // is visible in logs without changing the method body.
        tracing::debug!(
            added = delta.added.len(),
            removed = delta.removed.len(),
            version = delta.version,
            "Phase A catalog delta computed (no broadcast — Story 9.7 Phase B)"
        );

        Ok(())
    }

    pub fn mcp_clients(&self) -> &[Arc<crate::adapters::mcp::client::McpClientAdapter>] {
        &self.mcp_clients
    }

    pub fn mcp_server_specs(&self) -> &[McpServerSpec] {
        &self.server_specs
    }

    pub fn connected_servers(&self) -> Vec<&str> {
        self.mcp_clients
            .iter()
            .filter(|c| matches!(c.state(), McpConnectionState::Connected { .. }))
            .map(|c| c.server_id())
            .collect()
    }

    pub fn mcp_health_rows(&self) -> Vec<McpHealthRow> {
        self.mcp_clients
            .iter()
            .map(|c| {
                let state = c.state();
                McpHealthRow {
                    server_name: c.server_id().to_string(),
                    transport: format!(
                        "{:?}",
                        self.server_specs
                            .iter()
                            .find(|s| s.id == c.server_id())
                            .map(|s| s.transport)
                            .unwrap_or(crate::domain::models::McpTransport::Stdio)
                    )
                    .to_lowercase(),
                    level: state.health_level(),
                    metric: state.metric(),
                }
            })
            .collect()
    }

    pub fn start_mcp_connections(&self) {
        if self.mcp_clients.is_empty() {
            return;
        }
        let clients = self.mcp_clients.clone();
        tokio::spawn(async move {
            crate::adapters::mcp::lazy_connect::lazy_connect_all(clients).await;
        });
    }

    async fn dispatch_mcp_call(
        &self,
        client: std::sync::Arc<crate::adapters::mcp::client::McpClientAdapter>,
        tool_name: &str,
        input: serde_json::Value,
        cancel: CancellationToken,
        tool_use_id: String,
        _progress_tx: Option<mpsc::UnboundedSender<ToolProgressEvent>>,
    ) -> Result<ToolResult, ToolError> {
        // P-22: progress_tx is accepted but MCP tools don't support progress events yet
        // TODO: Wire progress channel when rmcp supports streaming results
        match client.call_tool(tool_name, input, cancel).await {
            Ok(mut result) => {
                result.tool_use_id = tool_use_id;
                tracing::debug!(
                    server = %client.server_id(),
                    tool = %tool_name,
                    "MCP tool call completed"
                );
                Ok(result)
            }
            Err(e) => {
                tracing::warn!(
                    server = %client.server_id(),
                    tool = %tool_name,
                    error = %e,
                    "MCP tool call failed"
                );
                Err(match &e {
                    crate::adapters::mcp::error::McpError::Cancelled => ToolError::Cancelled,
                    crate::adapters::mcp::error::McpError::Timeout(_) => ToolError::Timeout,
                    _ => ToolError::ExecutionFailed(e.to_string()),
                })
            }
        }
    }
}

#[async_trait]
impl ToolSetPort for CompositeToolsetAdapter {
    fn available_tools(&self) -> Vec<ToolDefinition> {
        let mut out = if self.include_builtin {
            self.builtin.available_tools()
        } else {
            Vec::new()
        };

        for client in &self.mcp_clients {
            let state = client.state();
            if !matches!(
                state,
                McpConnectionState::Connected { .. } | McpConnectionState::Degraded { .. }
            ) {
                continue;
            }
            if let Some(tools) = client.cached_tools() {
                let server_id = client.server_id();
                for tool in &tools {
                    out.push(crate::adapters::mcp::tool_projection::project_tool(
                        server_id, tool,
                    ));
                }
            }
        }

        out
    }

    /// Story 9.3b — domain catalog shape from the internal registry.
    fn describe(&self) -> Vec<crate::domain::models::tool_descriptor::ToolDescriptor> {
        self.capability_registry
            .snapshot()
            .iter()
            .map(crate::domain::models::tool_descriptor::ToolDescriptor::from)
            .collect()
    }

    fn health_snapshot(&self) -> HealthSummary {
        let builtin_health = self.builtin.health_snapshot();

        if self.mcp_clients.is_empty() {
            return builtin_health;
        }

        // Worst level among all MCP clients
        let mcp_levels: Vec<_> = self
            .mcp_clients
            .iter()
            .map(|c| c.state().health_level())
            .collect();

        let worst_mcp_level = mcp_levels
            .iter()
            .max_by_key(|l| match l {
                crate::domain::models::HealthLevel::Error => 3,
                crate::domain::models::HealthLevel::Degraded => 2,
                crate::domain::models::HealthLevel::Healthy => 1,
                crate::domain::models::HealthLevel::Unknown => 0,
            })
            .copied()
            .unwrap_or(crate::domain::models::HealthLevel::Unknown);

        let connected_count = self
            .mcp_clients
            .iter()
            .filter(|c| matches!(c.state(), McpConnectionState::Connected { .. }))
            .count();

        let total_mcp_tools: usize = self
            .mcp_clients
            .iter()
            .filter_map(|c| match c.state() {
                McpConnectionState::Connected { tool_count, .. } => Some(tool_count),
                _ => None,
            })
            .sum();

        let builtin_tool_count = self.builtin.available_tools().len();
        let metric = format!(
            "tools: {builtin_tool_count} builtin + {total_mcp_tools} mcp ({connected_count}/{} connected)",
            self.mcp_clients.len()
        );

        let overall_level = match (builtin_health.level, worst_mcp_level) {
            (crate::domain::models::HealthLevel::Error, _)
            | (_, crate::domain::models::HealthLevel::Error) => {
                crate::domain::models::HealthLevel::Error
            }
            (crate::domain::models::HealthLevel::Degraded, _)
            | (_, crate::domain::models::HealthLevel::Degraded) => {
                crate::domain::models::HealthLevel::Degraded
            }
            (
                crate::domain::models::HealthLevel::Healthy,
                crate::domain::models::HealthLevel::Healthy,
            ) => crate::domain::models::HealthLevel::Healthy,
            _ => crate::domain::models::HealthLevel::Unknown,
        };

        HealthSummary {
            level: overall_level,
            metric,
            suggested_action: None,
        }
    }

    fn swap_tier(&self) -> SwapTier {
        SwapTier::Warm
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn execute(
        &self,
        tool_name: &str,
        input: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        if let Some((server_id, tool_name_inner)) =
            crate::adapters::mcp::tool_projection::parse_mcp_tool_name(tool_name)
        {
            let client = self
                .mcp_clients
                .iter()
                .find(|c| c.server_id() == server_id)
                .cloned()
                .ok_or_else(|| {
                    ToolError::NotFound(format!("MCP server '{server_id}' not in active profile"))
                })?;
            let state = client.state();
            if !matches!(
                state,
                McpConnectionState::Connected { .. } | McpConnectionState::Degraded { .. }
            ) {
                return Err(ToolError::NotFound(format!(
                    "MCP server '{server_id}' is not connected (state: {state:?})"
                )));
            }
            let seq = COMPOSITE_TOOL_ID_COUNTER.fetch_add(1, Ordering::SeqCst);
            let synthetic_id = format!("mcp-{}-{}", chrono::Utc::now().timestamp_millis(), seq);
            return self
                .dispatch_mcp_call(client, tool_name_inner, input, cancel, synthetic_id, None)
                .await;
        }
        self.builtin.execute(tool_name, input, cancel).await
    }

    async fn execute_with_id(
        &self,
        tool_name: &str,
        tool_use_id: &str,
        input: serde_json::Value,
        cancel: CancellationToken,
        progress_tx: Option<mpsc::UnboundedSender<ToolProgressEvent>>,
    ) -> Result<ToolResult, ToolError> {
        if let Some((server_id, tool_name_inner)) =
            crate::adapters::mcp::tool_projection::parse_mcp_tool_name(tool_name)
        {
            let client = self
                .mcp_clients
                .iter()
                .find(|c| c.server_id() == server_id)
                .cloned()
                .ok_or_else(|| {
                    ToolError::NotFound(format!("MCP server '{server_id}' not in active profile"))
                })?;
            return self
                .dispatch_mcp_call(
                    client,
                    tool_name_inner,
                    input,
                    cancel,
                    tool_use_id.to_string(),
                    progress_tx,
                )
                .await;
        }
        self.builtin
            .execute_with_id(tool_name, tool_use_id, input, cancel, progress_tx)
            .await
    }

    async fn prepare_detach(&self) -> Result<TransitionState, TransitionError> {
        tracing::info!(
            server_count = self.mcp_clients.len(),
            "CompositeToolsetAdapter: preparing detach"
        );

        // Capture current server specs and states
        let state_map: std::collections::BTreeMap<String, McpConnectionState> = self
            .mcp_clients
            .iter()
            .map(|c| (c.server_id().to_string(), c.state()))
            .collect();

        let payload = serde_json::json!({
            "server_specs": &self.server_specs,
            "states": state_map,
        });

        // Shutdown non-persistent servers; leave persistent servers running
        for client in &self.mcp_clients {
            let is_persistent = self
                .server_specs
                .iter()
                .any(|s| s.id == client.server_id() && s.persistent);
            if !is_persistent {
                if let Err(e) = client.disconnect().await {
                    tracing::warn!(server = %client.server_id(), error = %e, "Failed to disconnect non-persistent MCP server during detach");
                }
            }
        }

        Ok(TransitionState {
            port_type: "tools",
            adapter_id: "composite".into(),
            version: 1,
            data: payload,
        })
    }

    async fn receive_state(&self, state: TransitionState) -> Result<(), TransitionError> {
        tracing::info!(
            server_count = self.mcp_clients.len(),
            "CompositeToolsetAdapter: receiving state"
        );

        let prev_specs: Vec<McpServerSpec> = state
            .data
            .get("server_specs")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        let _prev_states: std::collections::BTreeMap<String, McpConnectionState> = state
            .data
            .get("states")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        let _new_ids: std::collections::HashSet<&str> =
            self.mcp_clients.iter().map(|c| c.server_id()).collect();
        let prev_ids: std::collections::HashSet<&str> =
            prev_specs.iter().map(|s| s.id.as_str()).collect();

        // AC-9 3-way diff:
        // same-id → already connected (same spec) or needs reconnect (different spec)
        // old-only → already handled by prepare_detach (non-persistent shut down, persistent in pool)
        // new-only → connect eagerly
        for client in &self.mcp_clients {
            let id = client.server_id();
            if prev_ids.contains(id) {
                // Same id — check if spec changed
                let prev_spec = prev_specs.iter().find(|s| s.id == id);
                let current_spec = self.server_specs.iter().find(|s| s.id == id);
                let spec_changed = match (prev_spec, current_spec) {
                    (Some(p), Some(c)) => p != c,
                    _ => false,
                };

                if spec_changed {
                    tracing::info!(server = %id, "Spec changed — reconnecting MCP server");
                    let _ = client.disconnect().await;
                    let _ = client.connect().await;
                }
                // Same spec — server may be in warm pool or already connected via persistent flag
                // If not connected, attempt connection
                if !matches!(
                    client.state(),
                    McpConnectionState::Connected { .. } | McpConnectionState::Degraded { .. }
                ) {
                    let _ = client.connect().await;
                }
            } else {
                // New only — fully connect
                tracing::info!(server = %id, "Connecting new MCP server after profile switch");
                let _ = client.connect().await;
            }
        }

        Ok(())
    }

    async fn post_transition_verify(&self) -> Result<(), TransitionError> {
        for client in &self.mcp_clients {
            let state = client.state();
            match &state {
                McpConnectionState::Connected { .. }
                | McpConnectionState::Connecting { .. }
                | McpConnectionState::Reconnecting { .. }
                | McpConnectionState::Degraded { .. } => {
                    // Acceptable states after transition
                }
                McpConnectionState::ConnectionFailed { last_error, .. } => {
                    tracing::warn!(
                        server = %client.server_id(),
                        error = %last_error,
                        "MCP server did not reconnect after profile switch — autocomplete will exclude its tools"
                    );
                }
                McpConnectionState::Unsupported { reason } => {
                    tracing::warn!(
                        server = %client.server_id(),
                        reason = %reason,
                        "Unsupported MCP server after profile switch"
                    );
                }
                McpConnectionState::NotConnected => {
                    // Not yet connected after lazy connect — acceptable
                }
            }
        }
        Ok(())
    }

    async fn set_execution_context(
        &self,
        conversation_id: String,
        checkpoint: CheckpointId,
        activation_depth: u8,
    ) {
        self.builtin
            .set_execution_context(conversation_id, checkpoint, activation_depth)
            .await;
    }
}
