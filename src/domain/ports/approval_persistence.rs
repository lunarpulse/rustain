//! Port trait for persisting approval rules.
//!
//! See ADR-06-01 for the canonical design.

use async_trait::async_trait;

use crate::domain::errors::ApprovalPersistenceError;
use crate::domain::models::ApprovalScope;
use crate::domain::services::approval_runtime::SessionApprovalSet;

#[async_trait]
pub trait ApprovalPersistencePort: Send + Sync {
    /// Load persisted always-allow rules at startup. Returns the seed set.
    async fn load(&self) -> Result<SessionApprovalSet, ApprovalPersistenceError>;

    /// Persist a single Always-and-Save scope. Atomic; concurrent-safe.
    /// Tool/Server scopes write to `~/.rustain/config.toml`.
    /// PathPrefix scope writes to `{workspace}/.rustain/permissions.toml`.
    async fn save(&self, scope: ApprovalScope) -> Result<(), ApprovalPersistenceError>;
}
