//! Who must act on a ticket (Story 18.3a-b, AC3 — FR152, COLLAB invariant 17).
//!
//! # Why a variant and never a bare name
//!
//! FR152 forbids *"a bare … field (which would launder execution authority)"*
//! — the word it forbids is the one this file may not contain, so it is
//! paraphrased here and pinned by a structural ratchet in
//! `tests/conformance_18_3a_b_addressing.rs`, which asserts that literal
//! appears **zero** times across `src/`. The prohibition is a **type**
//! requirement, not a naming convention. A bare `AgentId` could name an agent,
//! and nothing structural would stop a reader from acting on it: *"a UI control
//! that launders execution authority around the security model. A remote
//! teammate types a name into a text box and my agent writes to my
//! filesystem"*
//! (`rustain-shared-context-and-operator-collaboration-design-2026-07-10.md`,
//! invariant I10). Making the addressee an enum forces every future arm to
//! carry its own authority consequence beside the name.
//!
//! ⛔ **The `Node` arm does not ship in this cut.** When it lands it must carry
//! its minted grant — `Node { agent: AgentId, granted: CapabilityTokenId }` —
//! so an assignment-to-node without a journaled delegation is *unrepresentable*
//! (`DF-18-3a-b-ASSIGN-MINT`, trigger-story 18-3a-c). Until then the human case
//! is the only representable case, which is the strongest available discharge
//! of FR152's prohibition.
//!
//! ⛔ **`NodeId` never enters** (NFR74): iroh's `NodeId` is a reachability
//! address, not an identity. This type keys on `AgentId` only.

use crate::domain::models::AgentId;
use serde::{Deserialize, Serialize};

/// Who must act on a ticket. The **variant** — never a bare name — carries the
/// authority consequence (FR152, COLLAB invariant 17).
///
/// Internally tagged, mirroring [`crate::domain::models::RoomEvent`]'s own
/// idiom, so `#[serde(other)]` is legal on the fallback arm.
///
/// ⚠ **`Unknown` re-serializes as `Unknown`.** `#[serde(other)]` is
/// deserialize-only: a `to` written by a newer build reads back here as
/// [`TicketAddressee::Unknown`] and, if it were ever re-serialized, would emit
/// `{"kind":"unknown"}` and lose the original. That is harmless for the same
/// documented reason [`crate::domain::models::RoomEvent::Unrecognized`] gives —
/// the journal is append-only and never rewritten, so nothing round-trips this
/// variant back to disk. Stated here rather than left for the next reader to
/// discover.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TicketAddressee {
    /// A human principal, addressed by [`AgentId::local_operator`].
    ///
    /// **Zero authority**: a durable attribution and nothing more. Naming the
    /// operator here grants no capability, contributes to no `CapabilityToken`,
    /// no `AuthorityProvider` decision, and no approval fingerprint.
    Operator { id: AgentId },
    /// Written by a newer build. Never fabricates identity.
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC3 positive control — the shipped wire shape, pinned.
    #[test]
    fn the_operator_addressee_round_trips_as_a_tagged_object() {
        let addressee = TicketAddressee::Operator {
            id: AgentId::local_operator(),
        };
        let json = serde_json::to_string(&addressee).expect("serialize");
        assert_eq!(json, r#"{"kind":"operator","id":"operator"}"#);
        let back: TicketAddressee = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, addressee);
    }

    /// AC3 third mutant — remove `#[serde(other)] Unknown` and a ticket from a
    /// newer build fails the whole journal load instead of degrading.
    #[test]
    fn an_addressee_kind_from_a_newer_build_reads_as_unknown() {
        let future: TicketAddressee =
            serde_json::from_str(r#"{"kind":"node","agent":"abc","granted":"tok"}"#)
                .expect("an unknown kind must not fail the load");
        assert_eq!(future, TicketAddressee::Unknown);
    }
}
