//! Usage ledger port — append-only token-usage tracking + read aggregation.
//!
//! Contract: `append()` writes a single `UsageLedgerEntry` atomically.
//! The adapter is responsible for serialization and persistence.
//!
//! Story 7.5 (AC9) added reader methods so the usage panel and daily-budget
//! recompute can aggregate prior entries on demand. Lines that fail to parse
//! are `tracing::warn!`-logged and skipped (graceful degradation — never error
//! the whole read).
//!
//! Flat-file JSONL v0 (Story 7.1c); sqlx persistence deferred to Story 10.9.

use async_trait::async_trait;

use crate::domain::errors::StorageError;
use crate::domain::models::usage::UsageLedgerEntry;

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

    /// Read every entry across all sessions newer than `since_unix_ms`
    /// (Story 7.5 AC9). Used for "today" aggregation in the usage panel and
    /// daily-budget recompute.
    ///
    /// Returns `Ok(vec![])` when the usage directory is empty or missing.
    async fn read_since(&self, since_unix_ms: i64) -> Result<Vec<UsageLedgerEntry>, StorageError>;
}
