//! Composite toolset adapter — delegates to builtin adapter while owning
//! MCP client lifecycle (Story 9.1).
//!
//! `available_tools()` and `execute()` are delegated to `builtin` for this
//! story. Story 9.2 will extend them to include MCP-discovered tools.

use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::domain::errors::{ToolError, TransitionError};
use crate::domain::events::ToolProgressEvent;
use crate::domain::models::{
    CheckpointId, HealthSummary, McpConnectionState, McpHealthRow, McpServerSpec, ToolDefinition,
    ToolResult, TransitionState,
};
use crate::domain::ports::ToolSetPort;
use crate::domain::services::swap_tier::SwapTier;

/// Composes a builtin `ToolSetAdapter` with zero or more `McpClientAdapter`s.
pub struct CompositeToolsetAdapter {
    builtin: Arc<dyn ToolSetPort>,
    mcp_clients: Vec<Arc<crate::adapters::mcp::client::McpClientAdapter>>,
    server_specs: Vec<McpServerSpec>,
    include_builtin: bool,
}

impl CompositeToolsetAdapter {
    pub fn new(
        builtin: Arc<dyn ToolSetPort>,
        mcp_clients: Vec<Arc<crate::adapters::mcp::client::McpClientAdapter>>,
        server_specs: Vec<McpServerSpec>,
        include_builtin: bool,
    ) -> Self {
        Self {
            builtin,
            mcp_clients,
            server_specs,
            include_builtin,
        }
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
}

#[async_trait]
impl ToolSetPort for CompositeToolsetAdapter {
    fn available_tools(&self) -> Vec<ToolDefinition> {
        if self.include_builtin {
            self.builtin.available_tools()
        } else {
            Vec::new()
        }
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
        // Story 9.1: delegate to builtin only.
        // Story 9.2 will route mcp__ prefixed names to MCP clients.
        if tool_name.starts_with("mcp__") {
            tracing::warn!("Story 9.1: MCP invocation attempted but routing lands in 9.2");
            return Err(ToolError::NotFound(tool_name.to_string()));
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
