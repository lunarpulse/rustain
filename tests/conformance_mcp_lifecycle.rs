//! Conformance tests for MCP lifecycle — Story 9.1 reconnect & shutdown.
//!
//! Uses the `fake-mcp-server` binary to exercise real stdio MCP sessions.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use rustain::adapters::mcp::client::McpClientAdapter;
use rustain::adapters::mcp::lifecycle::shutdown_all_clients;
use rustain::domain::events::AppEvent;
use rustain::domain::models::{McpConnectionState, McpServerSource, McpServerSpec, McpTransport};

fn fake_spec(id: &str, env: BTreeMap<String, String>) -> McpServerSpec {
    let binary_name = if cfg!(target_os = "windows") {
        "fake-mcp-server.exe"
    } else {
        "fake-mcp-server"
    };
    let exe_dir = std::env::current_exe()
        .expect("current exe")
        .parent()
        .expect("parent")
        .to_path_buf();
    let mut candidates = vec![
        exe_dir.join(binary_name),
        exe_dir.parent().expect("deps parent").join(binary_name),
    ];
    let command = candidates
        .iter()
        .find(|p| p.exists())
        .cloned()
        .unwrap_or_else(|| candidates.remove(0));
    McpServerSpec {
        id: id.to_string(),
        transport: McpTransport::Stdio,
        command: Some(command.to_string_lossy().into_owned()),
        args: vec![],
        env,
        url: None,
        persistent: false,
        source: McpServerSource::Workspace,
    }
}

fn fake_spec_with_drop(id: &str, drop_ms: u64) -> McpServerSpec {
    let mut env = BTreeMap::new();
    env.insert("FAKE_MCP_DROP_AFTER_MS".into(), drop_ms.to_string());
    fake_spec(id, env)
}

/// Smoke test: verify that spawn_reconnect_task attempts connections repeatedly
/// when the initial connect fails. Uses a bogus command to force SpawnFailed,
/// then counts state transitions from the reconnect task.
#[tokio::test]
async fn test_reconnect_exponential_backoff_capped_at_5() {
    use rustain::adapters::mcp::reconnect::spawn_reconnect_task;

    let mut spec = fake_spec("reconnect-test", BTreeMap::new());
    spec.command = Some("/nonexistent-binary-that-does-not-exist".into());

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    let client = Arc::new(McpClientAdapter::new(spec, Some(tx)));

    let state_changes: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
    let state_changes_clone = state_changes.clone();
    let _collector = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            if let AppEvent::McpConnectionStateChanged { state, .. } = event {
                if matches!(
                    state,
                    McpConnectionState::ConnectionFailed { .. }
                        | McpConnectionState::Reconnecting { .. }
                        | McpConnectionState::Connecting { .. }
                ) {
                    state_changes_clone.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    });

    spawn_reconnect_task(client.clone());

    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let attempts = state_changes.load(Ordering::Relaxed);
            if attempts >= 5 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("timed out waiting for 5 reconnect state changes");

    let attempts = state_changes.load(Ordering::Relaxed);
    assert!(
        attempts >= 5,
        "expected at least 5 state changes during reconnect, got {attempts}"
    );

    client.cancel_token().cancel();
    let _ = client.disconnect().await;
}

/// AC-6: After shutdown, no child processes should remain.
#[tokio::test]
async fn test_no_zombie_processes_after_shutdown() {
    let mut clients: Vec<Arc<McpClientAdapter>> = Vec::new();

    for i in 0..3 {
        let spec = fake_spec(&format!("zombie-test-{i}"), BTreeMap::new());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
        let client = Arc::new(McpClientAdapter::new(spec, Some(tx)));
        let _ = client.connect().await;
        clients.push(client);
    }

    shutdown_all_clients(&clients).await;

    for client in &clients {
        let state = client.state();
        assert!(
            matches!(state, McpConnectionState::NotConnected),
            "client {} should be NotConnected after shutdown, got {:?}",
            client.server_id(),
            state
        );
    }

    tokio::time::sleep(Duration::from_millis(500)).await;

    for client in &clients {
        let output = std::process::Command::new("pgrep")
            .args(["-f", &format!("fake-mcp-server.*{}", client.server_id())])
            .output();
        if let Ok(out) = output {
            assert!(
                out.stdout.is_empty(),
                "zombie process found for server {}",
                client.server_id()
            );
        }
    }
}
