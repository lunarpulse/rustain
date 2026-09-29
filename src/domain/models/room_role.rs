//! Room-role axis (Story 18.3a, AC3).
//!
//! A room role governs **room edits only** — who may change the durable room's
//! own membership and content. It is deliberately a *separate axis* from
//! execution authority: side effects still require
//! `AuthorityProvider` + `CapabilityToken` + an approval fingerprint, and a
//! role contributes to none of them. `tests/conformance_18_3a_room.rs` pins
//! that separation with a negative source ratchet, because no behavioural test
//! can exhaustively prove an absence.
//!
//! # Why this is not a fourth [`crate::domain::models::subagent_view::OwnershipKind`] variant
//!
//! `OwnershipKind::Self_` is unforgeable by construction (sealed payload, no
//! `Serialize`/`Deserialize`, structurally absent from `WireOwnershipKind`).
//! Room roles ride the durable journal, so they *must* be serializable. Adding
//! a serializable role to that enum would put a wire-reachable variant beside a
//! deliberately wire-unreachable one. Two axes, two types.

use serde::{Deserialize, Serialize};

/// Who a principal is *in a room*.
///
/// `Unknown` is serde's future-value fallback for a role string this build does
/// not recognise, exactly as [`crate::domain::models::Direction::Unknown`] is
/// for directions. It is a **deny-everything** role: a build that cannot read a
/// role must not act on a guess.
///
/// The `Default` is `Viewer` and that is load bearing. `RoomEvent::RoomRoleGranted`
/// declares `#[serde(default)] role`, and serde's field-level `default` calls
/// `<RoomRole as Default>::default()`. A grant line written by a newer build
/// with a field this one cannot read must therefore fall to the *least*
/// privileged role, never to ownership.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoomRole {
    /// May change room roles, and everything an `Editor` may do.
    Owner,
    /// May make durable room-content edits, but never a role edit.
    Editor,
    /// Read-only. The least-privileged role, and the default.
    #[default]
    Viewer,
    /// A role string this build does not understand. Denies everything.
    #[serde(other)]
    Unknown,
}

impl RoomRole {
    /// Stable operator-facing label. Also the accepted spelling on the
    /// `/room role grant <target> <role>` command line.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Editor => "editor",
            Self::Viewer => "viewer",
            _ => "unknown",
        }
    }

    /// Parse an operator-typed role. Deliberately **not** `FromStr` over the
    /// serde representation: an unrecognised word typed at the prompt is an
    /// operator error to report, not a silent fall to [`RoomRole::Unknown`].
    #[must_use]
    pub fn parse_grantable(value: &str) -> Option<Self> {
        match value {
            "owner" => Some(Self::Owner),
            "editor" => Some(Self::Editor),
            "viewer" => Some(Self::Viewer),
            _ => None,
        }
    }
}

/// The classes of room edit a role can gate.
///
/// Cut 1 of Story 18.3a ships exactly one surface in the [`Self::RoleAssignment`]
/// class (`/room role grant|revoke`). [`Self::DurableContent`] is classified per
/// Rule 3 as **deferred capability, not a defect**: the durable room-content
/// edit surfaces a role would gate (patch verdict, artifact mutation) do not
/// exist yet and arrive with story **18-3a-c**, its named trigger-story. No
/// reachable invariant fails in the meantime, because no `Viewer` principal can
/// reach a durable write path at all in cut 1.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RoomEditKind {
    /// Grant or revoke another principal's room role.
    RoleAssignment,
    /// Mutate durable room content (artifact/patch decisions). 18-3a-c.
    DurableContent,
}

/// Verdict of the room-edit decision core. An enum, not a `bool`: a verdict
/// that reads as `true` at a call site is the shape that gets inverted in a
/// refactor and never noticed.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoomEditDecision {
    Allow,
    Deny,
}

impl RoomEditDecision {
    #[must_use]
    pub fn is_allowed(self) -> bool {
        matches!(self, Self::Allow)
    }
}
