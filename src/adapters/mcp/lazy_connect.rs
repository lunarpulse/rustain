//! Lazy-connect entry point for MCP servers after first frame.
//!
//! Called from `event_loop.rs::run` via a one-line helper to stay within
//! the event_loop.rs line budget (Story 8.5, Task 13.2).

use crate::adapters::mcp::client::McpClientAdapter;
use crate::adapters::mcp::reconnect::spawn_reconnect_task;
use futures::future::join_all;
use std::sync::Arc;

/// Connect all MCP clients in parallel.
///
/// This is spawned as a detached `tokio::spawn` so it does not block
/// the first frame (NFR10 < 100ms to first frame).
/// On failure, spawns a reconnect task for that server.
pub async fn lazy_connect_all(clients: Vec<Arc<McpClientAdapter>>) {
    if clients.is_empty() {
        return;
    }

    let results = join_all(clients.iter().map(|c| c.connect())).await;

    for (i, result) in results.iter().enumerate() {
        match result {
            Ok(()) => {
                tracing::info!(server = %clients[i].server_id(), "MCP server connected");
            }
            Err(e) => {
                tracing::warn!(server = %clients[i].server_id(), error = %e, "MCP server connection failed — spawning reconnect task");
                spawn_reconnect_task(clients[i].clone());
            }
        }
    }
}
