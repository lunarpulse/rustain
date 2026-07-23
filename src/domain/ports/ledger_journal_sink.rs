//! `LedgerJournalSink` — the thin domain seam by which the synchronous
//! `AuthorityLedger` records its conservation head to the durable journal
//! WITHOUT importing infrastructure or becoming async itself.
//!
//! Story 17.2c (discharges the 17-2b review's D4). Party ruling fork 2 + the
//! dev-story roundtable: the ledger stays a synchronous `std::sync::Mutex` leaf
//! lock (ADR-14-2-01 preserved). Durability lives OUTSIDE the lock — a mutation
//! stages a [`LedgerConservationRecord`] under the lock, and the async caller
//! flushes it through this port (write-ahead, before the side-effect is
//! observable) as its own single-record `JournalRecord::Batch`. This is a
//! recorder, not a parallel log; recovery replays the records idempotently
//! (latest snapshot per token wins). There is NO fire-and-forget background
//! drain — a drain window would let spent budget resurrect / a grant
//! double-count on recovery.

use crate::domain::models::LedgerConservationRecord;

/// Failure surface for a conservation-head flush. Domain-only so the trait
/// carries no infra error type; the infra impl maps its journal error in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerJournalError(pub String);

impl std::fmt::Display for LedgerJournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ledger conservation flush failed: {}", self.0)
    }
}

impl std::error::Error for LedgerJournalError {}

/// Durable recorder for the ledger's conservation head.
#[async_trait::async_trait]
pub trait LedgerJournalSink: Send + Sync {
    /// Append one conservation-head snapshot to the durable journal (its own
    /// single-record atomic batch, fsynced) so it survives a restart. Awaited
    /// by the caller OUTSIDE the ledger `Mutex`, before the causing mutation's
    /// side-effect becomes externally observable (write-ahead).
    async fn journal_conservation(
        &self,
        record: LedgerConservationRecord,
    ) -> Result<(), LedgerJournalError>;
}
