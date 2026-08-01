//! Effective-consent projection consumed by the NFR66 policy explainer.
//!
//! The query remains synchronous and effect-free: adapters fold the durable
//! room journal before composition and update their cached projection only
//! after successful consent appends. This keeps journal I/O out of the domain
//! resolver and the delivery hot path.

use crate::domain::models::PeerId;

/// Whether a sender holds a consent grant.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentState {
    /// A grant is recorded and current.
    Trusted,
    /// A grant existed and was withdrawn. Distinct from `None`: a revocation is a
    /// decision the operator made and must not read as "never asked".
    Revoked,
    /// No grant recorded either way.
    None,
}

/// Read-only view of journaled per-sender consent.
///
/// Sync and effect-free at the trait level: the explainer that consumes it is a
/// decision core, and an `async` query here would drag the fold into an async
/// shell — the anti-pattern `architecture.md:1775` names.
pub trait ConsentProjectionQuery: Send + Sync {
    /// Senders the projection knows about at all.
    ///
    /// Empty is a legitimate answer and the explainer states it out loud.
    fn known_senders(&self) -> Vec<PeerId>;

    /// The consent state for one sender.
    fn consent_for(&self, sender: &PeerId) -> ConsentState;
}
