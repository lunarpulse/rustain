//! Exponential-backoff reconnect task for MCP servers.

use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::adapters::mcp::client::McpClientAdapter;

/// Spawn a reconnect task for a single MCP client.
///
/// Backoff schedule: 1s, 2s, 4s, 8s, 16s, 32s (capped at 32s).
/// Max 5 attempts per disconnect event.
pub fn spawn_reconnect_task(client: Arc<McpClientAdapter>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let max_attempts: u32 = 5;
        let ct = client.cancel_token();

        for attempt in 1..=max_attempts {
            if ct.is_cancelled() {
                return;
            }

            let backoff = Duration::from_millis(1000 * 2u64.pow((attempt - 1).min(5)));
            tokio::select! {
                _ = sleep(backoff) => {},
                _ = ct.cancelled() => return,
            }

            match client.connect().await {
                Ok(()) => {
                    tracing::info!(
                        server = %client.server_id(),
                        "MCP server reconnected (attempt {attempt}/{max_attempts})"
                    );
                    return;
                }
                Err(e) => {
                    tracing::warn!(
                        server = %client.server_id(),
                        error = %e,
                        "MCP server reconnect attempt {attempt}/{max_attempts} failed"
                    );
                }
            }
        }

        tracing::error!(
            server = %client.server_id(),
            "MCP server connection failed after {max_attempts} attempts. Use /mcp reconnect <name> to retry (Story 9.2) or restart rustain."
        );
    })
}
