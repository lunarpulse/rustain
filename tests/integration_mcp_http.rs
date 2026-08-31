//! Story 9.9 — the MCP Streamable HTTP client transport, end to end.
//!
//! Every keystone here enters through the production front door,
//! `McpClientAdapter::connect()` (and, for the retry envelope,
//! `lazy_connect::lazy_connect_all` — the real caller). ⛔ None of them
//! constructs an rmcp transport and calls `serve_with_ct` directly: that would
//! prove rmcp works, not that rustain wired it (authoring-rules Rule 2).
//!
//! The HTTP server is `common::fake_mcp_http_server` — in-process, bound to
//! `127.0.0.1:0`, deterministic. ⛔ No test here dials a real MCP server or
//! reaches the network; AC11's real-server evidence is a manual receipt in the
//! story record, deliberately not a `#[test]`.

#![cfg(all(feature = "mcp", feature = "test-fake-mcp"))]

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use common::fake_mcp_http_server::{
    CallBehaviour, FakeHttpConfig, FakeHttpMcpServer, GetBehaviour, InitBehaviour, ListBehaviour,
};
use rustain::adapters::mcp::client::McpClientAdapter;
use rustain::adapters::mcp::error::{HttpFailureKind, McpError};
use rustain::adapters::mcp::http::auth_token_env_for;
use rustain::adapters::mcp::lazy_connect::lazy_connect_all;
use rustain::adapters::mcp::lifecycle::shutdown_all_clients;
use rustain::adapters::mcp::task_driver::mint_mcp_node_id;
use rustain::adapters::mcp::tool_projection::project_tool;
use rustain::domain::events::AppEvent;
use rustain::domain::models::tool_call::ApprovalSource;
use rustain::domain::models::{
    ApprovalOutcome, ApprovalScope, HealthLevel, McpConnectionState, McpServerSource,
    McpServerSpec, McpTransport, NoticeLevel, ToolRisk,
};
use tokio_util::sync::CancellationToken;

type Events = tokio::sync::mpsc::UnboundedReceiver<AppEvent>;

fn http_spec(id: &str, url: &str) -> McpServerSpec {
    McpServerSpec {
        id: id.to_string(),
        transport: McpTransport::Http,
        command: None,
        args: vec![],
        env: BTreeMap::new(),
        url: Some(url.to_string().into()),
        persistent: false,
        source: McpServerSource::Workspace,
    }
}

fn stdio_spec(id: &str) -> McpServerSpec {
    McpServerSpec {
        id: id.to_string(),
        transport: McpTransport::Stdio,
        command: Some(common::fake_mcp_binary().to_string_lossy().into_owned()),
        args: vec![],
        env: BTreeMap::new(),
        url: None,
        persistent: false,
        source: McpServerSource::Workspace,
    }
}

fn adapter(spec: McpServerSpec) -> (Arc<McpClientAdapter>, Events) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    let client = Arc::new(McpClientAdapter::new(spec, Some(tx)));
    client.set_self_weak(Arc::downgrade(&client));
    (client, rx)
}

fn drain(rx: &mut Events) -> Vec<AppEvent> {
    let mut out = Vec::new();
    while let Ok(event) = rx.try_recv() {
        out.push(event);
    }
    out
}

fn notices(events: &[AppEvent]) -> Vec<(NoticeLevel, String)> {
    events
        .iter()
        .filter_map(|event| match event {
            AppEvent::SystemNotice { level, message, .. } => Some((*level, message.clone())),
            _ => None,
        })
        .collect()
}

fn state_changes(events: &[AppEvent]) -> Vec<McpConnectionState> {
    events
        .iter()
        .filter_map(|event| match event {
            AppEvent::McpConnectionStateChanged { state, .. } => Some(state.clone()),
            _ => None,
        })
        .collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// AC1 — a `transport = "http"` server with a `url` connects, lists, and
// projects exactly as stdio.
// ─────────────────────────────────────────────────────────────────────────────

/// Front door: `connect()`. Positive control: the SAME catalogue driven over
/// stdio and over HTTP in one test must project identically — parity is the
/// claim, so both legs run.
#[tokio::test]
async fn ac1_http_connects_and_projects_the_same_catalogue_as_stdio() {
    let server = FakeHttpMcpServer::start(FakeHttpConfig::healthy()).await;
    let (http_client, _http_rx) = adapter(http_spec("remote", &server.url()));
    http_client
        .connect()
        .await
        .expect("http connect must succeed");

    let (stdio_client, _stdio_rx) = adapter(stdio_spec("remote"));
    stdio_client
        .connect()
        .await
        .expect("stdio connect must succeed");

    let project = |client: &McpClientAdapter| {
        let mut projected: Vec<_> = client
            .cached_tools()
            .expect("a connected client caches its tools")
            .iter()
            .map(|tool| project_tool("remote", tool))
            .map(|def| {
                (
                    def.name,
                    def.description,
                    def.input_schema,
                    def.parallel_safe,
                )
            })
            .collect();
        projected.sort_by(|a, b| a.0.cmp(&b.0));
        projected
    };

    let over_http = project(&http_client);
    let over_stdio = project(&stdio_client);

    assert_eq!(
        over_http.iter().map(|t| t.0.clone()).collect::<Vec<_>>(),
        vec![
            "mcp__remote__add".to_string(),
            "mcp__remote__echo".to_string()
        ],
        "HTTP tools must project through the unchanged `mcp__<server>__<tool>` naming"
    );
    assert_eq!(
        over_http, over_stdio,
        "the projected catalogue must be transport-blind — name, description, schema and \
         parallel_safe all included"
    );

    stdio_client.disconnect().await.expect("stdio disconnect");
    http_client.disconnect().await.expect("http disconnect");
}

/// State flows `Connecting { attempt: 1 }` → `Connected { tool_count }`, each
/// transition observable on the production event channel.
#[tokio::test]
async fn ac1_http_state_flows_connecting_then_connected_on_the_event_channel() {
    let server = FakeHttpMcpServer::start(FakeHttpConfig::healthy()).await;
    let (client, mut rx) = adapter(http_spec("remote", &server.url()));
    client.connect().await.expect("http connect");

    let observed = state_changes(&drain(&mut rx));
    assert!(
        matches!(
            observed.first(),
            Some(McpConnectionState::Connecting { attempt: 1, .. })
        ),
        "first transition must be Connecting {{ attempt: 1 }}, got {observed:?}"
    );
    assert!(
        matches!(
            observed.last(),
            Some(McpConnectionState::Connected { tool_count: 2, .. })
        ),
        "last transition must be Connected with the two fixture tools, got {observed:?}"
    );
    assert!(
        matches!(
            client.state(),
            McpConnectionState::Connected { tool_count: 2, .. }
        ),
        "terminal state must be Connected"
    );
    assert_eq!(
        client.health_summary().level,
        HealthLevel::Healthy,
        "a healthy HTTP server reads Healthy in the status panel"
    );
    assert!(
        client.health_summary().metric.contains("tools: 2"),
        "positive control for AC6: a healthy server keeps the unchanged metric idiom, got {:?}",
        client.health_summary().metric
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// AC3 — invocation over HTTP round-trips through the same path as stdio.
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn ac3_http_invocation_round_trips_through_the_shared_projection() {
    let server = FakeHttpMcpServer::start(FakeHttpConfig::healthy()).await;
    let (http_client, _rx) = adapter(http_spec("remote", &server.url()));
    http_client.connect().await.expect("http connect");

    let over_http = http_client
        .call_tool(
            "echo",
            serde_json::json!({"text": "over http"}),
            CancellationToken::new(),
        )
        .await
        .expect("http tool call must succeed");
    assert_eq!(over_http.content, "echo: over http");
    assert!(!over_http.is_error);

    // Positive control — parity is the claim, so the stdio leg runs too.
    let (stdio_client, _stdio_rx) = adapter(stdio_spec("remote"));
    stdio_client.connect().await.expect("stdio connect");
    let over_stdio = stdio_client
        .call_tool(
            "echo",
            serde_json::json!({"text": "over http"}),
            CancellationToken::new(),
        )
        .await
        .expect("stdio tool call must succeed");
    assert_eq!(
        over_http.content, over_stdio.content,
        "both transports must render through the unchanged `project_rmcp_result`"
    );

    stdio_client.disconnect().await.expect("stdio disconnect");
    http_client.disconnect().await.expect("http disconnect");
}

/// The persisted `AlwaysServer` approval is keyed on the server id and is
/// therefore transport-blind. Mutant: grant it for a DIFFERENT server id and the
/// HTTP call must still prompt.
#[tokio::test]
async fn ac3_always_server_approval_binds_by_server_id_not_by_transport() {
    use rustain::adapters::approval_persistence_toml::ApprovalPersistenceToml;
    use rustain::adapters::composite_toolset_adapter::CompositeToolsetAdapter;
    use rustain::adapters::noop::{NoOpSecurity, NoOpToolSet};
    use rustain::domain::models::tool_call::{ToolCall, ToolCallRequest};
    use rustain::domain::ports::ApprovalPersistencePort;
    use rustain::domain::ports::{SecurityPort, ToolSetPort};
    use rustain::domain::services::approval_runtime::ApprovalRuntime;
    use rustain::domain::services::tool_scheduler::ToolScheduler;

    let tmp = tempfile::tempdir().expect("tempdir");
    let persistence: Arc<dyn ApprovalPersistencePort> = Arc::new(ApprovalPersistenceToml::new(
        tmp.path().join("config.toml"),
        tmp.path().join("permissions.toml"),
    ));
    let runtime = ApprovalRuntime::new(16, persistence);

    let (id, _rx) = runtime
        .request(
            ApprovalSource::ForegroundTurn {
                conversation_id: "c-9-9".into(),
            },
            "mcp__remote__echo".to_string(),
            serde_json::json!({"text": "hi"}),
            ToolRisk::Elevated,
            Some("mcp__remote"),
            None,
        )
        .await;
    let id = id.expect("first HTTP-server call must take the slow path");
    runtime
        .resolve(
            &id,
            ApprovalOutcome::AlwaysAndSave {
                scope: ApprovalScope::Server("mcp__remote".into()),
            },
        )
        .await;

    // Same server id → the production ToolScheduler permission chain
    // auto-approves, then CompositeToolsetAdapter dispatches the real HTTP call.
    let server = FakeHttpMcpServer::start(FakeHttpConfig::healthy()).await;
    let spec = http_spec("remote", &server.url());
    let (client, _events) = adapter(spec.clone());
    client.connect().await.expect("HTTP front door connects");
    let tools: Arc<dyn ToolSetPort> = Arc::new(CompositeToolsetAdapter::new(
        Arc::new(NoOpToolSet),
        vec![client.clone()],
        vec![spec],
        false,
        None,
        None,
        None,
    ));
    let security: Arc<dyn SecurityPort> = Arc::new(NoOpSecurity);
    let scheduler = ToolScheduler::new(security, tools, runtime.clone(), 16);
    let terminal = scheduler
        .schedule(
            ApprovalSource::ForegroundTurn {
                conversation_id: "c-9-9".into(),
            },
            vec![ToolCallRequest {
                id: "approved-http-call".into(),
                tool_name: "mcp__remote__echo".into(),
                input: serde_json::json!({"text": "hi"}),
            }],
            CancellationToken::new(),
            None,
        )
        .await;
    assert!(
        matches!(
            terminal.as_slice(),
            [ToolCall::Success { result, .. }] if result.output == "echo: hi"
        ),
        "persisted server approval must reach HTTP execution: {terminal:?}"
    );
    assert_eq!(server.observations().tool_calls, 1);

    // Mutant: a grant for a different server id must NOT cover this one.
    let (other, _rx) = runtime
        .request(
            ApprovalSource::ForegroundTurn {
                conversation_id: "c-9-9".into(),
            },
            "mcp__elsewhere__echo".to_string(),
            serde_json::json!({"text": "hi"}),
            ToolRisk::Elevated,
            Some("mcp__elsewhere"),
            None,
        )
        .await;
    assert!(
        other.is_some(),
        "an approval granted for one MCP server must not leak to another"
    );
    client.disconnect().await.expect("disconnect");
}

// ─────────────────────────────────────────────────────────────────────────────
// AC4 — ONE retry owner on the initial-connect path.
// ─────────────────────────────────────────────────────────────────────────────

/// The rmcp transport is pinned to `NeverRetry`, so when the server's standalone
/// SSE response stream closes, the client must NOT reopen it. Mutant: leave
/// `retry_config` at rmcp's default `ExponentialBackoff` → a second GET appears
/// inside a second and this turns RED.
#[tokio::test]
async fn ac4_never_retry_means_a_closed_sse_stream_is_not_reopened() {
    let server = FakeHttpMcpServer::start(FakeHttpConfig {
        assign_session: true,
        get: GetBehaviour::OpenThenClose,
        ..FakeHttpConfig::healthy()
    })
    .await;
    let (client, _rx) = adapter(http_spec("remote", &server.url()));
    client.connect().await.expect("http connect");

    tokio::time::sleep(Duration::from_millis(1_600)).await;

    let observed = server.observations();
    assert_eq!(
        observed.get_requests, 1,
        "with NeverRetry the standalone SSE stream is opened exactly once; \
         rmcp's default ExponentialBackoff would have reopened it by now \
         (observations: {observed:?})"
    );
    client.disconnect().await.expect("http disconnect");
}

/// Positive control for the retry loop, driven through the real production
/// caller (`lazy_connect_all` → `spawn_reconnect_task`): a server that refuses
/// twice and then accepts must reach `Connected` on the THIRD attempt, and the
/// attempt counter must stop there.
///
/// ⚑ Rule 4: the assertion is on the COUNT and ORDERING of attempts the server
/// itself observed, never on a wall-clock duration.
#[tokio::test]
async fn ac4_positive_control_retry_reaches_connected_on_the_third_attempt() {
    let server = FakeHttpMcpServer::start(FakeHttpConfig {
        refuse_initialize_times: 2,
        ..FakeHttpConfig::healthy()
    })
    .await;
    let (client, _rx) = adapter(http_spec("remote", &server.url()));

    let sink = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let closed = Arc::new(AtomicBool::new(false));
    lazy_connect_all(vec![client.clone()], sink.clone(), closed).await;

    // 1s + 2s of backoff before the accepting attempt.
    let deadline = Instant::now() + Duration::from_secs(12);
    while Instant::now() < deadline
        && !matches!(client.state(), McpConnectionState::Connected { .. })
    {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let observed = server.observations();
    assert!(
        matches!(client.state(), McpConnectionState::Connected { .. }),
        "the retry loop must be able to SUCCEED, not only to exhaust; state = {:?}, \
         observations = {observed:?}",
        client.state()
    );
    assert_eq!(
        observed.initialize_posts, 3,
        "exactly three initialize attempts: the direct connect plus two retries \
         ({observed:?})"
    );
    let gaps: Vec<u128> = observed
        .initialize_offsets_ms
        .windows(2)
        .map(|w| w[1] - w[0])
        .collect();
    assert!(
        gaps.len() == 2 && gaps[0] >= 900 && gaps[1] >= 1_900 && gaps[1] > gaps[0],
        "attempts must be monotonically spaced on the 1s/2s schedule, saw gaps {gaps:?}"
    );

    for handle in sink.lock().await.iter() {
        handle.abort();
    }
    client.disconnect().await.expect("http disconnect");
}

#[tokio::test]
async fn ac4_exhaustion_stops_after_five_total_attempts() {
    let server = FakeHttpMcpServer::start(FakeHttpConfig {
        refuse_initialize_times: u32::MAX,
        ..FakeHttpConfig::healthy()
    })
    .await;
    let (client, _rx) = adapter(http_spec("exhausted", &server.url()));
    let sink = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    lazy_connect_all(vec![client.clone()], sink, Arc::new(AtomicBool::new(false))).await;

    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline
        && !matches!(
            client.state(),
            McpConnectionState::ConnectionFailed { attempts: 5, .. }
        )
    {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert_eq!(
        server.observations().initialize_posts,
        5,
        "the front-door attempt plus four retries is the complete envelope"
    );
    assert!(
        matches!(
            client.state(),
            McpConnectionState::ConnectionFailed { attempts: 5, .. }
        ),
        "terminal state must record the same five-attempt envelope: {:?}",
        client.state()
    );
}
// ─────────────────────────────────────────────────────────────────────────────
// AC5 — HTTP shutdown reaps no child and stays inside the NFR24 budget.
// ─────────────────────────────────────────────────────────────────────────────

/// The core AC5 clause, observed deterministically: an HTTP connect involves NO
/// `tokio::process::Child` at all, so the stdio-only reap path
/// (`task_transport.rs`: 2s grace, then `start_kill()` + `wait()`) is never
/// entered.
///
/// ⚑ The spec deliberately carries a `command` that CANNOT be spawned. If the
/// transport branch regressed and the stdio arm ran, `TaskGuardTransport::spawn`
/// would fail and `connect()` would error — so a successful connect *is* the
/// no-spawn observation, and it needs no `pgrep` (which sees every child of the
/// test binary, including the stdio fakes other tests in this target spawn
/// concurrently — an earlier version of this assertion was flaky for exactly
/// that reason).
///
/// ⚑ Ruling A5's mutant ① — "route the HTTP client through the child-reap
/// path" — is **structurally impossible to write**: `TaskGuardTransport::spawn`
/// is reachable only from the stdio arm. Per authoring-rules Rule 4 the
/// invariant is proven by this deterministic observation plus a teardown that
/// finishes well inside the 2s grace that path would have cost; the executed
/// AC1-3 mutant shows what a broken transport branch does instead.
#[tokio::test]
async fn ac5_http_shutdown_enters_no_child_reap_path() {
    let server = FakeHttpMcpServer::start(FakeHttpConfig {
        assign_session: true,
        ..FakeHttpConfig::healthy()
    })
    .await;
    let mut spec = http_spec("remote", &server.url());
    spec.command = Some("/nonexistent/binary-must-not-spawn".to_string());
    let (client, _rx) = adapter(spec);
    client
        .connect()
        .await
        .expect("an http spec must never reach the stdio spawn path, command or no command");

    let started = Instant::now();
    shutdown_all_clients(&[client.clone()]).await;
    let elapsed = started.elapsed();

    assert_eq!(client.state(), McpConnectionState::NotConnected);
    assert!(
        elapsed < Duration::from_secs(1),
        "an HTTP-only shutdown must not pay the stdio 2s child grace; took {elapsed:?}"
    );
    assert_eq!(
        server.observations().delete_requests,
        1,
        "the transport closed gracefully — it deleted its session rather than being killed"
    );
}

/// AC5's remaining clause: a server that accepts the session-teardown `DELETE`
/// and **never answers it** must not push shutdown past NFR24's 5s ceiling.
///
/// ⚑ MEASURED, and recorded because it changes what mutant ② is worth: the
/// ceiling here is doubly bounded — `shutdown_all_clients`' own 5s timeout
/// (`lifecycle.rs`) AND rmcp's internal 5s `SESSION_CLEANUP_TIMEOUT`
/// (`streamable_http_client.rs`). Removing the outer timeout therefore does NOT
/// turn this RED; the clause holds under either bound. That is a structural
/// fact about the transport, not an untested assertion.
#[tokio::test]
async fn ac5_a_server_that_never_answers_the_close_stays_inside_the_budget() {
    let server = FakeHttpMcpServer::start(FakeHttpConfig {
        assign_session: true,
        hang_delete: true,
        ..FakeHttpConfig::healthy()
    })
    .await;
    let (client, _rx) = adapter(http_spec("remote", &server.url()));
    client.connect().await.expect("http connect");

    let started = Instant::now();
    shutdown_all_clients(&[client.clone()]).await;
    let elapsed = started.elapsed();

    assert!(
        elapsed <= Duration::from_millis(5_500),
        "NFR24: a server that never answers the close must not extend shutdown past 5s; \
         took {elapsed:?}"
    );
    // ⚑ MEASURED: the state is NOT reset on this path. `shutdown_all_clients`'
    // 5s ceiling DROPS the in-flight `disconnect()` future, so the
    // `set_state(NotConnected)` at its tail never runs. Harmless — the process
    // is exiting — but pinned here so a future reader does not mistake a
    // guillotined teardown for a graceful one.
    assert!(
        matches!(client.state(), McpConnectionState::Connected { .. }),
        "expected the last observed state to survive the guillotine, got {:?}",
        client.state()
    );
}

/// Positive control: a healthy pair must be bounded by the slower single
/// client, not by their sum.
#[tokio::test]
async fn ac5_shutdown_of_a_healthy_pair_is_bounded_by_the_slower_client() {
    let server = FakeHttpMcpServer::start(FakeHttpConfig::healthy()).await;
    let (http_client, _rx) = adapter(http_spec("remote", &server.url()));
    let (stdio_client, _stdio_rx) = adapter(stdio_spec("local"));
    http_client.connect().await.expect("http connect");
    stdio_client.connect().await.expect("stdio connect");

    let started = Instant::now();
    shutdown_all_clients(&[http_client.clone(), stdio_client.clone()]).await;
    let elapsed = started.elapsed();

    assert_eq!(http_client.state(), McpConnectionState::NotConnected);
    assert_eq!(stdio_client.state(), McpConnectionState::NotConnected);
    assert!(
        elapsed < Duration::from_secs(5),
        "NFR24: the whole fan-out is bounded by 5s, took {elapsed:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// AC6 — three distinct, actionable failure strings, no credential in any.
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn ac6_unreachable_auth_and_server_error_are_three_distinct_classes() {
    // (a) unreachable — a stable endpoint accepts and immediately resets.
    let refusing = FakeHttpMcpServer::refusing_endpoint().await;
    let (refused, _rx) = adapter(http_spec("remote", &refusing.url()));
    let refused_error = refused
        .connect()
        .await
        .expect_err("a refused dial must error");

    // (b) 401 + WWW-Authenticate.
    let auth_server = FakeHttpMcpServer::start(FakeHttpConfig {
        init: InitBehaviour::Unauthorized,
        ..FakeHttpConfig::healthy()
    })
    .await;
    let (auth, _rx) = adapter(http_spec("remote", &auth_server.url()));
    let auth_error = auth.connect().await.expect_err("401 must error");

    // (c) 5xx.
    let broken_server = FakeHttpMcpServer::start(FakeHttpConfig {
        init: InitBehaviour::ServerError,
        ..FakeHttpConfig::healthy()
    })
    .await;
    let (broken, _rx) = adapter(http_spec("remote", &broken_server.url()));
    let broken_error = broken.connect().await.expect_err("500 must error");

    assert!(
        matches!(
            &refused_error,
            McpError::Http {
                kind: HttpFailureKind::Unreachable,
                local: true,
                ..
            }
        ),
        "a refused loopback dial is Unreachable on OUR box, got {refused_error:?}"
    );
    assert!(
        matches!(
            &auth_error,
            McpError::Http {
                kind: HttpFailureKind::AuthRequired,
                ..
            }
        ),
        "401 must derive from rmcp's AuthRequired, got {auth_error:?}"
    );
    assert!(
        auth_error.to_string().contains("WWW-Authenticate")
            && auth_error.to_string().contains("Bearer"),
        "the auth string must carry the challenge the server sent, got {auth_error}"
    );
    assert!(
        matches!(
            &broken_error,
            McpError::Http {
                kind: HttpFailureKind::ServerError,
                ..
            }
        ),
        "500 must classify as ServerError, got {broken_error:?}"
    );

    // Distinctness is asserted on what the OPERATOR sees, not on the enum.
    let summaries: Vec<(String, Option<&'static str>)> = [&refused, &auth, &broken]
        .iter()
        .map(|client| {
            let summary = client.health_summary();
            (summary.metric, summary.suggested_action)
        })
        .collect();
    for (metric, action) in &summaries {
        assert!(
            action.is_some(),
            "every failure must carry an action: {metric}"
        );
    }
    let metrics: std::collections::BTreeSet<&String> = summaries.iter().map(|s| &s.0).collect();
    assert_eq!(
        metrics.len(),
        3,
        "the three classes must not collapse onto one string: {summaries:?}"
    );
    let actions: std::collections::BTreeSet<Option<&'static str>> =
        summaries.iter().map(|s| s.1).collect();
    assert_eq!(
        actions.len(),
        3,
        "each class must send the operator somewhere different: {summaries:?}"
    );
}

/// Mutant ②: interpolate `expose_url()` into any surfaced string and this turns
/// RED. `RedactedUrl`'s `Display` strips userinfo; `expose_url()` is for the
/// connect call and nothing else (A8).
///
/// ⚑ The URL is deliberately NON-loopback, so the D2 notice fires too. An
/// earlier version of this test used a loopback URL and a real `expose_url()`
/// interpolation into the notice ESCAPED it: the mutant poisoned a string the
/// test never reached. Every surface that can name the URL is drained here —
/// the error, the state, the panel metric, the health summary, and the notice.
#[tokio::test]
async fn ac6_no_surfaced_string_leaks_the_url_credential() {
    let refusing = FakeHttpMcpServer::refusing_endpoint().await;
    let with_credential = refusing
        .url()
        .replace("127.0.0.1", "0.0.0.0")
        .replace("http://", "http://admin:hunter2@");
    let (client, mut rx) = adapter(http_spec("credential-surface", &with_credential));
    let error = client
        .connect()
        .await
        .expect_err("a refused dial must error");

    let mut surfaced = vec![error.to_string(), client.health_summary().metric];
    if let McpConnectionState::ConnectionFailed { last_error, .. } = client.state() {
        surfaced.push(last_error);
    }
    surfaced.push(client.state().metric());
    let events = drain(&mut rx);
    assert!(
        !notices(&events).is_empty(),
        "the non-loopback D2 notice must be among the surfaces under test — without it \
         an expose_url() leak into the notice escapes this assertion"
    );
    for event in events {
        surfaced.push(format!("{event:?}"));
    }

    for text in &surfaced {
        assert!(
            !text.contains("hunter2"),
            "a credential reached an operator-facing string: {text}"
        );
    }
}

/// The timed-out half of AC6(a): a host that accepts the connection and then
/// answers nothing is *unreachable*, not a nameless timeout.
#[tokio::test]
async fn ac6_a_hung_server_classifies_as_unreachable_not_as_a_bare_timeout() {
    let server = FakeHttpMcpServer::start(FakeHttpConfig {
        init: InitBehaviour::Hang,
        ..FakeHttpConfig::healthy()
    })
    .await;
    let (client, _rx) = adapter(http_spec("remote", &server.url()));
    let error = client
        .connect()
        .await
        .expect_err("a hung server must error");
    assert!(
        matches!(
            &error,
            McpError::Http {
                kind: HttpFailureKind::Unreachable,
                local: true,
                ..
            }
        ),
        "got {error:?}"
    );
}

#[tokio::test]
async fn ac6_tools_list_failures_keep_http_classification() {
    for (list, expected) in [
        (ListBehaviour::Unauthorized, HttpFailureKind::AuthRequired),
        (ListBehaviour::ServerError, HttpFailureKind::ServerError),
    ] {
        let server = FakeHttpMcpServer::start(FakeHttpConfig {
            list,
            ..FakeHttpConfig::healthy()
        })
        .await;
        let (client, _rx) = adapter(http_spec("remote", &server.url()));
        let error = client
            .connect()
            .await
            .expect_err("tools/list failure must fail the complete connect");
        assert!(
            matches!(
                error,
                McpError::Http {
                    kind,
                    local: true,
                    ..
                } if kind == expected
            ),
            "tools/list must preserve {expected:?}, got {error:?}"
        );
        assert!(matches!(
            client.state(),
            McpConnectionState::ConnectionFailed { .. }
        ));
    }
}

#[tokio::test]
async fn ac6_configured_bearer_token_reaches_the_http_server() {
    let server = FakeHttpMcpServer::start(FakeHttpConfig::healthy()).await;
    let key = auth_token_env_for("authenticated");
    let previous = std::env::var_os(&key);
    // SAFETY: the per-server key is unique to this test and restored on drop.
    unsafe { std::env::set_var(&key, "integration-secret") };
    let _env_guard = scopeguard::guard((key, previous), |(key, previous)| match previous {
        Some(value) => unsafe { std::env::set_var(key, value) },
        None => unsafe { std::env::remove_var(key) },
    });
    let (client, _rx) = adapter(http_spec("authenticated", &server.url()));
    client.connect().await.expect("authenticated HTTP connect");
    let authorization = server.observations().presented_authorization;
    assert!(
        !authorization.is_empty()
            && authorization
                .iter()
                .all(|value| value == "Bearer integration-secret"),
        "every HTTP request must carry the configured bearer token: {authorization:?}"
    );
    client.disconnect().await.expect("disconnect");
}

// ─────────────────────────────────────────────────────────────────────────────
// AC7 — session-identity independence, over a real HTTP capture.
// ─────────────────────────────────────────────────────────────────────────────

/// 🔴 The positive control comes FIRST: "identity survived a session-id
/// rotation" is trivially true if no rotation happened. The fixture proves,
/// independently of rustain, that it served two DIFFERENT session tokens in this
/// run — then, and only then, the identity assertion means something.
///
/// The rotation is real, not simulated: the server answers a session-attached
/// POST with `404`, which drives rmcp's own `reinit_on_expired_session` path and
/// makes it ask for a new session.
#[tokio::test]
async fn ac7_identity_survives_a_real_session_rotation() {
    let server = FakeHttpMcpServer::start(FakeHttpConfig {
        assign_session: true,
        rotate_after_calls: Some(1),
        ..FakeHttpConfig::healthy()
    })
    .await;
    let (client, _rx) = adapter(http_spec("remote", &server.url()));
    client.connect().await.expect("http connect");

    let before = mint_mcp_node_id("remote", "task-42");

    client
        .call_tool(
            "echo",
            serde_json::json!({"text": "one"}),
            CancellationToken::new(),
        )
        .await
        .expect("first call succeeds");
    // This one is answered 404 → rmcp re-initializes and replays it.
    client
        .call_tool(
            "echo",
            serde_json::json!({"text": "two"}),
            CancellationToken::new(),
        )
        .await
        .expect("the replayed call succeeds after re-initialization");

    let observed = server.observations();
    assert!(
        observed.issued_session_tokens.len() >= 2,
        "POSITIVE CONTROL: the fixture must actually have rotated — it issued {:?}",
        observed.issued_session_tokens
    );
    assert_ne!(
        observed.issued_session_tokens[0], observed.issued_session_tokens[1],
        "the two session tokens must differ, or nothing rotated"
    );
    assert!(
        observed.initialize_posts >= 2,
        "a re-initialization must have happened: {observed:?}"
    );

    let after = mint_mcp_node_id("remote", "task-42");
    assert_eq!(
        before, after,
        "node identity must be (server, taskId) alone — a rotated session must not move it"
    );

    // And across two INDEPENDENTLY constructed clients (17.5a's keystone, now
    // re-run over the HTTP transport).
    let (second, _rx2) = adapter(http_spec("remote", &server.url()));
    second.connect().await.expect("second http connect");
    assert_eq!(
        mint_mcp_node_id("remote", "task-42"),
        before,
        "two independently constructed clients must mint the same node id"
    );

    second.disconnect().await.expect("disconnect second");
    client.disconnect().await.expect("disconnect first");
}

/// Mutant ②/FR147: a stateless server that never sends a session header at all
/// must behave identically — `allow_stateless` is rmcp's default and FR147
/// forbids `Mcp-Session-Id` assumptions.
#[tokio::test]
async fn ac7_a_stateless_server_that_issues_no_session_works_unchanged() {
    let server = FakeHttpMcpServer::start(FakeHttpConfig {
        assign_session: false,
        ..FakeHttpConfig::healthy()
    })
    .await;
    let (client, _rx) = adapter(http_spec("remote", &server.url()));
    client.connect().await.expect("stateless http connect");
    let result = client
        .call_tool(
            "add",
            serde_json::json!({"a": 2, "b": 3}),
            CancellationToken::new(),
        )
        .await
        .expect("stateless tool call");
    assert_eq!(result.content, "5");
    assert!(
        server.observations().issued_session_tokens.is_empty(),
        "the fixture must have issued no session token in this leg"
    );
    client.disconnect().await.expect("disconnect");
}

// ─────────────────────────────────────────────────────────────────────────────
// AC8 — a task-shaped reply over HTTP fails CLOSED with a named boundary.
// ─────────────────────────────────────────────────────────────────────────────

/// ⚑ T0.3(2), MEASURED: before this guard existed, a task-shaped `tools/call`
/// reply over HTTP landed in `call_tool`'s `_other` arm and the operator got
/// `CallToolFailed("unexpected server result type")` — opaque, and one
/// field-order change away from decoding silently into `CallToolResult`. Over
/// HTTP `guard_response` never runs, so rmcp's untagged `ServerResult` union
/// matches the flat task record against its SUPERSEDED task variants.
#[tokio::test]
async fn ac8_task_shaped_http_reply_fails_closed_naming_the_boundary() {
    let server = FakeHttpMcpServer::start(FakeHttpConfig {
        call: CallBehaviour::TaskShaped,
        ..FakeHttpConfig::healthy()
    })
    .await;
    let (client, _rx) = adapter(http_spec("remote", &server.url()));
    client.connect().await.expect("http connect");

    let error = client
        .call_tool(
            "echo",
            serde_json::json!({"text": "hi"}),
            CancellationToken::new(),
        )
        .await
        .expect_err("a task-shaped HTTP reply must be refused, not materialized");

    let message = error.to_string();
    assert!(
        matches!(error, McpError::Unsupported(_)),
        "the D1 boundary rides McpError::Unsupported, got {error:?}"
    );
    assert!(
        message.contains("stdio") && message.contains("remote") && message.contains("http"),
        "the refusal must name the boundary, the server and its transport: {message}"
    );
    client.disconnect().await.expect("disconnect");
    assert_eq!(
        server.observations().call_tasks_advertised,
        vec![false],
        "HTTP tools/call must not advertise the Tasks capability it refuses"
    );
}

#[tokio::test]
async fn ac8_hybrid_task_reply_cannot_hide_behind_call_tool_content() {
    let server = FakeHttpMcpServer::start(FakeHttpConfig {
        call: CallBehaviour::TaskShapedWithContent,
        ..FakeHttpConfig::healthy()
    })
    .await;
    let (client, _rx) = adapter(http_spec("hybrid", &server.url()));
    client.connect().await.expect("http connect");

    let error = client
        .call_tool(
            "echo",
            serde_json::json!({"text": "hi"}),
            CancellationToken::new(),
        )
        .await
        .expect_err("content must not mask a task-shaped HTTP response");
    assert!(
        matches!(error, McpError::Unsupported(_)),
        "raw response guard must preserve and refuse the task shape: {error:?}"
    );
    client.disconnect().await.expect("disconnect");
}
/// Positive control: the SAME task-shaped reply over **stdio** is still
/// accepted, proving the guard is transport-scoped rather than a blanket kill of
/// the Tasks path. Mutant: drop the `!= Stdio` condition from the
/// `materialize_task` guard and this turns RED (as do 17.5a's task keystones).
#[tokio::test]
async fn ac8_positive_control_a_task_shaped_reply_over_stdio_is_not_refused() {
    let (client, _rx) = adapter(stdio_spec("local"));
    client.connect().await.expect("stdio connect");

    client
        .send_custom_request(
            "test/control/arm",
            serde_json::json!({"target": "echo", "remaining": 1, "scenario": "progress"}),
        )
        .await
        .expect("arming the stdio fake for a task reply");

    let result = client
        .call_tool(
            "echo",
            serde_json::json!({"text": "hi"}),
            CancellationToken::new(),
        )
        .await
        .expect("stdio task replies must still be accepted");
    assert!(
        result.content.contains("created task"),
        "with no task runtime wired the stdio path degrades to a descriptive text result \
         (never the D1 refusal); got {:?}",
        result.content
    );
    client.disconnect().await.expect("disconnect");
}

// ─────────────────────────────────────────────────────────────────────────────
// AC9 — D2: a non-loopback plaintext URL warns exactly once and proceeds.
// ─────────────────────────────────────────────────────────────────────────────

/// 🔴 Positive control first: the notice must be OBSERVABLE on the production
/// event channel. AC9's other two assertions are both silence assertions, and
/// silence is indistinguishable from a notice mechanism wired to nothing —
/// which is exactly the A10 defect this AC repairs (`emit_transport_warnings`
/// claimed a `SystemNotice` for four months and only ever logged).
///
/// Mutant ①: drop the once-guard and the second connect emits a second notice.
/// Mutant ③: emit only `tracing::warn!` and this turns RED.
#[tokio::test]
async fn ac9_non_loopback_plaintext_warns_exactly_once_and_still_proceeds() {
    // `0.0.0.0` is non-loopback by the shared predicate; the port is dead, so
    // the dial fails fast. The notice fires BEFORE the dial — warn-and-allow
    // means the connection is attempted, not refused.
    let refusing = FakeHttpMcpServer::refusing_endpoint().await;
    let non_loopback = refusing.url().replace("127.0.0.1", "0.0.0.0");
    let spec = http_spec("warning-reconstruction", &non_loopback);
    let (first_client, mut first_rx) = adapter(spec.clone());
    let first = first_client.connect().await;
    drop(first_client);
    let (second_client, mut second_rx) = adapter(spec);
    let second = second_client.connect().await;
    assert!(
        first.is_err() && second.is_err(),
        "the dial was ATTEMPTED (warn-and-allow), it just had nowhere to land"
    );

    let emitted = notices(
        &drain(&mut first_rx)
            .into_iter()
            .chain(drain(&mut second_rx))
            .collect::<Vec<_>>(),
    );
    assert_eq!(
        emitted.len(),
        1,
        "exactly one notice per server for the process lifetime, got {emitted:?}"
    );
    assert_eq!(emitted[0].0, NoticeLevel::Warning);
    assert!(
        emitted[0].1.contains("plaintext") && emitted[0].1.contains("warning-reconstruction"),
        "the notice must name the condition and the server: {:?}",
        emitted[0].1
    );
}

#[tokio::test]
async fn ac9_loopback_and_https_are_silent() {
    let server = FakeHttpMcpServer::start(FakeHttpConfig::healthy()).await;
    let (loopback, mut loopback_rx) = adapter(http_spec("local", &server.url()));
    loopback.connect().await.expect("loopback http connect");
    assert!(
        notices(&drain(&mut loopback_rx)).is_empty(),
        "a loopback URL must produce no notice"
    );
    loopback.disconnect().await.expect("disconnect");

    let refusing = FakeHttpMcpServer::refusing_endpoint().await;
    let https_non_loopback = refusing
        .url()
        .replace("127.0.0.1", "0.0.0.0")
        .replace("http://", "https://");
    let (secure, mut secure_rx) = adapter(http_spec("secure", &https_non_loopback));
    let _ = secure.connect().await;
    assert!(
        notices(&drain(&mut secure_rx)).is_empty(),
        "https produces no notice regardless of host"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// AC10 — SSE is still Unsupported, and the HTTP guard no longer catches it.
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn ac10_sse_is_still_unsupported_while_http_connects() {
    let mut spec = http_spec("legacy", "http://127.0.0.1:1/sse");
    spec.transport = McpTransport::Sse;
    let (client, _rx) = adapter(spec);
    let error = client
        .connect()
        .await
        .expect_err("SSE must still be refused");
    assert!(
        matches!(&error, McpError::Unsupported(reason) if reason.contains("ADR-06-08")),
        "SSE keeps its existing reason string, got {error:?}"
    );
    assert!(matches!(
        client.state(),
        McpConnectionState::Unsupported { .. }
    ));
    assert!(
        !error.to_string().contains("deferred"),
        "⛔ the 'http transport deferred to a later Epic 9 story' string must be gone — \
         this story IS that story"
    );
}
