use crate::domain::models::{PeerId, PinnedKey};

/// One operator-authored entry in the transport-specific `.rustain/p2p.json`
/// allowlist. It deliberately reuses the existing pinned-key representation;
/// transport admission does not define a second key format or identity derivation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct P2pPeerSpec {
    pub id: String,
    pub pinned_key: Option<PinnedKey>,
}

impl P2pPeerSpec {
    pub fn new(id: String, pinned_key: Option<PinnedKey>) -> Self {
        Self { id, pinned_key }
    }

    pub fn pinned_identity(&self) -> Option<PeerId> {
        self.pinned_key.as_ref()?.peer_id().ok()
    }
}

/// Lossless load state for the transport allowlist.
///
/// Absence, a valid empty list, and malformed input remain distinct so the
/// fail-closed verdict can explain the operator's actual configuration state.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum P2pConfigState {
    Absent,
    Malformed { reason: String },
    Present(Vec<P2pPeerSpec>),
}
