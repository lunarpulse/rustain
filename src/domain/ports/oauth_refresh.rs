//! Refresh transport seam; HTTP classification belongs to the login engines.
use async_trait::async_trait;

use crate::domain::models::{CredentialIdentity, OAuthCredential, SecretString};

#[derive(Debug, Clone)]
pub struct RefreshedTokens {
    pub access: SecretString,
    pub refresh: Option<SecretString>,
    pub expires_at_ms: i64,
    pub identity: CredentialIdentity,
}

#[derive(Debug, Clone)]
pub enum RefreshFailure {
    Definitive(String),
    Transient(String),
}

#[async_trait]
pub trait OAuthRefreshPort: Send + Sync {
    async fn refresh(
        &self,
        provider: &str,
        credential: &OAuthCredential,
    ) -> Result<RefreshedTokens, RefreshFailure>;
}
