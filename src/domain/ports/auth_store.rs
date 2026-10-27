//! Provider credential lifecycle. Implementations serialize read-modify-write,
//! fence refresh writes by row identity, revision, and login generation, and
//! elect one refresher across processes (ADR-19-1-03 no-daemon path).

use async_trait::async_trait;

use crate::domain::errors::AuthError;
use crate::domain::models::credential::{
    AuthSnapshot, CasOutcome, Credential, CredentialId, CredentialOrigin, CredentialStamp,
};

/// Cross-process refresh election, held across one bounded refresh exchange so
/// two processes never spend the same rotating refresh token. Dropping the
/// lease ends the election.
pub struct RefreshLease {
    _guard: Option<Box<dyn std::any::Any + Send + Sync>>,
}

impl RefreshLease {
    /// For stores that no other process can share.
    pub fn unshared() -> Self {
        Self { _guard: None }
    }

    /// The election lasts as long as `guard` lives.
    pub fn held(guard: impl std::any::Any + Send + Sync) -> Self {
        Self {
            _guard: Some(Box::new(guard)),
        }
    }
}

#[async_trait]
pub trait AuthStorePort: Send + Sync {
    async fn snapshot(&self) -> Result<AuthSnapshot, AuthError>;
    async fn upsert(
        &self,
        provider: &str,
        cred: Credential,
        origin: CredentialOrigin,
        validated: bool,
    ) -> Result<CredentialId, AuthError>;
    async fn replace_if_matches(
        &self,
        provider: &str,
        expected: CredentialStamp,
        cred: Credential,
    ) -> Result<CasOutcome, AuthError>;
    async fn disable_if_matches(
        &self,
        provider: &str,
        expected: CredentialStamp,
        cause: &str,
    ) -> Result<CasOutcome, AuthError>;
    async fn remove(&self, provider: &str) -> Result<(), AuthError>;
    /// Waits until this caller is the only refresher of the store's OAuth rows.
    /// The default suits stores no other process shares; durable stores override it.
    async fn refresh_lease(&self) -> Result<RefreshLease, AuthError> {
        Ok(RefreshLease::unshared())
    }
}
