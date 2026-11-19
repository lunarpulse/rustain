//! Usage ledger port — append-only token-usage tracking + read aggregation.
//!
//! Contract: `append()` writes a single `UsageLedgerEntry` atomically.
//! The adapter is responsible for serialization and persistence.
//!
//! Ordinary display readers log and skip corrupt lines. Budget admission can
//! instead ask for a strict snapshot of records in its accounting window.
//!
//! Flat-file JSONL v0 (Story 7.1c); sqlx persistence deferred to Story 10.9.

use async_trait::async_trait;

use crate::domain::errors::StorageError;
use crate::domain::models::usage::UsageLedgerEntry;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LedgerReadMode {
    Lenient,
    Strict { end_unix_ms: i64 },
}

#[async_trait]
pub trait UsageLedgerPort: Send + Sync {
    /// Append a usage entry to the ledger.
    ///
    /// Implementations must be atomic (one entry per call) and must not
    /// silently drop entries on error.
    async fn append(&self, entry: UsageLedgerEntry) -> Result<(), StorageError>;

    /// Read every entry for a given session (Story 7.5 AC9).
    ///
    /// Returns `Ok(vec![])` when the file doesn't exist (no entries yet).
    /// Lines that fail to parse are `tracing::warn!`-logged and skipped.
    async fn read_session(&self, session_id: &str) -> Result<Vec<UsageLedgerEntry>, StorageError>;

    /// Read entries across sessions starting at `since_unix_ms` (inclusive).
    ///
    /// Strict reads also exclude `end_unix_ms` and later, and require
    /// `end_unix_ms > since_unix_ms`. A missing directory is empty. Strict reads
    /// reject a malformed row unless a unique integer `timestampMs` in a JSON
    /// object proves it is outside the window, and reject any error reading a
    /// file already found in the directory.
    async fn read_since(
        &self,
        since_unix_ms: i64,
        mode: LedgerReadMode,
    ) -> Result<Vec<UsageLedgerEntry>, StorageError>;
}
