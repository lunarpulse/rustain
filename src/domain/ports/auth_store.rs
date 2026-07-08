//! Port trait for provider-credential lifecycle (Story 13.4a, architecture.md port #13).
//!
//! Sibling to `StoragePort` (not a widening of it) — credentials have distinct
//! at-rest requirements (0600 perms, masked display, fcntl-locked writes).

use async_trait::async_trait;

use crate::domain::errors::AuthError;
use crate::domain::models::credential::{Credential, ProviderStatus};

/// Outbound port for provider credentials.
///
/// Default adapter `FileAuthStore` persists to `~/.rustain/auth.json`.
/// Env vars remain the highest-priority auth source; `auth.json` is consulted
/// only when no env var is set (backward compatible).
#[async_trait]
pub trait AuthStorePort: Send + Sync {
    async fn get(&self, provider: &str) -> Result<Option<Credential>, AuthError>;
    async fn set(&self, provider: &str, cred: Credential) -> Result<(), AuthError>;
    /// Store a credential recording whether it was validated. Default delegates
    /// to `set` (stamping the current time). Adapters override to record
    /// `last_validated = None` when `validated` is false (spec Q1: an
    /// inconclusive `/models` probe must not look "validated"). Story 13.4a.
    async fn set_validated(
        &self,
        provider: &str,
        cred: Credential,
        _validated: bool,
    ) -> Result<(), AuthError> {
        self.set(provider, cred).await
    }
    async fn remove(&self, provider: &str) -> Result<(), AuthError>;
    async fn list(&self) -> Result<Vec<ProviderStatus>, AuthError>;
}
