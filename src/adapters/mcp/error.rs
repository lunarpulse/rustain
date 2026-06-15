//! MCP error types.

use thiserror::Error;

#[derive(Error, Debug, Clone)]
pub enum McpError {
    #[error("Failed to spawn MCP server process: {0}")]
    SpawnFailed(String),
    #[error("MCP initialize handshake failed: {0}")]
    HandshakeFailed(String),
    #[error("tools/list call failed: {0}")]
    ToolsListFailed(String),
    #[error("Child process exited unexpectedly: {0}")]
    ChildExited(String),
    #[error("Transport closed: {0}")]
    TransportClosed(String),
    #[error("timeout after {0}s")]
    Timeout(u64),
    #[error("Unsupported transport: {0}")]
    Unsupported(String),
    #[error("MCP tool call failed: {0}")]
    CallToolFailed(String),
    #[error("MCP tool call cancelled")]
    Cancelled,
    #[error("Internal error: {0}")]
    Internal(String),
}
