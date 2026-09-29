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

use crate::domain::models::{AgentId, RoomEditDecision, RoomEditKind, RoomRole};

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

/// Which room role does `principal` hold locally?
///
/// Story 18.3a-b (AC2) — this replaces 18.3a's `LOCAL_OPERATOR_ROLE`
/// placeholder. The answer is no longer a constant asserted about an
/// unnamed actor; it is **derived from a named principal**. The local
/// operator — the human at this keyboard, addressed by
/// [`AgentId::local_operator`] — owns the workspace, the journal file and the
/// process, so they are the room's [`RoomRole::Owner`] by construction.
///
/// Fail-closed for everyone else: any other principal gets
/// [`RoomRole::default`] (`Viewer`), never a guess. A second addressable human
/// is a Rule-3 deferred capability — nothing can construct one today (no
/// transport until 18.4, no second local principal), and when one arrives it
/// arrives with a journaled grant, not with this function.
///
/// ⛔ **Identity is equality, never a parse.** `AgentId` segment 0 is a route
/// discriminator, not an identity (`ADR-18-3b-01` D1): three production path
/// shapes exist, so a `segments().next() == "operator"` check would be right
/// for one and silently wrong for two.
///
/// ⛔ This answers *who holds which room role*. Turning a role into a verdict
/// stays [`room_edit_decision`]'s job, and neither may reach a
/// `CapabilityToken`, an `AuthorityProvider` decision, or an approval
/// fingerprint.
#[must_use]
pub fn local_room_role(principal: &AgentId) -> RoomRole {
    if principal == &AgentId::local_operator() {
        RoomRole::Owner
    } else {
        RoomRole::default()
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

    /// Story 18.3a-b AC2 — the acting principal is named, and only the
    /// reserved operator address answers `Owner`.
    ///
    /// Mutant this must turn RED: return `RoomRole::Owner` unconditionally
    /// (i.e. reinstate 18.3a's placeholder constant behind a principal-shaped
    /// signature) → every non-operator principal would become a room owner.
    #[test]
    fn only_the_reserved_operator_address_holds_owner_locally() {
        assert_eq!(local_room_role(&AgentId::local_operator()), RoomRole::Owner);
        for other in [
            AgentId::root(),
            AgentId::new(),
            AgentId::from_peer_path("mcp/s-srv").expect("valid peer path"),
        ] {
            assert_eq!(
                local_room_role(&other),
                RoomRole::Viewer,
                "{other} is not the local operator and must hold no room authority"
            );
            assert_eq!(
                room_edit_decision(local_room_role(&other), RoomEditKind::RoleAssignment),
                RoomEditDecision::Deny
            );
        }
    }
}
