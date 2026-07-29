//! The effective-consent projection the NFR66 explainer consumes (Story 18.3b).
//!
//! # Why this port exists in the story that cannot fill it
//!
//! `ux-design-specification-addendum-peer-policy.md` makes "doctor surfaces
//! effective consent beside TOML policy" a cross-story contract and resolves the
//! ordering in its **O4**: *18-3c defines the projection and its query; 18-3b's
//! NFR66 check consumes it. If 18-3b lands first, its explainer ships a stub that
//! 18-3c fills.* 18-3b landed first.
//!
//! # Why this is not an inert seam
//!
//! A port with no consumer proves nothing — `ContentBlockType::AutoSentMarker` has
//! sat unused since Sprint 0 and is the tree's canonical example. This port ships
//! **with its consumer in the same story**: `PolicyExplainerCheck` queries it and
//! renders a consent line per sender it knows about. The precedent is 18-3's own
//! `PeerInteractionRecorder`, introduced alongside its consumer.
//!
//! # Why the empty implementation must announce itself
//!
//! [`EmptyConsentProjection`] returns no senders, because no journaled grants
//! exist yet: 18-3 recorded `Accepted`/`Refused` per *delivery* via
//! `PeerInteractionRecorder`, and `RoomEvent::PeerDisclosure` is content
//! disclosure, not a grant. The explainer therefore emits an explicit
//! `no journaled consent grants recorded` line rather than nothing at all.
//! **Silence is indistinguishable from absence:** when 18-3c fills the projection,
//! grants would otherwise appear from nowhere — a policy source that materialises
//! without ever having been declared. Being observably empty is what makes the
//! stub conformant, because a mutant that removes the line can be turned RED.
//!
//! `DF-18-3b-CONSENT-PROJECTION` — the `ConsentGranted`/`ConsentRevoked` events and
//! the replay-folded projection are 18.3c's. **Trigger-story: 18.3c.**

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
