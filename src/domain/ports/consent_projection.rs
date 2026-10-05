//! Effective-consent projection consumed by the NFR66 policy explainer.
//!
//! The query remains synchronous and effect-free: adapters fold the durable
//! room journal before composition and update their cached projection only
//! after successful consent appends. This keeps journal I/O out of the domain
//! resolver and the delivery hot path.

use crate::domain::models::{PeerId, PinnedVia, PolicyPrincipal};

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

/// Storage namespace for typed grants and grants written before provenance.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ConsentSlot {
    Pinned,
    Pseudonymous,
    Legacy,
}

impl ConsentSlot {
    /// The typed cell a principal's grant is written to and read from first.
    /// Exhaustive and never `Legacy`: every `Pinned` rail shares one cell
    /// (Story 19.18d `P21`, `OQ3` = A).
    #[must_use]
    pub fn of(principal: &PolicyPrincipal) -> Self {
        match principal {
            PolicyPrincipal::Pinned { .. } => Self::Pinned,
            PolicyPrincipal::Pseudonymous(_) => Self::Pseudonymous,
        }
    }
}

/// Read-only view of journaled per-sender consent.
///
/// Sync and effect-free at the trait level: the explainer that consumes it is a
/// decision core, and an `async` query here would drag the fold into an async
/// shell — the anti-pattern `architecture.md:1775` names.
pub trait ConsentProjectionQuery: Send + Sync {
    /// Known cells, sorted by sender label then storage slot.
    fn known_senders(&self) -> Vec<(ConsentSlot, PeerId)>;

    /// Raw state of one storage cell, without legacy reach.
    fn slot_state(&self, slot: ConsentSlot, sender: &PeerId) -> ConsentState;

    /// Typed decisions take precedence; only an absent typed cell reaches legacy.
    /// Story 19.18d: an operator-bound A2A caller never reaches legacy, so no
    /// grant written before typed provenance admits it.
    fn consent_for(&self, principal: &PolicyPrincipal) -> ConsentState {
        match self.slot_state(ConsentSlot::of(principal), principal.peer()) {
            ConsentState::None => match principal {
                PolicyPrincipal::Pinned { via, .. } => match via {
                    PinnedVia::Rap => self.slot_state(ConsentSlot::Legacy, principal.peer()),
                    PinnedVia::BoundA2a => ConsentState::None,
                },
                PolicyPrincipal::Pseudonymous(_) => {
                    self.slot_state(ConsentSlot::Legacy, principal.peer())
                }
            },
            state => state,
        }
    }
}

/// Operator display/revocation view only, never a policy-admission decision.
pub fn peer_consent_state(query: &dyn ConsentProjectionQuery, peer: &PeerId) -> ConsentState {
    let mut state = ConsentState::None;
    for slot in [
        ConsentSlot::Pinned,
        ConsentSlot::Pseudonymous,
        ConsentSlot::Legacy,
    ] {
        match query.slot_state(slot, peer) {
            ConsentState::Trusted => return ConsentState::Trusted,
            ConsentState::Revoked => state = ConsentState::Revoked,
            ConsentState::None => {}
        }
    }
    state
}
