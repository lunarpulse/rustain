//! `rustain peer revoke <alias-or-peer-id>` — remove an entry and journal it
//! (Story 18.4b, AC6).
//!
//! # `--now` changes nothing, and says so
//!
//! `epics.md` and `CC:64` write `revoke --now`. Admission is re-evaluated **per
//! inbound frame**, which is proven by
//! `conformance_p2p_ingress.rs::allowlist_removal_refuses_the_next_frame_on_the_same_open_connection`
//! — so a plain `revoke` already refuses the next frame with no restart, and an
//! optional `--now` would be a flag that changes nothing, which is a flag that
//! lies. It is accepted as a documented no-op alias for the planning text and
//! ⛔ never presented as selecting a behaviour.

use crate::adapters::cli::peer::rows::sanitize_for_terminal;

/// What `--now` means, printed when it is passed so the operator is not left
/// believing it did something.
pub const NOW_IS_A_NO_OP: &str = concat!(
    "note: --now is accepted for compatibility and changes nothing. Admission is ",
    "already checked per frame, so every revocation takes effect on the next frame."
);

/// Copy for a revocation that found nothing to remove.
///
/// A revocation never manufactures identity: an unrecorded target is not turned
/// into a record of its own removal.
#[must_use]
pub fn nothing_recorded_text(target: &str) -> String {
    format!(
        "No transport admission recorded for {:?}; nothing changed.\n\
         `rustain peer list` shows what is configured. A well-formed peer id that\n\
         no configured entry derives is not a target — nothing is written for it.",
        sanitize_for_terminal(target)
    )
}

/// Copy for a revocation refused because the allowlist did not parse.
#[must_use]
pub fn unreadable_text(reason: &str) -> String {
    format!(
        "Refusing to change .rustain/p2p.json: its current contents did not parse ({}).\n\
         Nothing was written.",
        sanitize_for_terminal(reason)
    )
}
