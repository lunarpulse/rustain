//! Authority token delegation port.
//!
//! Guard: this is deliberately separate from the discovery-side CapabilityProvider.
//! Discovery answers "what tools exist?"; authority answers "may this node do this now?".

use async_trait::async_trait;
use thiserror::Error;

use crate::domain::models::{
    AgentId, CapabilityFlag, CapabilityToken, CapabilityTokenId, DelegateRequest,
    JournaledTerminalCheckpoint,
};

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AuthorityError {
    #[error("non-subset authority delegation on {dimension}")]
    NonSubset { dimension: &'static str },
    #[error("max depth exceeded: limit {limit}, attempted {attempted}")]
    MaxDepthExceeded { limit: usize, attempted: usize },
    #[error("authority token expired")]
    Expired,
    #[error("authority budget exhausted")]
    BudgetExhausted,
    #[error("authority token revoked")]
    Revoked,
    #[error("authority capability not granted: {flag:?}")]
    Denied { flag: CapabilityFlag },
    #[error("authority token not found")]
    NotFound,
    #[error("authority token malformed: {reason}")]
    Malformed { reason: &'static str },
}

#[async_trait]
pub trait AuthorityProvider: Send + Sync {
    async fn delegate(
        &self,
        parent: &CapabilityToken,
        req: DelegateRequest,
    ) -> Result<CapabilityToken, AuthorityError>;

    async fn validate(
        &self,
        token: &CapabilityToken,
        want: &CapabilityFlag,
        scope: &AgentId,
    ) -> Result<(), AuthorityError>;

    async fn revoke(&self, token: &CapabilityTokenId) -> Result<(), AuthorityError>;

    /// Settle a delegated token on terminal (AC4): refund the unused
    /// reservation to the parent, idempotently. Synchronous in R1.
    async fn settle(&self, token: &CapabilityTokenId) -> Result<(), AuthorityError>;

    /// Reclaim a settled or revoked terminal grant only after the caller has
    /// obtained durable journal proof for the same node/token.
    async fn prune_terminal(
        &self,
        terminal: &JournaledTerminalCheckpoint,
    ) -> Result<bool, AuthorityError>;

    /// Charge one use at the point of use (AC4/AC9 budget-spend). `validate()`
    /// is the check; this is the commit — each authority-gated action consumes
    /// one use. A token past its `uses_limit` is denied its next gated action.
    async fn spend_use(&self, token: &CapabilityTokenId) -> Result<(), AuthorityError>;
}
