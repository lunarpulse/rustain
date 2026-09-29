//! Live-refolded journal room-role projection (Story 18.3a, AC4).
//!
//! # This is a live refold, not a startup snapshot
//!
//! Story 18.3d's first and unanimous review finding: `/team trust|untrust`
//! wrote the journal while the long-lived process held a *startup-time*
//! projection, so a revocation was inert until restart. The carried-forward
//! ruling — any projection consumed by a long-lived process and mutated by more
//! than one writer must be a **live high-water-sequence refold of the one
//! journal, swapped atomically behind a sync snapshot** — applies here
//! verbatim, because this projection has exactly that shape and exactly those
//! writers.
//!
//! The journal is the write authority, not a process: any writer appends
//! durable-first and this holder refolds on a high-water sequence compare.
//! There is no second store — no role table, no sidecar file (ADR-17-CC-01).
//!
//! # ⛔ It fails CLOSED, and that is a deliberate divergence
//!
//! The sibling `PendingConsentManager::refresh_projection` logs
//! `"consent projection refresh failed; proceeding with cached state"` and
//! returns — fail-**open**-to-cached. For a *role* projection that is the
//! defect itself: a stale snapshot answering `Owner` after a revoke. On a read
//! error this projection drops to the empty fold, so every principal answers
//! the least-privileged [`RoomRole::Viewer`], and it retains the error so a
//! surface can say why. Recorded in `ADR-18-3a-01`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::domain::models::{JournalEntry, JournalRecord, PeerId, RoomEvent, RoomRole};
use crate::domain::ports::{RoomJournalReader, RoomRoleProjectionQuery, RoomRoleState};

/// Lock-free read projection over the append-only room-role record family,
/// plus the high-water refresh driver that keeps it live.
///
/// Unlike the consent pair — where the fold
/// (`JournalConsentProjection`) and its driver (`PendingConsentManager`) are
/// separate types — the fold and the driver are one type here. There is
/// exactly one holder, and splitting them would create a second seam whose
/// only caller is its own partner.
pub struct JournalRoomRoleProjection {
    roles: arc_swap::ArcSwap<HashMap<PeerId, RoomRoleState>>,
    reader: Option<Arc<dyn RoomJournalReader>>,
    /// Highest journal `seq` folded into the current snapshot.
    last_seq: AtomicU64,
    /// Retained read failure. `Some` means the snapshot was deliberately
    /// emptied and every answer is least-privileged.
    read_error: arc_swap::ArcSwapOption<String>,
}

impl JournalRoomRoleProjection {
    /// A projection with no reader: it never refolds and answers
    /// least-privilege for everyone. The honest composition for a workspace
    /// with no orchestration journal.
    #[must_use]
    pub fn inert() -> Self {
        Self {
            roles: arc_swap::ArcSwap::from_pointee(HashMap::new()),
            reader: None,
            last_seq: AtomicU64::new(0),
            read_error: arc_swap::ArcSwapOption::empty(),
        }
    }

    /// Bind the read side of the one journal. The fold happens on the first
    /// [`Self::refresh`], not here: construction stays effect-free so the
    /// composition root never blocks on I/O.
    #[must_use]
    pub fn with_reader(reader: Arc<dyn RoomJournalReader>) -> Self {
        Self {
            reader: Some(reader),
            ..Self::inert()
        }
    }

    /// Fold a point-in-time snapshot. Test and CLI seam; the live path is
    /// [`Self::refresh`].
    #[must_use]
    pub fn from_entries(entries: &[JournalEntry]) -> Self {
        let projection = Self::inert();
        projection.replace_from(entries);
        projection
    }

    /// Refold from the durable journal when its high-water mark changes.
    ///
    /// Called before every read and every decision by the `/room role` shell,
    /// so a grant or revocation appended by any writer — this process, a future
    /// CLI, the daemon — takes effect **without a restart**. A lower high-water
    /// mark is a journal regression and refolds fail-closed rather than keeping
    /// cached authority. A full refold is the deliberate first cut.
    pub async fn refresh(&self) {
        let Some(reader) = &self.reader else {
            return;
        };
        match reader.load_entries().await {
            Ok(entries) => {
                let max_seq = entries.last().map(|entry| entry.seq).unwrap_or(0);
                // Refold whenever the authoritative high-water mark changes in
                // either direction, and after a failed-closed read. A missing
                // journal is a successful empty read; retaining cached roles
                // in that case would preserve authority with no durable grant.
                if max_seq != self.last_seq.load(Ordering::Acquire)
                    || self.read_error.load().is_some()
                {
                    self.replace_from(&entries);
                    self.last_seq.store(max_seq, Ordering::Release);
                }
                self.read_error.store(None);
            }
            Err(error) => {
                // FAIL CLOSED. Do not proceed on cached state: that is how a
                // revoked principal keeps answering `Owner`.
                self.roles.store(Arc::new(HashMap::new()));
                self.last_seq.store(0, Ordering::Release);
                self.read_error.store(Some(Arc::new(error.to_string())));
                tracing::warn!(
                    message_type = "room_role_projection_failed_closed",
                    error = %error,
                    "room role projection could not read the journal; every role answers viewer"
                );
            }
        }
    }

    /// Apply one successfully appended event to the cached snapshot, so the
    /// writer's own next read reflects its own append even before the next
    /// high-water refresh.
    pub fn apply(&self, event: &RoomEvent, seq: u64) {
        self.roles.rcu(|current| {
            let mut next = (**current).clone();
            apply_to_map(&mut next, event);
            Arc::new(next)
        });
        self.last_seq.fetch_max(seq, Ordering::Release);
    }

    /// Atomically replace the cached fold with a fresh fold of `entries`.
    pub fn replace_from(&self, entries: &[JournalEntry]) {
        let mut roles = HashMap::new();
        for entry in entries {
            if let JournalRecord::Room(event) = &entry.record {
                apply_to_map(&mut roles, event);
            }
        }
        self.roles.store(Arc::new(roles));
    }

    /// The retained read failure, if the last refresh failed closed.
    #[must_use]
    pub fn read_error(&self) -> Option<String> {
        self.read_error.load().as_deref().cloned()
    }
}

impl Default for JournalRoomRoleProjection {
    fn default() -> Self {
        Self::inert()
    }
}

impl RoomRoleProjectionQuery for JournalRoomRoleProjection {
    fn role_for(&self, peer: &PeerId) -> RoomRole {
        match self.roles.load().get(peer) {
            Some(RoomRoleState::Granted(RoomRole::Unknown)) => RoomRole::Viewer,
            Some(RoomRoleState::Granted(role)) => *role,
            // Revoked, never-granted, and forward-unknown are all least
            // privilege. A read failure already emptied the map.
            _ => RoomRole::Viewer,
        }
    }

    fn journaled_roles(&self) -> Vec<(PeerId, RoomRoleState)> {
        let snapshot = self.roles.load();
        let mut roles: Vec<(PeerId, RoomRoleState)> = snapshot
            .iter()
            .map(|(peer, state)| (peer.clone(), *state))
            .collect();
        roles.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
        roles
    }
}

/// The fold, scoped **by variant**.
///
/// ⛔ Only this story's two variants may be read here. A
/// [`RoomEvent::ConsentRevoked`] is one sender's standing *delivery* consent
/// and an FR158 trust-set revocation is allowlist removal at L1; folding
/// either into a room role would conflate three different facts into one
/// answer. `tests/conformance_18_3a_room.rs` keeps that mutant RED.
fn apply_to_map(roles: &mut HashMap<PeerId, RoomRoleState>, event: &RoomEvent) {
    match event {
        RoomEvent::RoomRoleGranted {
            peer: Some(peer),
            role,
            ..
        } => {
            // Latest act per peer. A duplicate grant re-inserts the same
            // value, so replay is idempotent by construction.
            roles.insert(peer.clone(), RoomRoleState::Granted(*role));
        }
        // `contains_key` is load bearing: revoking a peer that was never
        // granted a role must not manufacture an identity in the projection.
        RoomEvent::RoomRoleRevoked {
            peer: Some(peer), ..
        } if roles.contains_key(peer) => {
            roles.insert(peer.clone(), RoomRoleState::Revoked);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(byte: u8) -> PeerId {
        PeerId::from_public_key(&[byte; 32]).expect("32-byte key")
    }

    fn entry(seq: u64, event: RoomEvent) -> JournalEntry {
        JournalEntry::new(seq, JournalRecord::Room(event), seq as i64)
    }

    #[test]
    fn latest_act_wins_and_revocation_never_synthesizes_identity() {
        let projection = JournalRoomRoleProjection::from_entries(&[
            entry(
                1,
                RoomEvent::RoomRoleGranted {
                    peer: Some(peer(1)),
                    role: RoomRole::Editor,
                    granted_at: 1,
                },
            ),
            // Duplicate grant — idempotent.
            entry(
                2,
                RoomEvent::RoomRoleGranted {
                    peer: Some(peer(1)),
                    role: RoomRole::Editor,
                    granted_at: 2,
                },
            ),
            // Revoke a peer nobody granted — no-op, no entry created.
            entry(
                3,
                RoomEvent::RoomRoleRevoked {
                    peer: Some(peer(9)),
                    revoked_at: 3,
                },
            ),
            // A missing identity is a no-op too.
            entry(
                4,
                RoomEvent::RoomRoleGranted {
                    peer: None,
                    role: RoomRole::Owner,
                    granted_at: 4,
                },
            ),
        ]);
        assert_eq!(projection.role_for(&peer(1)), RoomRole::Editor);
        assert_eq!(projection.role_for(&peer(9)), RoomRole::Viewer);
        assert_eq!(
            projection.journaled_roles(),
            vec![(peer(1), RoomRoleState::Granted(RoomRole::Editor))]
        );
    }

    #[test]
    fn a_revoked_role_reads_as_revoked_and_answers_least_privilege() {
        let projection = JournalRoomRoleProjection::from_entries(&[
            entry(
                1,
                RoomEvent::RoomRoleGranted {
                    peer: Some(peer(2)),
                    role: RoomRole::Owner,
                    granted_at: 1,
                },
            ),
            entry(
                2,
                RoomEvent::RoomRoleRevoked {
                    peer: Some(peer(2)),
                    revoked_at: 2,
                },
            ),
        ]);
        assert_eq!(projection.role_for(&peer(2)), RoomRole::Viewer);
        assert_eq!(
            projection.journaled_roles(),
            vec![(peer(2), RoomRoleState::Revoked)],
            "a revocation is a decision the operator made; it must not read as never-granted"
        );
    }

    #[test]
    fn an_unknown_grant_is_auditable_but_effectively_viewer() {
        let identity = peer(4);
        let projection = JournalRoomRoleProjection::from_entries(&[entry(
            1,
            RoomEvent::RoomRoleGranted {
                peer: Some(identity.clone()),
                role: RoomRole::Unknown,
                granted_at: 1,
            },
        )]);

        assert_eq!(projection.role_for(&identity), RoomRole::Viewer);
        assert_eq!(
            projection.journaled_roles(),
            vec![(identity, RoomRoleState::Granted(RoomRole::Unknown))]
        );
    }

    /// The negative control that makes Trap 2 executable at the fold layer.
    #[test]
    fn a_consent_revocation_is_not_a_room_role_revocation() {
        let projection = JournalRoomRoleProjection::from_entries(&[
            entry(
                1,
                RoomEvent::RoomRoleGranted {
                    peer: Some(peer(3)),
                    role: RoomRole::Owner,
                    granted_at: 1,
                },
            ),
            entry(
                2,
                RoomEvent::ConsentRevoked {
                    sender: Some(peer(3)),
                    revoked_at: 2,
                },
            ),
        ]);
        assert_eq!(
            projection.role_for(&peer(3)),
            RoomRole::Owner,
            "withdrawing delivery consent is not withdrawing a room role"
        );
    }
}
