//! The empty consent projection (Story 18.3b, `DF-18-3b-CONSENT-PROJECTION`).
//!
//! Ships with its consumer — `PolicyExplainerCheck` queries it on every
//! `rustain doctor` run — which is what distinguishes it from an inert seam.
//! 18-3c replaces it with a replay-folded projection over `ConsentGranted` /
//! `ConsentRevoked`; nothing else about the explainer changes when it does.

use crate::domain::models::PeerId;
use crate::domain::ports::{ConsentProjectionQuery, ConsentState};

/// The projection before any consent grant is journaled.
///
/// Returns no senders, truthfully: 18-3 recorded `Accepted`/`Refused` per
/// *delivery* through `PeerInteractionRecorder`, and `RoomEvent::PeerDisclosure`
/// is content disclosure — neither is a grant. The explainer turns that emptiness
/// into an explicit `no journaled consent grants recorded` line rather than into
/// no output at all, so 18-3c's grants cannot later appear from nowhere.
#[derive(Debug, Clone, Copy, Default)]
pub struct EmptyConsentProjection;

impl ConsentProjectionQuery for EmptyConsentProjection {
    fn known_senders(&self) -> Vec<PeerId> {
        Vec::new()
    }

    fn consent_for(&self, _sender: &PeerId) -> ConsentState {
        ConsentState::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stub_knows_no_senders_and_grants_nothing() {
        let projection = EmptyConsentProjection;
        assert!(projection.known_senders().is_empty());
        assert_eq!(
            projection.consent_for(&PeerId::from_public_key(&[1u8; 32]).unwrap()),
            ConsentState::None
        );
    }
}
