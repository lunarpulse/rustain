//! Budget-state persistence adapter (Story 7.5 AC7).
//!
//! Mirrors `adapters/ledger/` shape — single file-backed store, stateless,
//! resolves the path per call. Used to persist `BudgetPause` "dismissed until
//! tomorrow" decisions across rustain restarts.

pub mod file_store;

pub use file_store::{BudgetState, BudgetStateStore};
