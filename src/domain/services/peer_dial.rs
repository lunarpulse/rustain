use crate::domain::models::{P2pConfigState, PeerId};

/// Strict transport allowlist result. Reachability alone never grants access.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PeerDialVerdict {
    Admit,
    Refuse(PeerDialRefusal),
}

impl std::fmt::Display for PeerDialVerdict {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Admit => formatter.write_str("peer admitted by the transport allowlist"),
            Self::Refuse(reason) => reason.fmt(formatter),
        }
    }
}

/// Operator-facing, reason-carrying transport refusal.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PeerDialRefusal {
    #[error("peer transport refused: .rustain/p2p.json is absent")]
    ConfigAbsent,
    #[error("peer transport refused: .rustain/p2p.json is valid but its agents list is empty")]
    EmptyAllowlist,
    #[error("peer transport refused: .rustain/p2p.json is malformed: {reason}")]
    MalformedConfig { reason: String },
    #[error("peer transport refused: configured peer {peer:?} has no pinned key")]
    MissingPinnedKey { peer: String },
    #[error("peer transport refused: {peer_id} is not in .rustain/p2p.json")]
    NotAllowlisted { peer_id: PeerId },
}

/// Decide whether a presented cryptographic peer may use the QUIC transport.
///
/// Pure and effect-free: no I/O, clock, lock, cache, or fallback. In particular,
/// `A2aAdmissionPolicy` is not consulted. That setting governs tasks arriving
/// over HTTP JSON-RPC; inheriting its `Allow` value here would silently open a
/// separate QUIC transport feeding the delivery front door.
pub fn peer_dial_verdict(peer_id: &PeerId, config: &P2pConfigState) -> PeerDialVerdict {
    match config {
        P2pConfigState::Absent => PeerDialVerdict::Refuse(PeerDialRefusal::ConfigAbsent),
        P2pConfigState::Malformed { reason } => {
            PeerDialVerdict::Refuse(PeerDialRefusal::MalformedConfig {
                reason: reason.clone(),
            })
        }
        P2pConfigState::Present(peers) if peers.is_empty() => {
            PeerDialVerdict::Refuse(PeerDialRefusal::EmptyAllowlist)
        }
        P2pConfigState::Present(peers) => {
            if peers
                .iter()
                .filter_map(|peer| peer.pinned_identity())
                .any(|allowed| allowed == *peer_id)
            {
                return PeerDialVerdict::Admit;
            }
            // Precedence matters. An unpinned entry has no PeerId to compare, so
            // it can only explain a refusal when the list can answer NOTHING —
            // if even one entry carries a comparable pin, the honest answer for
            // a key that matched none of them is that this peer is not on the
            // list. Letting one unpinned entry speak for every refusal would
            // make `NotAllowlisted` unreachable and would name an innocent peer
            // in the record of a stranger knocking.
            match peers.iter().find(|peer| peer.pinned_identity().is_none()) {
                Some(unpinned) if peers.iter().all(|peer| peer.pinned_identity().is_none()) => {
                    PeerDialVerdict::Refuse(PeerDialRefusal::MissingPinnedKey {
                        peer: unpinned.id.clone(),
                    })
                }
                _ => PeerDialVerdict::Refuse(PeerDialRefusal::NotAllowlisted {
                    peer_id: peer_id.clone(),
                }),
            }
        }
    }
}
