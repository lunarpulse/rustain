//! Usage ledger port — append-only token-usage tracking.
//!
//! Contract: `append()` writes a single `UsageLedgerEntry` atomically.
//! The adapter is responsible for serialization and persistence.
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
}
