//! Replay-folded journal consent projection (Story 18.3d).
//!
//! The durable room journal is authoritative. The projection folds it once at
//! startup or observer load, then accepts appended consent records
//! incrementally without reading the journal on the delivery hot path.

use std::collections::HashMap;
use std::sync::Arc;

use crate::domain::models::{JournalEntry, JournalRecord, PeerId, RoomEvent};
use crate::domain::ports::{
    ConsentProjectionQuery, ConsentState, RoomJournalError, RoomJournalReader,
};

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

/// Lock-free read projection over the append-only consent record family.
#[derive(Clone)]
pub struct JournalConsentProjection {
    states: Arc<arc_swap::ArcSwap<HashMap<PeerId, ConsentState>>>,
}

impl Default for JournalConsentProjection {
    fn default() -> Self {
        Self {
            states: Arc::new(arc_swap::ArcSwap::from_pointee(HashMap::new())),
        }
    }
}

impl JournalConsentProjection {
    /// Fold a point-in-time journal snapshot. Duplicate grants are idempotent;
    /// a revocation for an unknown sender is deliberately a no-op.
    #[must_use]
    pub fn from_entries(entries: &[JournalEntry]) -> Self {
        let mut states = HashMap::new();
        for entry in entries {
            if let JournalRecord::Room(event) = &entry.record {
                apply_to_map(&mut states, event);
            }
        }
        Self {
            states: Arc::new(arc_swap::ArcSwap::from_pointee(states)),
        }
    }

    /// Read and fold the workspace journal without creating files.
    pub async fn load_workspace(workspace: &std::path::Path) -> Result<Self, RoomJournalError> {
        let reader =
            crate::infrastructure::subagent::node_journal::WorkspaceJournalReader::open_workspace(
                workspace,
            );
        let entries = reader.load_entries().await?;
        Ok(Self::from_entries(&entries))
    }

    /// Apply one successfully appended room event to the cached projection.
    pub fn apply(&self, event: &RoomEvent) {
        self.states.rcu(|current| {
            let mut next = (**current).clone();
            apply_to_map(&mut next, event);
            Arc::new(next)
        });
    }
}

impl ConsentProjectionQuery for JournalConsentProjection {
    fn known_senders(&self) -> Vec<PeerId> {
        let snapshot = self.states.load();
        let mut senders: Vec<_> = snapshot.keys().cloned().collect();
        senders.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        senders
    }

    fn consent_for(&self, sender: &PeerId) -> ConsentState {
        self.states
            .load()
            .get(sender)
            .copied()
            .unwrap_or(ConsentState::None)
    }
}

fn apply_to_map(states: &mut HashMap<PeerId, ConsentState>, event: &RoomEvent) {
    match event {
        RoomEvent::ConsentGranted {
            sender: Some(sender),
            ..
        } => {
            states.insert(sender.clone(), ConsentState::Trusted);
        }
        RoomEvent::ConsentRevoked {
            sender: Some(sender),
            ..
        } if states.contains_key(sender) => {
            states.insert(sender.clone(), ConsentState::Revoked);
        }
        _ => {}
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

    fn entry(
        seq: u64,
        event: crate::domain::models::RoomEvent,
    ) -> crate::domain::models::JournalEntry {
        crate::domain::models::JournalEntry::new(
            seq,
            crate::domain::models::JournalRecord::Room(event),
            seq as i64,
        )
    }

    #[test]
    fn journal_projection_folds_latest_act_and_ignores_unknown_revocation() {
        let alice = PeerId::from_public_key(&[2u8; 32]).unwrap();
        let bob = PeerId::from_public_key(&[3u8; 32]).unwrap();
        let entries = vec![
            entry(
                1,
                crate::domain::models::RoomEvent::ConsentGranted {
                    sender: Some(alice.clone()),
                    granted_at: 10,
                },
            ),
            entry(
                2,
                crate::domain::models::RoomEvent::ConsentGranted {
                    sender: Some(alice.clone()),
                    granted_at: 11,
                },
            ),
            entry(
                3,
                crate::domain::models::RoomEvent::ConsentRevoked {
                    sender: Some(alice.clone()),
                    revoked_at: 12,
                },
            ),
            entry(
                4,
                crate::domain::models::RoomEvent::ConsentRevoked {
                    sender: Some(bob.clone()),
                    revoked_at: 13,
                },
            ),
        ];

        let projection = JournalConsentProjection::from_entries(&entries);
        assert_eq!(projection.known_senders(), vec![alice.clone()]);
        assert_eq!(projection.consent_for(&alice), ConsentState::Revoked);
        assert_eq!(projection.consent_for(&bob), ConsentState::None);

        projection.apply(&crate::domain::models::RoomEvent::ConsentGranted {
            sender: Some(bob.clone()),
            granted_at: 14,
        });
        assert_eq!(projection.consent_for(&bob), ConsentState::Trusted);
    }

    #[test]
    fn consent_events_default_missing_sender_without_fabricating_identity() {
        let event: crate::domain::models::RoomEvent =
            serde_json::from_str(r#"{"event":"consent_granted","granted_at":10}"#).unwrap();
        assert!(matches!(
            event,
            crate::domain::models::RoomEvent::ConsentGranted { sender: None, .. }
        ));
    }
}
