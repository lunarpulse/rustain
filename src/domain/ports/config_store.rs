//! Config store port — domain-only trait for reading and atomically swapping
//! the active `AppConfig`. Story 8.1 AC-7 + AC-14.
//!
//! The `Arc<ArcSwap<AppConfig>>` newtype in `infrastructure/` implements this
//! trait so that handler modules in `adapters/tui/handlers/` can reference the
//! config store without importing `crate::infrastructure::*` (AC-14).

use std::sync::Arc;

use crate::domain::models::AppConfig;

pub trait ConfigStorePort: Send + Sync {
    /// Return the current `AppConfig` snapshot (lock-free read via `ArcSwap::load`).
    fn load(&self) -> Arc<AppConfig>;

    /// Atomically swap in a new `AppConfig`.
    fn store(&self, config: AppConfig);
}
