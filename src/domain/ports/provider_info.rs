//! Read/write port for provider catalog + active-delegate state.
//!
//! Established Story 8.0a Phase 2 (Task 3 — compaction prototype) per user
//! direction "per spec + long-term correctness" — handlers under
//! `src/adapters/tui/handlers/` must satisfy AC-5 domain isolation
//! (`rg 'use crate::infrastructure' handlers/` returns 0). To pass provider/
//! router state into a handler without an infrastructure import, the dispatch
//! arm constructs an `AppContext` (infrastructure-side wrapper) and passes it
//! as `&dyn ProviderInfoPort`.
//!
//! Implementations:
//! - `AppContext<'a>` in `infrastructure/runtime/app_context.rs` — wraps
//!   `&'a AppState` + `&'a ProviderRouter`.
//! - Future: `NoopProviderInfoPort` in `adapters/noop.rs` for testability
//!   (Phase 5 / Task 11 if test handlers need it).

#![allow(dead_code)]

use std::sync::Arc;

use crate::domain::errors::ProviderError;
use crate::domain::models::provider::{ModelDescriptor, ProviderDescriptor};
use crate::domain::ports::StreamingProvider;

/// Provider/model catalog lookup + active-delegate router operations,
/// abstracted so domain-isolated handler modules can consume them without
/// importing infrastructure or adapter types directly.
pub trait ProviderInfoPort: Send + Sync {
    /// ID of the currently active provider delegate (router state).
    fn active_delegate_id(&self) -> String;

    /// Look up model metadata in a specific provider's catalog. Returns `None`
    /// if the provider isn't registered or doesn't list the model.
    fn get_model(&self, provider_id: &str, model_id: &str) -> Option<ModelDescriptor>;

    /// Find which provider serves a given model ID, with optional `prefer`
    /// hint for deterministic resolution per Story 7-8 (BTreeMap-iteration +
    /// prefer-precedence).
    fn get_model_provider(&self, model_id: &str, prefer: Option<&str>) -> Option<String>;

    /// List all registered providers + their health status.
    fn list_providers(&self) -> Vec<ProviderDescriptor>;

    /// List models served by a specific provider.
    fn list_models_by_provider(&self, provider_id: &str) -> Vec<ModelDescriptor>;

    /// Get a clone-able handle to a specific provider's `StreamingProvider`
    /// impl. Used for spawn-bearing handlers (health-check, compaction) that
    /// pass the provider into a spawned task at the dispatch site.
    fn get_provider(&self, provider_id: &str) -> Option<Arc<dyn StreamingProvider>>;

    /// Set the active delegate provider. Returns `Err(ProviderError::Other)`
    /// if the provider isn't registered.
    fn set_active_provider(&self, provider_id: &str) -> Result<(), ProviderError>;

    /// Current wall-clock Unix timestamp in seconds. Delegates to
    /// `infrastructure::clock_util::now_unix()` at the impl site so
    /// handler modules stay free of infrastructure imports.
    fn now_unix(&self) -> i64;

    /// Start of today as a Unix timestamp in milliseconds. Delegates to
    /// `infrastructure::clock_util::today_start_unix_ms()` at the impl site.
    fn today_start_unix_ms(&self) -> i64;
}
