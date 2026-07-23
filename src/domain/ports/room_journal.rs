//! `RoomJournal` — the narrow domain seam by which `adapters/mcp` emits room
//! events durably WITHOUT holding a concrete `NodeJournal` (which lives in
//! `infrastructure/subagent/`).
//!
//! Story 17.5a (ADR-17-5-01, ruling D2). `architecture.md` forbids
//! `adapters/* → infrastructure/*`; the rule is documentary (no CI test
//! enforces it) and `a2a/driver.rs:37-38` is a known, unremediated violation.
//! This story copies 17.4b's driver *shape* but not its layering: the MCP
//! task driver reaches the journal only through this port, mirroring
//! `SupervisedNodes` (ADR-17-2c-01). The concrete implementation is
//! constructed only at the composition root (`startup.rs`).
//!
//! The port carries exactly the methods 17.5a code paths call — no dead
//! methods. Ordering is part of the contract: **durable-first, bus-second**
//! (journal append, then `AppEvent` emit), matching the canonical
//! `persist_room_event` helper (`orchestrator/mod.rs:1138-1149`).

use crate::domain::models::RoomEvent;

/// Failure surface for a room-event record reached through the seam. Kept in
/// `domain/` so the trait carries no infra error type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoomJournalError {
    /// The journal append failed. Carries a sanitized message. The bus emit
    /// is NOT attempted when the durable write fails (durable-first).
    Append(String),
}

impl std::fmt::Display for RoomJournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Append(msg) => write!(f, "room journal append failed: {msg}"),
        }
    }
}

impl std::error::Error for RoomJournalError {}

/// Durable room-event emission across a domain boundary.
#[async_trait::async_trait]
pub trait RoomJournal: Send + Sync {
    /// Append `event` to the room journal, then emit it on the domain-event
    /// bus — in that exact order. A journal failure returns `Err` and the
    /// event is never emitted to the bus.
    async fn record_event(&self, event: RoomEvent) -> Result<(), RoomJournalError>;
}
