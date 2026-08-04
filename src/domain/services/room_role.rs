//! Room-edit decision core (Story 18.3a, AC3).
//!
//! `architecture.md`'s dominant idiom: effect-free, value-returning, sync, and
//! enum-verdict shaped. No `.await`, no locks, no I/O, no environment reads —
//! the effects live in the `/room role` shell that calls this.
//!
//! ⛔ **The separation is the point.** `epics.md` contracted scope: *"Owner/
//! editor/viewer roles govern room/artifact edits only — never execution
//! authority (side effects still require `AuthorityProvider` + `CapabilityToken`
//! + approval fingerprint)."* Nothing in this module may be threaded into a
//! capability set, a delegation, or a fingerprint's canonical bytes; adding a
//! field to those would additionally require a format-version bump this story
//! does not take.

use crate::domain::models::{RoomEditDecision, RoomEditKind, RoomRole};

/// May `role` perform `edit` in the room?
///
/// Fail-closed by construction: every arm that is not an explicit grant is a
/// [`RoomEditDecision::Deny`], so a [`RoomRole`] variant added by a future
/// build (the enum is `#[non_exhaustive]`) and a role string this build cannot
/// read both deny rather than guess.
#[must_use]
pub fn room_edit_decision(role: RoomRole, edit: RoomEditKind) -> RoomEditDecision {
    match (role, edit) {
        (RoomRole::Owner, _) => RoomEditDecision::Allow,
        (RoomRole::Editor, RoomEditKind::DurableContent) => RoomEditDecision::Allow,
        _ => RoomEditDecision::Deny,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The core is total over the shipped cross-product; the exhaustive
    /// table-driven keystone lives in `tests/conformance_18_3a_room.rs`.
    #[test]
    fn only_an_owner_may_assign_roles() {
        assert_eq!(
            room_edit_decision(RoomRole::Owner, RoomEditKind::RoleAssignment),
            RoomEditDecision::Allow
        );
        for role in [RoomRole::Editor, RoomRole::Viewer, RoomRole::Unknown] {
            assert_eq!(
                room_edit_decision(role, RoomEditKind::RoleAssignment),
                RoomEditDecision::Deny,
                "{role:?} must not assign roles"
            );
        }
    }

    #[test]
    fn the_decision_core_is_effect_free_and_repeatable() {
        let first = room_edit_decision(RoomRole::Editor, RoomEditKind::DurableContent);
        let second = room_edit_decision(RoomRole::Editor, RoomEditKind::DurableContent);
        assert_eq!(first, second);
        assert!(first.is_allowed());
    }
}
