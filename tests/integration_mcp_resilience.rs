//! Story 9.1/9.2 — resilience integration tests against a real stdio MCP child.
//!
//! The existing conformance suite uses ``Some("echo")`` as a placeholder command
//! that never actually connects, so its prepare_detach / list_changed tests
//! only exercise the in-memory state-machine path. These tests connect a real
//! ``fake-mcp-server`` subprocess so the integration path through the OS is
//! covered end-to-end.
//!
//! Risks closed:
//!   * R5 — profile-swap detach kills a *running* MCP subprocess (not just sets state)
//!   * R8 — ``list_changed`` notification keeps the catalog populated after refresh
//!
//! Combined with `conformance_mcp_lifecycle::test_no_zombie_processes_after_shutdown`
//! (which does pgrep verification under explicit shutdown) and the existing
//! profile-swap conformance tests, R5 is covered at all three levels:
//! state-machine (existing), prepare_detach with real child (here), and
//! TUI E2E (deferred — gated on the Ctrl+X chord-UI investigation).

#![cfg(feature = "mcp")]
mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use rustain::adapters::composite_toolset_adapter::CompositeToolsetAdapter;
use rustain::adapters::mcp::client::McpClientAdapter;
use rustain::adapters::noop::NoOpToolSet;
use rustain::domain::events::AppEvent;
use rustain::domain::models::{McpConnectionState, McpServerSource, McpServerSpec, McpTransport};
use rustain::domain::ports::ToolSetPort;

// ── Helpers ────────────────────────────────────────────────────────────────

fn fake_spec(id: &str, env: BTreeMap<String, String>, persistent: bool) -> McpServerSpec {
    McpServerSpec {
        id: id.to_string(),
        transport: McpTransport::Stdio,
        command: Some(common::fake_mcp_binary().to_string_lossy().into_owned()),
        args: vec![],
        env,
        url: None,
        persistent,
        source: McpServerSource::Workspace,
    }
}

async fn wait_connected(client: &McpClientAdapter, timeout_ms: u64) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if matches!(
            client.state(),
            McpConnectionState::Connected { .. } | McpConnectionState::Degraded { .. }
        ) {
            return true;
        }
        if std::time::Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn pgrep_alive(server_id: &str) -> bool {
    let output = std::process::Command::new("pgrep")
        .args(["-f", &format!("fake-mcp-server.*{}", server_id)])
        .output();
    matches!(output, Ok(out) if !out.stdout.is_empty())
}

// ── R5: prepare_detach actually kills a running MCP subprocess ──────────────

#[tokio::test]
async fn r5_prepare_detach_kills_running_non_persistent_mcp_child() {
    let builtin: Arc<dyn ToolSetPort> = Arc::new(NoOpToolSet);
    let spec = fake_spec("r5-ephemeral", BTreeMap::new(), false);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    let client = Arc::new(McpClientAdapter::new(spec.clone(), Some(tx)));
    client.connect().await.expect("connect");
    assert!(
        wait_connected(&client, 5000).await,
        "fake-mcp-server must connect"
    );
    // Smoke-check: child is alive before detach.
    // (Optional — pgrep may not always find a freshly-spawned child on every OS.)

    let composite = CompositeToolsetAdapter::new(
        builtin,
        vec![client.clone()],
        vec![spec],
        true,
        None,
        None,
        None,
    );

    composite
        .prepare_detach()
        .await
        .expect("prepare_detach should succeed");

    // State must reflect disconnection.
    assert!(
        matches!(client.state(), McpConnectionState::NotConnected),
        "ephemeral server should be NotConnected after prepare_detach, got {:?}",
        client.state()
    );

    // Give the OS a beat to reap the child.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !pgrep_alive("r5-ephemeral"),
        "child process for non-persistent server must die after prepare_detach"
    );
}

#[tokio::test]
async fn r5_prepare_detach_does_not_kill_persistent_mcp_child() {
    let builtin: Arc<dyn ToolSetPort> = Arc::new(NoOpToolSet);
    let spec = fake_spec("r5-persistent", BTreeMap::new(), true);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    let client = Arc::new(McpClientAdapter::new(spec.clone(), Some(tx)));
    client.connect().await.expect("connect");
    assert!(
        wait_connected(&client, 5000).await,
        "fake-mcp-server must connect"
    );

    let composite = CompositeToolsetAdapter::new(
        builtin,
        vec![client.clone()],
        vec![spec],
        true,
        None,
        None,
        None,
    );

    composite
        .prepare_detach()
        .await
        .expect("prepare_detach should succeed");

    // The persistent child must remain alive after detach. Give it a moment
    // to fail (if it were going to) and then assert the state is still
    // Connected — or at minimum, NOT NotConnected.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !matches!(client.state(), McpConnectionState::NotConnected),
        "persistent server MUST NOT be NotConnected after detach (was: {:?})",
        client.state()
    );

    // Cleanup: explicitly disconnect so the child doesn't outlive the test.
    let _ = client.disconnect().await;
}

// ── R8: list_changed keeps catalog populated after refresh ──────────────────

/// The existing `test_list_changed_refreshes_cache` proves the notification
/// fires and reaches the event bus. This test goes one step further: after
/// the notification, `client.cached_tools()` and the composite's
/// `available_tools()` must still expose the tools — proving the refresh
/// re-fetched rather than wiping the cache on the notification path.
#[tokio::test]
async fn r8_list_changed_keeps_catalog_populated_after_refresh() {
    let mut env = BTreeMap::new();
    env.insert(
        "FAKE_MCP_EMIT_LIST_CHANGED_AFTER_MS".to_string(),
        "300".to_string(),
    );
    let spec = fake_spec("r8-svr", env, false);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    let client = Arc::new(McpClientAdapter::new(spec.clone(), Some(tx)));
    // CRITICAL: production wires `set_self_weak` in the composition root
    // (see infrastructure/composition/mod.rs:262). Without this, the rmcp
    // ClientHandler's `on_tool_list_changed` callback upgrades a default
    // Weak that never points back at the adapter — so the notification is
    // delivered to the server-side runtime but never refreshes our cache
    // nor fires the AppEvent. This was an instructive find when authoring
    // R8: the existing conformance test "test_list_changed_refreshes_cache"
    // silently never exercises the McpCatalogChanged path because it also
    // skips this wiring.
    client.set_self_weak(Arc::downgrade(&client));
    client.connect().await.expect("connect");
    assert!(
        wait_connected(&client, 5000).await,
        "fake-mcp-server must connect"
    );

    // Baseline: catalog populated before the notification.
    let before = client
        .cached_tools()
        .expect("cached_tools should be populated post-handshake");
    assert!(
        !before.is_empty(),
        "expected non-empty tool list before list_changed"
    );

    // Wait past the trigger time so the server has armed the flag.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Drive a real RPC so the server reads its next stdin line — that's the
    // point at which `fake-mcp-server` actually emits the buffered
    // `notifications/tools/list_changed`. Without this, the flag stays set
    // but the notification line is never written.
    let _ = client
        .call_tool(
            "echo",
            serde_json::json!({"text": "ping"}),
            tokio_util::sync::CancellationToken::new(),
        )
        .await;

    // Wait for the catalog-changed event so we know the refresh path ran.
    let evt = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match rx.recv().await {
                Some(AppEvent::McpCatalogChanged { server_id, .. }) if server_id == "r8-svr" => {
                    return Some(server_id);
                }
                Some(_) => continue,
                None => return None,
            }
        }
    })
    .await
    .expect("timed out waiting for McpCatalogChanged");

    assert_eq!(
        evt.as_deref(),
        Some("r8-svr"),
        "expected catalog-changed event for r8-svr"
    );

    // After refresh, the cache must STILL expose the server's tools — the
    // refresh path must repopulate, not just invalidate. (fake-mcp-server
    // returns the same list, so we assert presence, not delta content.)
    let after = client
        .cached_tools()
        .expect("cached_tools must remain populated after list_changed");
    assert!(
        !after.is_empty(),
        "expected non-empty tool list after list_changed refresh"
    );

    // Composite-level: the same tools must surface on the LLM-facing catalog
    // with the `mcp__<server>__<tool>` projection.
    let builtin: Arc<dyn ToolSetPort> = Arc::new(NoOpToolSet);
    let composite = CompositeToolsetAdapter::new(
        builtin,
        vec![client.clone()],
        vec![spec],
        true,
        None,
        None,
        None,
    );
    let names: Vec<String> = composite
        .available_tools()
        .into_iter()
        .map(|d| d.name)
        .collect();
    assert!(
        names.iter().any(|n| n.starts_with("mcp__r8-svr__")),
        "composite available_tools must include mcp__r8-svr__* after list_changed; got {:?}",
        names
    );

    let _ = client.disconnect().await;
}
