//! Effective room-role projection consumed by the `/room role` surface
//! (Story 18.3a, AC4).
//!
//! The query is **synchronous and effect-free** on purpose, exactly as
//! [`crate::domain::ports::ConsentProjectionQuery`] is: the consumer is a
//! decision shell around an effect-free core, and an `async` query here would
//! drag journal I/O into it. Adapters fold the durable room journal and swap a
//! cached snapshot; reading that snapshot never touches a file.

use crate::domain::models::{PeerId, RoomRole};

/// What the journal knows about one principal's room role.
///
/// [`Self::Revoked`] is deliberately distinct from absence. A revocation is a
/// decision the operator made and must not read back as "never granted" — the
/// same reasoning that gives [`crate::domain::ports::ConsentState`] its
/// `Revoked` variant. It is a *journaled fact*, never an enforcement claim:
/// "X's role was revoked at T" is what the journal knows; "X can no longer
/// read anything" is not.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomRoleState {
    /// A role grant is recorded and current.
    Granted(RoomRole),
    /// A grant existed and was withdrawn.
    Revoked,
}

/// Read-only view of journaled room roles.
pub trait RoomRoleProjectionQuery: Send + Sync {
    /// The effective role for one principal.
    ///
    /// **Fail-closed.** Unknown, revoked, and unreadable all answer
    /// [`RoomRole::Viewer`] — the least-privileged role. A projection that
    /// cannot read the journal must never answer `Owner` from a stale cache;
    /// that stale-authority answer is precisely the defect Story 18.3d shipped
    /// one layer down.
    fn role_for(&self, peer: &PeerId) -> RoomRole;

    /// Every principal the journal has a role fact for, including revocations,
    /// sorted by peer id. Empty is a legitimate answer and the `/room role
    /// list` surface says so out loud rather than rendering nothing.
    fn journaled_roles(&self) -> Vec<(PeerId, RoomRoleState)>;
}
