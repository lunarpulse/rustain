//! Identity provenance for consent and per-sender interaction policy.
//!
//! Peer labels remain byte-compatible across transports; policy must retain
//! their origin instead of treating an equal label as equal authority.

use serde::{Deserialize, Serialize};

use super::PeerId;

#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PolicyPrincipal {
    Pinned { peer: PeerId, via: PinnedVia },
    Pseudonymous(PeerId),
}

#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PinnedVia {
    Rap,
}

#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrincipalKind {
    Pinned,
    Pseudonymous,
    #[serde(other)]
    Unknown,
}

impl PolicyPrincipal {
    /// Construct the principal at the RAP ingress boundary.
    pub fn rap(peer: PeerId) -> Self {
        PolicyPrincipal::Pinned {
            peer,
            via: PinnedVia::Rap,
        }
    }

    /// Display/item label only. This does not preserve policy authority.
    pub fn peer(&self) -> &PeerId {
        match self {
            Self::Pinned { peer, .. } | Self::Pseudonymous(peer) => peer,
        }
    }

    pub fn kind(&self) -> PrincipalKind {
        match self {
            Self::Pinned { .. } => PrincipalKind::Pinned,
            Self::Pseudonymous(_) => PrincipalKind::Pseudonymous,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_peer_labels_keep_distinct_policy_authority() {
        let peer = PeerId::from_public_key(&[7; 32]).unwrap();
        let pinned = PolicyPrincipal::rap(peer.clone());
        let pseudonymous = PolicyPrincipal::Pseudonymous(peer.clone());
        assert_eq!(pinned.peer(), &peer);
        assert_eq!(pseudonymous.peer(), &peer);
        assert_ne!(pinned, pseudonymous);
        assert_eq!(pinned.kind(), PrincipalKind::Pinned);
        assert_eq!(pseudonymous.kind(), PrincipalKind::Pseudonymous);
        assert!(matches!(
            pinned,
            PolicyPrincipal::Pinned {
                via: PinnedVia::Rap,
                ..
            }
        ));
    }

    #[test]
    fn principal_kind_encoding_and_future_values() {
        for (kind, wire) in [
            (PrincipalKind::Pinned, "\"pinned\""),
            (PrincipalKind::Pseudonymous, "\"pseudonymous\""),
            (PrincipalKind::Unknown, "\"unknown\""),
        ] {
            assert_eq!(serde_json::to_string(&kind).unwrap(), wire);
            assert_eq!(serde_json::from_str::<PrincipalKind>(wire).unwrap(), kind);
        }
        assert_eq!(
            serde_json::from_str::<PrincipalKind>("\"quarantined\"").unwrap(),
            PrincipalKind::Unknown
        );
        assert!(serde_json::from_str::<PrincipalKind>("{}").is_err());
    }
}
