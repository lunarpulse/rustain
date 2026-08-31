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
    #[error("Timeout after {0}s")]
    Timeout(u64),
    #[error("Unsupported transport: {0}")]
    Unsupported(String),
    #[error("MCP tool call failed: {0}")]
    CallToolFailed(String),
    #[error("MCP task protocol error: {0}")]
    TaskProtocol(String),
    #[error("MCP task failed: {0}")]
    TaskFailed(String),
    #[error("MCP tool call cancelled")]
    Cancelled,
    #[error("Internal error: {0}")]
    Internal(String),
    /// Story 9.9 (AC2) — a transport ↔ field inconsistency in the server's
    /// config entry: `transport = "http"` with no `url`, an unparseable `url`,
    /// `transport = "stdio"` with no `command`. Raised by the fail-closed gate
    /// in `McpClientAdapter::connect` so the fault surfaces as
    /// `ConnectionFailed { last_error }` in the adapter status panel instead of
    /// deleting the entry's healthy siblings at parse time (ruling A1/A17).
    #[error("MCP server configuration error: {0}")]
    InvalidConfig(String),
    /// Story 9.9 (AC6 / ruling A7) — a Streamable HTTP transport failure,
    /// classified so the operator gets a distinct, actionable string per class.
    ///
    /// `local` carries THE loopback computation. `rustain doctor`'s
    /// `map_connect_result` is a pure mapper over this error with no access to
    /// the URL, so tiering "whose box is it" is only possible if the predicate
    /// travels WITH the error. ⛔ One computation, two consumers (the D2 notice
    /// and the doctor tier) — never two checks that can disagree.
    #[error("MCP HTTP transport failed ({kind}): {detail}")]
    Http {
        kind: HttpFailureKind,
        local: bool,
        detail: String,
    },
}

/// The Streamable HTTP failure classes Story 9.9 AC6 requires to be distinct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpFailureKind {
    /// Connection refused, reset, or timed out before any HTTP response.
    Unreachable,
    /// The host name did not resolve — a typo in the operator's config.
    DnsFailure,
    /// `401` carrying a `WWW-Authenticate` challenge.
    AuthRequired,
    /// The server answered, and answered badly (`5xx`, or a protocol-level
    /// response the transport refused).
    ServerError,
}

impl std::fmt::Display for HttpFailureKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::Unreachable => "unreachable",
            Self::DnsFailure => "dns failure",
            Self::AuthRequired => "auth required",
            Self::ServerError => "server error",
        };
        f.write_str(text)
    }
}
