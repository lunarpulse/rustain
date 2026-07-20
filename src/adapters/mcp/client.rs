//! MCP client adapter — thin wrapper around `rmcp` for stdio transport.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

/// Global counter for MCP tool_use_id generation to avoid timestamp collisions.
static MCP_TOOL_ID_SEQ: AtomicU64 = AtomicU64::new(0);

use rmcp::ServiceExt;
use rmcp::handler::client::ClientHandler;
use rmcp::model::{ListToolsResult, Meta, Tool};
use rmcp::service::{Peer, RoleClient, RunningService};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::domain::models::HealthSummary;
use crate::domain::models::{McpConnectionState, McpServerSpec, McpTransport};

use super::error::McpError;
use super::task_driver::McpTaskRuntime;
use super::task_transport::{PeerTaskTransport, TaskGuardTransport};
use super::tasks::{self, CreateTaskReply};

struct McpClientService {
    adapter: std::sync::Weak<McpClientAdapter>,
}

impl McpClientService {
    fn new(adapter: std::sync::Weak<McpClientAdapter>) -> Self {
        Self { adapter }
    }
}

impl ClientHandler for McpClientService {
    fn on_tool_list_changed(
        &self,
        _context: rmcp::service::NotificationContext<rmcp::service::RoleClient>,
    ) -> impl std::future::Future<Output = ()> + std::marker::Send + '_ {
        async {
            if let Some(adapter) = self.adapter.upgrade() {
                // Debounce: skip if refresh ran within last 100ms
                let last_refresh = adapter.last_refresh_ms.load(Ordering::SeqCst);
                let now = now_unix();
                if now.saturating_sub(last_refresh) < 100 {
                    tracing::debug!(server = %adapter.server_id(), "list_changed debounced");
                    return;
                }
                adapter.last_refresh_ms.store(now, Ordering::SeqCst);
                if let Err(e) = adapter.refresh_cached_tools().await {
                    tracing::warn!(
                        server = %adapter.server_id(),
                        error = %e,
                        "Failed to refresh cached tools on list_changed notification"
                    );
                }
            }
        }
    }
}

/// Per-server MCP client handle.
///
/// Uses `std::sync::RwLock` for `state` and `cached_tools` because these are
/// quick clone operations never held across `.await` points. This follows
/// tokio's recommendation: prefer `std::sync` when the lock is held briefly
/// and synchronously. The `running` field uses `tokio::sync::Mutex` because
/// it may be held across `.await` during connect/disconnect.
pub struct McpClientAdapter {
    pub(crate) spec: McpServerSpec,
    state: std::sync::RwLock<McpConnectionState>, // CONFORMANCE_EXCEPTION_STD_SYNC_LOCK: PERMANENT per ADR-09-01 — quick clone reads for status panel, never held across .await
    cached_tools: std::sync::RwLock<Option<Vec<Tool>>>, // CONFORMANCE_EXCEPTION_STD_SYNC_LOCK: PERMANENT per ADR-09-01 — quick clone reads for tool list, never held across .await
    running: tokio::sync::Mutex<Option<RunningService<RoleClient, McpClientService>>>,
    reconnect_attempts: AtomicU32,
    cancel_token: std::sync::RwLock<CancellationToken>, // CONFORMANCE_EXCEPTION_STD_SYNC_LOCK: PERMANENT per ADR-09-01 — quick clone for cancel token, never held across .await
    event_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::domain::events::AppEvent>>,
    self_weak: std::sync::RwLock<Option<std::sync::Weak<McpClientAdapter>>>,
    last_refresh_ms: std::sync::atomic::AtomicU64,
    /// 17.5a: the task runtime (domain seams + clock), injected once at the
    /// composition root after the node tree/journal exist. Until set, a
    /// `resultType: "task"` reply degrades to a text result (no node) —
    /// observable, never a panic.
    task_runtime: std::sync::OnceLock<Arc<McpTaskRuntime>>,
}

impl McpClientAdapter {
    pub fn new(
        spec: McpServerSpec,
        event_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::domain::events::AppEvent>>,
    ) -> Self {
        Self {
            spec,
            state: std::sync::RwLock::new(McpConnectionState::NotConnected), // CONFORMANCE_EXCEPTION_STD_SYNC_LOCK: PERMANENT per ADR-09-01
            cached_tools: std::sync::RwLock::new(None), // CONFORMANCE_EXCEPTION_STD_SYNC_LOCK: PERMANENT per ADR-09-01
            running: tokio::sync::Mutex::new(None),
            reconnect_attempts: AtomicU32::new(0),
            cancel_token: std::sync::RwLock::new(CancellationToken::new()), // CONFORMANCE_EXCEPTION_STD_SYNC_LOCK: PERMANENT per ADR-09-01
            event_tx,
            self_weak: std::sync::RwLock::new(None),
            last_refresh_ms: std::sync::atomic::AtomicU64::new(0),
            task_runtime: std::sync::OnceLock::new(),
        }
    }

    /// Inject the 17.5a task runtime (called once by the composition root).
    pub fn set_task_runtime(&self, runtime: Arc<McpTaskRuntime>) {
        let _ = self.task_runtime.set(runtime);
    }

    pub fn set_self_weak(&self, weak: std::sync::Weak<McpClientAdapter>) {
        *self.self_weak.write().unwrap() = Some(weak);
    }

    pub fn server_id(&self) -> &str {
        &self.spec.id
    }

    pub fn state(&self) -> McpConnectionState {
        self.state.read().unwrap().clone()
    }

    pub fn cached_tools(&self) -> Option<Vec<Tool>> {
        self.cached_tools.read().unwrap().clone()
    }

    /// Number of cached tools (0 if not yet connected).
    /// Reads `cached_tools.len()` — sync, in-policy under `CONFORMANCE_EXCEPTION_STD_SYNC_LOCK`.
    pub fn tool_count(&self) -> usize {
        self.cached_tools
            .read()
            .unwrap()
            .as_ref()
            .map_or(0, |v| v.len())
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel_token.read().unwrap().clone()
    }

    fn emit_state_change(&self, new_state: &McpConnectionState) {
        if let Some(tx) = &self.event_tx {
            let _ = tx.send(crate::domain::events::AppEvent::McpConnectionStateChanged {
                server_id: self.spec.id.clone(),
                state: new_state.clone(),
                source_profile: match &self.spec.source {
                    crate::domain::models::McpServerSource::Profile { profile_name } => {
                        Some(profile_name.clone())
                    }
                    _ => None,
                },
            });
        }
    }

    fn set_state(&self, new_state: McpConnectionState) {
        let mut guard = self.state.write().unwrap();
        self.emit_state_change(&new_state);
        *guard = new_state;
    }

    pub async fn connect(&self) -> Result<(), McpError> {
        // P-6: Guard against concurrent invocations
        {
            let current = self.state.read().unwrap();
            if matches!(
                *current,
                McpConnectionState::Connected { .. }
                    | McpConnectionState::Connecting { .. }
                    | McpConnectionState::Reconnecting { .. }
            ) {
                return Ok(());
            }
        }

        // 17.5a (D1/P2): mint a fresh owner token for THIS session. `disconnect`
        // cancels the previous one and leaves it cancelled so a task that
        // materializes mid-teardown is refused admission (`start_task` checks
        // `owner_cancel.is_cancelled()`).
        {
            let mut ct = self.cancel_token.write().unwrap();
            *ct = CancellationToken::new();
        }

        let started = now_unix();
        self.set_state(McpConnectionState::Connecting {
            attempt: 1,
            started_at_ms: started,
        });

        // P-5: Set Unsupported state for non-stdio transports
        if self.spec.transport != McpTransport::Stdio {
            let reason = match self.spec.transport {
                McpTransport::Http => "http transport deferred to a later Epic 9 story; skipping",
                McpTransport::Sse => {
                    "SSE transport is not supported (deprecated by MCP spec 2025-03-26 per ADR-06-08). Use a proxy like mcp-proxy, or update the server to Streamable HTTP."
                }
                _ => "unknown transport",
            };
            self.set_state(McpConnectionState::Unsupported {
                reason: reason.to_string(),
            });
            return Err(McpError::Unsupported(reason.to_string()));
        }

        // P-14: Validate command is non-empty
        let command = self
            .spec
            .command
            .as_deref()
            .ok_or_else(|| McpError::SpawnFailed("no command configured".into()))?;
        if command.is_empty() {
            let reason = format!(
                "command resolved to empty string for server '{}'",
                self.spec.id
            );
            return Err(McpError::SpawnFailed(reason));
        }

        let mut cmd = Command::new(command);
        cmd.args(&self.spec.args);
        for (k, v) in &self.spec.env {
            cmd.env(k, v);
        }
        cmd.kill_on_drop(true);

        // 17.5a (ADR-17-5-01 D1 amendment): the byte-level transport shim.
        // rmcp's untagged `ServerResult` decode would silently parse
        // task-shaped replies into its SUPERSEDED `GetTaskResult` shape,
        // dropping the inlined result/error/inputRequests. The shim wraps
        // task-shaped payloads so they arrive as `CustomResult` and decode
        // through our own serde types. Non-task traffic is byte-identical.
        let transport = TaskGuardTransport::spawn(&mut cmd).map_err(|e| {
            let reason = format!("failed to spawn {command}: {e}");
            self.handle_connection_failure(&reason);
            McpError::SpawnFailed(reason)
        })?;

        let ct = self.cancel_token();

        let result = tokio::time::timeout(Duration::from_secs(10), async {
            let service =
                McpClientService::new(self.self_weak.read().unwrap().clone().unwrap_or_default());
            let running = service
                .serve_with_ct(transport, ct)
                .await
                .map_err(|e| McpError::HandshakeFailed(format!("initialize failed: {e:?}")))?;

            let tools = match running.list_tools(None).await {
                Ok(ListToolsResult { tools, .. }) => tools,
                Err(e) => {
                    let now = now_unix();
                    let reason = format!("tools/list failed: {e:?}");
                    // P-7: Set Degraded state and store running service
                    self.set_state(McpConnectionState::Degraded {
                        since_ms: now,
                        reason: reason.clone(),
                    });
                    {
                        let mut running_guard = self.running.lock().await;
                        *running_guard = Some(running);
                    }
                    return Err(McpError::ToolsListFailed(reason));
                }
            };

            let tool_count = tools.len();
            {
                let mut cache = self.cached_tools.write().unwrap();
                *cache = Some(tools);
            }

            let now = now_unix();
            self.set_state(McpConnectionState::Connected {
                connected_at_ms: now,
                tool_count,
            });

            {
                let mut running_guard = self.running.lock().await;
                *running_guard = Some(running);
            }

            Ok(())
        })
        .await;

        match result {
            Ok(Ok(())) => {
                self.reconnect_attempts.store(0, Ordering::SeqCst);
                Ok(())
            }
            // P-7: Degraded is a partial success — don't overwrite with ConnectionFailed
            Ok(Err(McpError::ToolsListFailed(_))) => {
                Err(McpError::ToolsListFailed("server in degraded state".into()))
            }
            Ok(Err(e)) => {
                self.handle_connection_failure(&e.to_string());
                Err(e)
            }
            Err(_timeout) => {
                let err = McpError::Timeout(10);
                self.handle_connection_failure("timeout after 10s");
                Err(err)
            }
        }
    }

    fn handle_connection_failure(&self, reason: &str) {
        let attempts = self.reconnect_attempts.fetch_add(1, Ordering::SeqCst) + 1;
        self.set_state(McpConnectionState::ConnectionFailed {
            attempts,
            last_error: reason.to_string(),
        });
    }

    pub async fn disconnect(&self) -> Result<(), McpError> {
        // 17.5a (D1/AC5): terminalize in-flight task nodes through their
        // ack-gated cooperative cancel FIRST — `kill_all_tasks` fires each
        // driver's node cancel and the driver issues a real `tasks/cancel` over
        // the STILL-LIVE peer, terminalizing on the ack. `tearing_down` (set
        // inside `kill_all_tasks`) closes the admission window meanwhile.
        if let Some(runtime) = self.task_runtime.get() {
            runtime.kill_all_tasks().await;
        }

        // Only now cancel the owner token (tears the peer down) and leave it
        // cancelled through teardown so a late admission is refused;
        // `connect()` mints a fresh token for the next session.
        {
            let ct = self.cancel_token.read().unwrap();
            ct.cancel();
        }

        {
            let mut running_guard = self.running.lock().await;
            if let Some(mut running) = running_guard.take() {
                let _ = running.close().await;
            }
        }

        self.set_state(McpConnectionState::NotConnected);
        Ok(())
    }

    pub fn health_summary(&self) -> HealthSummary {
        let state = self.state();
        match &state {
            McpConnectionState::Connected { tool_count, .. } => {
                HealthSummary::healthy(format!("tools: {tool_count}"))
            }
            McpConnectionState::Degraded { reason, .. } => {
                HealthSummary::degraded(reason.clone(), "check server logs")
            }
            McpConnectionState::Reconnecting { attempt, .. } => {
                HealthSummary::degraded(format!("reconnecting {attempt}/5"), "wait or restart")
            }
            McpConnectionState::ConnectionFailed { last_error, .. } => {
                HealthSummary::error(last_error.clone(), "restart rustain or fix server config")
            }
            McpConnectionState::Unsupported { reason } => {
                HealthSummary::error(reason.clone(), "use a supported transport")
            }
            _ => HealthSummary::unknown(),
        }
    }

    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<crate::domain::models::ToolResult, McpError> {
        let running_guard = self.running.lock().await;
        let running = running_guard
            .as_ref()
            .ok_or(McpError::TransportClosed("not connected".into()))?;
        let peer = running.peer().clone();
        drop(running_guard);

        let params = if let Some(args) = arguments.as_object().cloned() {
            rmcp::model::CallToolRequestParams::new(tool_name.to_string()).with_arguments(args)
        } else {
            return Err(McpError::CallToolFailed(
                "arguments must be a JSON object".into(),
            ));
        };
        // 17.5a (R-13): advertise the Tasks extension in per-request `_meta`.
        // The server decides whether to create a task; there is NO client
        // opt-in field (`CallToolRequestParams::with_task` is the deleted
        // SEP-1686 knob and is never called).
        let mut params = params;
        params.meta = Some(Meta(
            tasks::tasks_extension_meta()
                .as_object()
                .cloned()
                .unwrap_or_default(),
        ));

        let request = rmcp::model::CallToolRequest::new(params);

        let timeout = std::time::Duration::from_secs(60);
        let call_fut = peer.send_request(rmcp::model::ClientRequest::CallToolRequest(request));

        let result = tokio::select! {
            r = tokio::time::timeout(timeout, call_fut) => match r {
                Ok(Ok(rmcp::model::ServerResult::CallToolResult(res))) => res,
                Ok(Ok(rmcp::model::ServerResult::CustomResult(value))) => {
                    // A task-shaped reply survives the transport shim as a
                    // wrapped CustomResult. Decode it through OUR types.
                    let raw = tasks::unwrap_task_result(&value.0).unwrap_or(value.0);
                    let reply: CreateTaskReply = serde_json::from_value(raw).map_err(|e| {
                        McpError::TaskProtocol(format!("task creation reply decode: {e}"))
                    })?;
                    if !reply.is_task() {
                        return Err(McpError::TaskProtocol(
                            "custom result without resultType:task on tools/call".into(),
                        ));
                    }
                    return self.materialize_task(peer, reply).await;
                }
                Ok(Ok(_other)) => return Err(McpError::CallToolFailed(
                    "unexpected server result type".into()
                )),
                Ok(Err(e)) => {
                    // P-25: Distinguish transport-closed from other errors
                    let err_str = format!("{e}");
                    if err_str.contains("transport") || err_str.contains("closed") {
                        return Err(McpError::TransportClosed(err_str));
                    }
                    return Err(McpError::CallToolFailed(err_str));
                }
                Err(_) => return Err(McpError::Timeout(60)),
            },
            _ = cancel.cancelled() => return Err(McpError::Cancelled),
        };

        let seq = MCP_TOOL_ID_SEQ.fetch_add(1, Ordering::SeqCst);
        let tool_use_id = format!("mcp-{}-{}", chrono::Utc::now().timestamp_millis(), seq);
        Ok(super::tool_projection::project_rmcp_result(
            result,
            tool_use_id,
        ))
    }

    /// 17.5a (AC1): a `tools/call` reply with `resultType: "task"` becomes a
    /// first-class durable node. With the runtime injected (production), the
    /// node is materialized and its driver spawned; without it (doctor /
    /// offline probes), degrade to a descriptive text result — observable,
    /// never a panic, and the non-task path is untouched.
    async fn materialize_task(
        &self,
        peer: Peer<RoleClient>,
        reply: CreateTaskReply,
    ) -> Result<crate::domain::models::ToolResult, McpError> {
        let task_id = reply.task.task_id.clone();
        let Some(runtime) = self.task_runtime.get() else {
            tracing::warn!(
                server = %self.spec.id,
                %task_id,
                "MCP server returned a task but no task runtime is wired; \
                 returning a text result without a durable node"
            );
            let seq = MCP_TOOL_ID_SEQ.fetch_add(1, Ordering::SeqCst);
            return Ok(crate::domain::models::ToolResult {
                tool_use_id: format!("mcp-task-unwired-{seq}"),
                content: format!(
                    "MCP server '{}' created task {task_id} but this client has no \
                     task runtime; the task runs untracked on the server.",
                    self.spec.id
                ),
                is_error: false,
            });
        };
        let transport = Arc::new(PeerTaskTransport::new(peer));
        runtime
            .start_task(&self.spec.id, reply, transport, self.cancel_token())
            .await
    }

    /// Test hook: is a task runtime wired?
    #[cfg(test)]
    pub(crate) fn has_task_runtime(&self) -> bool {
        self.task_runtime.get().is_some()
    }

    /// Send an arbitrary JSON-RPC method over the live connection as a
    /// `CustomRequest`, advertising the Tasks extension in per-request
    /// `_meta` (R-13). Method-generic seam: 17.5a's tests arm the scripted
    /// fake through it, and 17.5b's `tasks/update` will ride it. Returns
    /// the raw result payload (transport-shim wrapper already removed).
    pub async fn send_custom_request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        let running_guard = self.running.lock().await;
        let running = running_guard
            .as_ref()
            .ok_or(McpError::TransportClosed("not connected".into()))?;
        let peer = running.peer().clone();
        drop(running_guard);

        let mut params = params;
        if let Some(obj) = params.as_object_mut() {
            obj.insert("_meta".into(), tasks::tasks_extension_meta());
        }
        let request = rmcp::model::ClientRequest::CustomRequest(rmcp::model::CustomRequest::new(
            method,
            Some(params),
        ));
        match peer.send_request(request).await {
            Ok(rmcp::model::ServerResult::CustomResult(value)) => {
                Ok(tasks::unwrap_task_result(&value.0).unwrap_or(value.0))
            }
            Ok(_other) => Err(McpError::TaskProtocol(format!(
                "{method}: server replied in a legacy typed shape"
            ))),
            Err(e) => Err(McpError::TaskProtocol(format!("{method}: {e}"))),
        }
    }

    pub async fn refresh_cached_tools(&self) -> Result<(), McpError> {
        let running_guard = self.running.lock().await;
        let running = running_guard
            .as_ref()
            .ok_or(McpError::TransportClosed("not connected".into()))?;
        let result = running.list_tools(None).await;
        drop(running_guard);

        let tools = match result {
            Ok(rmcp::model::ListToolsResult { tools, .. }) => tools,
            Err(e) => {
                return Err(McpError::ToolsListFailed(format!("{e:?}")));
            }
        };

        let tool_count = tools.len();
        {
            let mut cache = self.cached_tools.write().unwrap();
            *cache = Some(tools);
        }

        if let Some(tx) = &self.event_tx {
            let _ = tx.send(crate::domain::events::AppEvent::McpCatalogChanged {
                server_id: self.spec.id.clone(),
                tool_count,
            });
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_call_tool_returns_transport_closed_when_not_running() {
        let spec = McpServerSpec {
            id: "test".to_string(),
            transport: McpTransport::Stdio,
            command: Some("true".to_string()),
            args: vec![],
            env: std::collections::BTreeMap::new(),
            url: None,
            persistent: false,
            source: crate::domain::models::McpServerSource::Workspace,
        };
        let client = McpClientAdapter::new(spec, None);
        let result = client
            .call_tool("echo", serde_json::json!({}), CancellationToken::new())
            .await;
        assert!(
            matches!(result, Err(McpError::TransportClosed(_))),
            "should return TransportClosed when not connected, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_weak_pointer_upgrade_failure() {
        let spec = McpServerSpec {
            id: "weak-test".to_string(),
            transport: McpTransport::Stdio,
            command: Some("true".to_string()),
            args: vec![],
            env: std::collections::BTreeMap::new(),
            url: None,
            persistent: false,
            source: crate::domain::models::McpServerSource::Workspace,
        };
        let service = {
            let client = Arc::new(McpClientAdapter::new(spec, None));
            // set_self_weak not called — simulates a bug where the weak ref is never set
            let svc = McpClientService::new(Arc::downgrade(&client));
            // client is dropped here, so the weak ref becomes dangling
            svc
        };
        // The on_tool_list_changed should handle upgrade failure gracefully.
        // We can't easily construct a NotificationContext without a real Peer,
        // but we verify the service struct can be created with a dangling weak ref.
        assert!(
            service.adapter.upgrade().is_none(),
            "weak ref should fail to upgrade after strong ref dropped"
        );
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
