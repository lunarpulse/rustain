//! MCP client adapter — thin wrapper around `rmcp` for stdio transport.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

/// Global counter for MCP tool_use_id generation to avoid timestamp collisions.
static MCP_TOOL_ID_SEQ: AtomicU64 = AtomicU64::new(0);

/// Process-lifetime D2 warning registry. Server IDs are stable across adapter
/// reconstruction and profile reload, so the same configured server warns once
/// even if its endpoint is edited during the process.
static PLAINTEXT_NOTICE_SERVERS: std::sync::OnceLock<
    tokio::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::OnceLock::new();

use rmcp::ServiceExt;
use rmcp::handler::client::ClientHandler;
use rmcp::model::{ListToolsResult, Meta, Tool};
use rmcp::service::{Peer, RoleClient, RunningService};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::domain::models::HealthSummary;
use crate::domain::models::{McpConnectionState, McpServerSpec, McpTransport};

use super::error::{HttpFailureKind, McpError};
use super::http;
use super::task_driver::McpTaskRuntime;
use super::task_transport::{PeerTaskTransport, TaskGuardTransport};
use super::tasks::{self, CreateTaskReply};

/// 9.9 AC6 — the failure class of the last connect attempt, stored as a plain
/// integer so `health_summary()` can read it without a lock (A9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum FailureClass {
    /// Anything that predates this story: spawn failures, handshake failures,
    /// timeouts. Keeps the existing generic action.
    Generic = 0,
    /// A transport ↔ field inconsistency in the config entry (AC2).
    Config = 1,
    /// The HTTP endpoint could not be reached at all.
    Unreachable = 2,
    /// The host name did not resolve.
    Dns = 3,
    /// `401` + `WWW-Authenticate`.
    Auth = 4,
    /// The server answered badly (`5xx` or a refused protocol response).
    ServerError = 5,
}

impl FailureClass {
    fn from_error(error: &McpError) -> Self {
        match error {
            McpError::InvalidConfig(_) => Self::Config,
            McpError::Http { kind, .. } => match kind {
                HttpFailureKind::Unreachable => Self::Unreachable,
                HttpFailureKind::DnsFailure => Self::Dns,
                HttpFailureKind::AuthRequired => Self::Auth,
                HttpFailureKind::ServerError => Self::ServerError,
            },
            _ => Self::Generic,
        }
    }

    fn from_u8(raw: u8) -> Self {
        match raw {
            1 => Self::Config,
            2 => Self::Unreachable,
            3 => Self::Dns,
            4 => Self::Auth,
            5 => Self::ServerError,
            _ => Self::Generic,
        }
    }

    /// The operator-facing next step. Paired with the state's metric by
    /// `health_summary()`; the house idiom is a terse imperative phrase
    /// (`client.rs` already ships "check server logs", "use a supported
    /// transport").
    fn action(self) -> &'static str {
        match self {
            Self::Generic => "restart rustain or fix server config",
            Self::Config => "fix the mcp config entry for this server",
            Self::Unreachable => "start the server or check the url",
            Self::Dns => "fix the host name in the url",
            Self::Auth => "set the auth token env var (see docs/mcp.md)",
            Self::ServerError => "check server logs",
        }
    }
}

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
    /// 9.9 AC6: the class of the most recent connect failure, so
    /// `health_summary()` can pair the state's metric with a class-specific
    /// ACTION. An `AtomicU8` and not a lock on purpose — the untagged
    /// `std::sync::*Lock` ratchet is at 4/4 with zero headroom (A9), and
    /// widening the persisted `McpConnectionState` enum would churn every
    /// status-panel fixture for one string.
    last_failure_class: AtomicU8,
    /// Loopback verdict parsed once from the configured HTTP URL. The same
    /// value gates the D2 notice and travels into both adapter and doctor
    /// timeout errors.
    http_local: Option<bool>,
}

impl McpClientAdapter {
    pub fn new(
        spec: McpServerSpec,
        event_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::domain::events::AppEvent>>,
    ) -> Self {
        let http_local = match (spec.transport, spec.url.as_ref()) {
            (McpTransport::Http, Some(url)) => url
                .parse_url()
                .ok()
                .map(|parsed| http::parsed_url_is_loopback(&parsed)),
            _ => None,
        };
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
            last_failure_class: AtomicU8::new(FailureClass::Generic as u8),
            http_local,
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

    fn record_failure_class(&self, class: FailureClass) {
        self.last_failure_class.store(class as u8, Ordering::SeqCst);
    }

    /// 9.9 AC9 (D2, warn-and-allow, ratified at SCP approval): a plaintext
    /// `http://` URL pointed off loopback gets ONE warning per server for the
    /// process lifetime — a log line **and** a real `AppEvent::SystemNotice`
    /// the TUI turns into a `FeedbackBlock` — and then the connection proceeds.
    ///
    /// ⚑ Both halves matter. `emit_transport_warnings`' doc comment has claimed
    /// "SystemNotice" since Story 9.1 while the body only called
    /// `tracing::warn!`, and `tracing` reaches `~/.rustain/rustain.log` and
    /// never the TUI (A10). A log line alone is not a warning the operator sees.
    async fn emit_plaintext_notice_once(
        &self,
        url: &crate::domain::models::redacted_url::RedactedUrl,
    ) {
        let mut warned = PLAINTEXT_NOTICE_SERVERS
            .get_or_init(|| tokio::sync::Mutex::new(std::collections::HashSet::new()))
            .lock()
            .await;
        if !warned.insert(self.spec.id.clone()) {
            return;
        }
        drop(warned);

        // `{url}` is the redacting `Display` form — `expose_url()` reaches the
        // connect call and nothing else (A8).
        let message = format!(
            "MCP server '{}': connecting over plaintext http to a non-loopback host ({url}); \
             traffic and any auth token are unencrypted. Use https for remote servers.",
            self.spec.id
        );
        tracing::warn!("{message}");
        if let Some(tx) = &self.event_tx {
            // CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS: 9.9 AC9 — McpClientAdapter
            // owns an `UnboundedSender<AppEvent>` (`ctx.domain_tx`), not an
            // `EventBus`; the sibling `McpConnectionStateChanged` emission in
            // this same file uses the identical channel and event_bus.rs
            // projects it. Routing this one notice differently would mean two
            // channels out of one adapter.
            let _ = tx.send(crate::domain::events::AppEvent::SystemNotice {
                conversation_id: None,
                level: crate::domain::models::NoticeLevel::Warning,
                message,
            });
        }
    }

    /// Classify a caller-owned timeout without reparsing the endpoint.
    ///
    /// `rustain doctor` has a shorter budget than the adapter handshake. It
    /// must preserve the same loopback verdict rather than synthesizing a
    /// transport-blind, exit-neutral timeout.
    pub fn timeout_error(&self, seconds: u64) -> McpError {
        match self.http_local {
            Some(local) => McpError::Http {
                kind: HttpFailureKind::Unreachable,
                local,
                detail: format!("no response within the {seconds}s caller budget"),
            },
            None => McpError::Timeout(seconds),
        }
    }

    /// 9.9 AC8 (D1) — the single D1 boundary message, shared by both guards:
    /// the `call_tool` decode arm (where a task-shaped reply lands over HTTP,
    /// measured at T0.3(2)) and `materialize_task` (the last line before
    /// `runtime.start_task` mints a durable node). One message, two call sites.
    fn refuse_task_off_stdio(&self, task_id: Option<&str>) -> McpError {
        let transport = match self.spec.transport {
            McpTransport::Stdio => "stdio",
            McpTransport::Http => "http",
            McpTransport::Sse => "sse",
        };
        let reason = format!(
            "MCP tasks require the stdio transport; server '{}' speaks {transport} — \
             run this server over stdio, or ask its author for a non-task tool",
            self.spec.id
        );
        tracing::warn!(
            server = %self.spec.id,
            task_id = task_id.unwrap_or("<undecoded>"),
            "{reason}"
        );
        McpError::Unsupported(reason)
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

        // 9.9 AC2 (A17): the fail-closed gate for a per-entry config fault.
        // The parsers deliberately KEEP a malformed entry so its healthy
        // siblings survive (ruling A1); the fault becomes visible here, as
        // `ConnectionFailed { last_error }` in the adapter status panel.
        if let Err(reason) = self.spec.validate_transport_fields() {
            self.record_failure_class(FailureClass::Config);
            self.handle_connection_failure(&reason);
            return Err(McpError::InvalidConfig(reason));
        }

        // 9.9 AC10 / ADR-06-08: SSE is rejected forever — a separate endpoint
        // pair the MCP spec deprecated in 2025-03-26. ⚑ Streamable HTTP's SSE
        // *response stream* is a different thing and is fully supported below.
        if self.spec.transport == McpTransport::Sse {
            let reason = "SSE transport is not supported (deprecated by MCP spec 2025-03-26 per ADR-06-08). Use a proxy like mcp-proxy, or update the server to Streamable HTTP.";
            self.set_state(McpConnectionState::Unsupported {
                reason: reason.to_string(),
            });
            return Err(McpError::Unsupported(reason.to_string()));
        }

        // Transport-specific preparation. Both arms feed the SAME
        // `serve_with_ct` below and yield the SAME
        // `RunningService<RoleClient, McpClientService>` — rmcp's
        // `RunningService<R, S>` carries no transport type parameter, so
        // `self.running` needs no change. ⛔ `Box<dyn Transport>` is not an
        // option: rmcp's `Transport` returns `impl Future` in return position.
        //
        // The stdio child is spawned HERE, outside the handshake timeout,
        // exactly where Story 9.1 put it — moving it inside would change the
        // attempt accounting the lifecycle conformance suite pins.
        let stdio_transport = match self.spec.transport {
            McpTransport::Stdio => {
                // P-14, and 9.9 A2: command validation is STDIO-SPECIFIC. An
                // HTTP-only spec has no command, so this must never run for it.
                let command = self.spec.command.as_deref().ok_or_else(|| {
                    McpError::InvalidConfig(format!(
                        "MCP server '{}': transport = \"stdio\" requires a `command`",
                        self.spec.id
                    ))
                })?;
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
                Some(TaskGuardTransport::spawn(&mut cmd).map_err(|e| {
                    let reason = format!("failed to spawn {command}: {e}");
                    self.record_failure_class(FailureClass::Generic);
                    self.handle_connection_failure(&reason);
                    McpError::SpawnFailed(reason)
                })?)
            }
            McpTransport::Http => None,
            McpTransport::Sse => unreachable!("SSE is refused above"),
        };

        // ⚑ THE loopback answer was computed ONCE at adapter construction (A7).
        // It gates the D2 notice AND travels into every HTTP failure below so
        // `rustain doctor` never performs a second check that can disagree.
        //
        // The rmcp config is built here too, so `expose_url()` — the ONE call in
        // this file, at the connect call and nowhere else (A8) — happens once.
        let http_prepared = match (self.spec.transport, self.spec.url.as_ref()) {
            (McpTransport::Http, Some(url)) => {
                let Some(local) = self.http_local else {
                    return Err(McpError::InvalidConfig(format!(
                        "MCP server '{}': HTTP URL was not parseable",
                        self.spec.id
                    )));
                };
                let parsed = url
                    .parse_url()
                    .expect("transport fields validated before HTTP preparation");
                if parsed.scheme() == "http" && !local {
                    self.emit_plaintext_notice_once(url).await;
                }
                let exposed = url.expose_url();
                Some((local, http::transport_config(&self.spec, exposed)))
            }
            _ => None,
        };
        let http_local = http_prepared.as_ref().map(|(local, _)| *local);

        let ct = self.cancel_token();

        let result = tokio::time::timeout(Duration::from_secs(10), async {
            let service =
                McpClientService::new(self.self_weak.read().unwrap().clone().unwrap_or_default());
            let running = match self.spec.transport {
                McpTransport::Stdio => service
                    .serve_with_ct(stdio_transport.expect("stdio transport prepared above"), ct)
                    .await
                    .map_err(|e| McpError::HandshakeFailed(format!("initialize failed: {e:?}")))?,
                McpTransport::Http => {
                    // Guaranteed by `validate_transport_fields` above; answered
                    // rather than unwrapped so a future caller that skips the
                    // gate degrades instead of panicking.
                    let Some((local, config)) = http_prepared else {
                        return Err(McpError::InvalidConfig(format!(
                            "MCP server '{}': transport = \"http\" requires a `url`",
                            self.spec.id
                        )));
                    };
                    service
                        .serve_with_ct(http::build_transport(config), ct)
                        .await
                        .map_err(|e| {
                            let error = http::classify_init_error(e, local);
                            self.record_failure_class(FailureClass::from_error(&error));
                            error
                        })?
                }
                McpTransport::Sse => unreachable!("SSE is refused above"),
            };

            let tools = match running.list_tools(None).await {
                Ok(ListToolsResult { tools, .. }) => tools,
                Err(e) if self.spec.transport == McpTransport::Http => {
                    let error = http::classify_service_error(
                        e,
                        http_local.expect("HTTP loopback verdict prepared above"),
                    );
                    self.record_failure_class(FailureClass::from_error(&error));
                    return Err(error);
                }
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
                // 9.9 AC6(a): a host that accepts the connection and then never
                // answers is *unreachable* to the operator, not a nameless
                // timeout — and the doctor still needs the loopback answer to
                // tier it. Stdio keeps the pre-9.9 `Timeout(10)` verbatim.
                let (err, reason) = match http_local {
                    Some(local) => {
                        let err = McpError::Http {
                            kind: HttpFailureKind::Unreachable,
                            local,
                            detail: "no response within the 10s handshake budget".to_string(),
                        };
                        self.record_failure_class(FailureClass::from_error(&err));
                        let reason = err.to_string();
                        (err, reason)
                    }
                    // Verbatim pre-9.9 string — the stdio path is unchanged.
                    None => (McpError::Timeout(10), "timeout after 10s".to_string()),
                };
                self.handle_connection_failure(&reason);
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
                // 9.9 AC6: the metric names the condition (it is the classified
                // error's own Display) and the ACTION is class-specific — three
                // distinct HTTP classes must not collapse onto one sentence.
                let class = FailureClass::from_u8(self.last_failure_class.load(Ordering::SeqCst));
                HealthSummary::error(last_error.clone(), class.action())
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
        // 17.5a (R-13): Tasks are a stdio-only extension. Advertising the
        // capability over HTTP invites a conforming server to return a task
        // that this client must then reject at the D1 boundary.
        let mut params = params;
        if self.spec.transport == McpTransport::Stdio {
            params.meta = Some(Meta(
                tasks::tasks_extension_meta()
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ));
        }

        let request = rmcp::model::CallToolRequest::new(params);

        let timeout = std::time::Duration::from_secs(60);
        let call_fut = peer.send_request(rmcp::model::ClientRequest::CallToolRequest(request));

        let result = tokio::select! {
            r = tokio::time::timeout(timeout, call_fut) => match r {
                Ok(Ok(rmcp::model::ServerResult::CallToolResult(res))) => res,
                Ok(Ok(rmcp::model::ServerResult::CustomResult(value))) => {
                    // Both the stdio shim and the guarded HTTP client preserve a
                    // raw task reply under the same wrapper before rmcp's
                    // untagged union can discard fields.
                    let wrapped = tasks::unwrap_task_result(&value.0);
                    if wrapped.is_some() && self.spec.transport != McpTransport::Stdio {
                        return Err(self.refuse_task_off_stdio(None));
                    }
                    let raw = wrapped.unwrap_or(value.0);
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
                // 🔴 9.9 AC8 (D1, ruling A5) — MEASURED at T0.3(2): over HTTP,
                // `guard_response` (the byte-level stdio shim) never runs, so a
                // task-shaped reply meets rmcp's UNTAGGED `ServerResult` union
                // raw and decodes into one of its SUPERSEDED task variants,
                // dropping `resultType`, `result`, `error` and `inputRequests`.
                // Before this arm existed the operator got
                // `CallToolFailed("unexpected server result type")`: opaque, and
                // one field-order change away from becoming a SILENT mis-decode
                // into `CallToolResult`. Fail closed, and name the boundary.
                //
                // ⚑ Detected by SHAPE, through the same `is_task_shaped_result`
                // predicate `guard_response` uses, rather than by matching the
                // superseded variants: those symbols are banned from this
                // directory by 17.5a's R-1 guard, and shape detection is the
                // stronger test anyway — it holds whichever variant rmcp's
                // untagged union happens to pick.
                Ok(Ok(other)) => {
                    let is_task_shaped = serde_json::to_value(&other)
                        .is_ok_and(|value| tasks::is_task_shaped_result(&value));
                    if is_task_shaped {
                        return Err(self.refuse_task_off_stdio(None));
                    }
                    return Err(McpError::CallToolFailed(
                        "unexpected server result type".into()
                    ));
                }
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
        // 9.9 AC8 (D1, ruling A5) — FAIL CLOSED on a task-shaped reply that did
        // not arrive over stdio. This is a DEFECT GUARD, not a scope cut: the
        // Tasks wire shapes are protected by `guard_response`
        // (`task_transport.rs:81-95`), which is a byte-level shim over the
        // child's stdio and therefore NEVER runs over an rmcp HTTP transport. A
        // task-shaped reply would meet rmcp's untagged `ServerResult` union raw
        // — the silent mis-decode into the superseded `GetTaskResult` variant
        // that `task_transport.rs:1-22` exists to prevent. Refuse BEFORE
        // `start_task` mints a durable node, and name the boundary.
        if self.spec.transport != McpTransport::Stdio {
            return Err(self.refuse_task_off_stdio(Some(reply.task.task_id.as_str())));
        }
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
    /// `_meta` (R-13). This is the test-arming seam ONLY: 17.5a's tests drive
    /// the scripted fake through it. Production `tasks/update` rides
    /// `McpTaskTransport::tasks_update` (task_transport.rs), NOT this path —
    /// routing production here would bypass the driver's state machine.
    /// Returns the raw result payload (transport-shim wrapper already removed).
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
