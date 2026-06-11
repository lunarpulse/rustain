//! MCP lifecycle helpers — connect, disconnect, shutdown.

use crate::adapters::mcp::client::McpClientAdapter;
use crate::adapters::mcp::error::McpError;
use futures::future::join_all;
use std::sync::Arc;
use tokio::time::{Duration, timeout};

/// Gracefully shutdown an MCP client: cancel token, close rmcp service,
/// wait up to 2s for child exit, then force kill.
pub async fn shutdown_client(client: &McpClientAdapter) -> Result<(), McpError> {
    let ct = client.cancel_token();
    ct.cancel();

    client.disconnect().await
}

/// Shutdown all MCP clients in parallel with a 5s overall timeout (AC-6).
///
/// Each client is shut down concurrently. The total time is bounded by the
/// slowest single client, NOT the sum (per spec AC-6).
pub async fn shutdown_all_clients(clients: &[Arc<McpClientAdapter>]) {
    if clients.is_empty() {
        return;
    }

    let overall_timeout = Duration::from_secs(5);

    let result = timeout(overall_timeout, async {
        let futures: Vec<_> = clients.iter().map(|c| shutdown_client(c)).collect();
        let results = join_all(futures).await;

        for (i, result) in results.iter().enumerate() {
            if let Err(e) = result {
                tracing::warn!(server = %clients[i].server_id(), error = %e, "MCP client shutdown failed");
            }
        }
    })
    .await;

    if result.is_err() {
        tracing::warn!("MCP shutdown timeout — force-killed remaining clients");
    }
}
