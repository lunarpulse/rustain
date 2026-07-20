//! `ArtifactSink` — the narrow domain seam by which an MCP task driver files
//! an input-request artifact + ticket to the scarce human (17.5b / AC3,
//! FR148/FR149/FR152).
//!
//! Sibling of [`crate::domain::ports::RoomJournal`] (17.5a), following the same
//! hexagonal rule: `adapters/mcp` may NOT import `infrastructure/` (arch guard
//! #1, `src/adapters/mcp/mod.rs`), so the first artifact an adapter ever
//! produces rides a one-method port implemented at the composition root. The
//! impl supplies `authority` + `host` (orchestrator-only fields the adapter
//! cannot reach — see story Task 6 / C4) and journals `ArtifactCreated` +
//! `TicketAssigned` durably-first, bus-second.
//!
//! No dead methods (R-9): the single production caller is the MCP task driver's
//! `Waiting` transition.

use crate::domain::models::{AgentId, ArtifactId};

/// Failure surface for an artifact write reached through the seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactSinkError {
    /// The durable write (artifact store or room journal) failed. Carries a
    /// sanitized message.
    Write(String),
}

impl std::fmt::Display for ArtifactSinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Write(msg) => write!(f, "input-request artifact write failed: {msg}"),
        }
    }
}

impl std::error::Error for ArtifactSinkError {}

/// File an MCP elicitation as a durable `InputRequest` artifact plus a
/// `TicketAssigned` room event, across the adapter→infrastructure boundary.
///
/// `producer` is the task node; `node` is the same node the ticket is assigned
/// from. `body` is the raw elicitation request envelope (the `InputRequest`
/// `{method, params}` captured from `tasks/get`), stored verbatim as the
/// artifact content so the human sees exactly what was asked.
#[async_trait::async_trait]
pub trait ArtifactSink: Send + Sync {
    async fn write_input_request(
        &self,
        producer: &AgentId,
        node: &AgentId,
        body: serde_json::Value,
    ) -> Result<ArtifactId, ArtifactSinkError>;
}
