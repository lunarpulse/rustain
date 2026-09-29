//! `rustain peer add <alias> <ticket>` — the trust gate (Story 18.4b, AC3/AC4).
//!
//! # The confirm is the gate, and it has no bypass
//!
//! `add` decodes, derives, shows a fingerprint, and pins **only** on an explicit
//! confirm. ⛔ There is no `--yes` and no `--force`: a flag that skips the
//! confirm deletes the trust gate rather than accelerating it, and `add` never
//! auto-trusts (`CC:107`).
//!
//! # A non-TTY refuses, and that is not a bypass either (ruling A10)
//!
//! `invite`, `list`, `show` and `revoke` are headless-safe. `add` is not: it
//! needs a human to compare a fingerprint out of band. Without a terminal it
//! **refuses with a named reason and writes nothing** — "no terminal" must not
//! become the bypass the paragraph above forbids.

use std::io::{BufRead, Write};

use crate::adapters::cli::peer::rows::sanitize_for_terminal;
use crate::domain::models::PeerTicketError;

/// Why an import did not happen. Every arm writes nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AddRefusal {
    /// The ticket itself refused: malformed, expired, or altered.
    Ticket(PeerTicketError),
    /// The alias is already pinned to a different key — AC4's alarm.
    KeyMismatch { block: String },
    /// No terminal, so no confirm could be taken.
    NoTerminal,
    /// The operator declined at the confirm.
    Declined,
    /// The requested alias cannot be represented by the shipped loader.
    InvalidAlias,
    /// This identity already has a different transport alias.
    AlreadyPinnedAs { alias: String },
    /// The allowlist could not be parsed, so it is not a file to overwrite.
    Unreadable { reason: String },
}

impl AddRefusal {
    /// One line naming the reason, for a headless caller and for the exit path.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Ticket(error) => error.to_string(),
            Self::KeyMismatch { block } => block.clone(),
            Self::NoTerminal => NO_TERMINAL_REFUSAL.to_owned(),
            Self::Declined => {
                "Cancelled at the confirm. Nothing was written; the peer is not admitted."
                    .to_owned()
            }
            Self::InvalidAlias => {
                "Refusing to pin: the alias must contain a non-whitespace character. Nothing was written."
                    .to_owned()
            }
            Self::AlreadyPinnedAs { alias } => format!(
                "Refusing to pin one peer under a second alias: this key is already pinned as {}. \
                 Nothing was written. Revoke that alias before importing it under a new name.",
                sanitize_for_terminal(alias)
            ),
            Self::Unreadable { reason } => format!(
                "Refusing to change .rustain/p2p.json: its current contents did not parse \
                 ({}). Nothing was written — fix the file first so nothing hand-edited is \
                 overwritten.",
                sanitize_for_terminal(reason)
            ),
        }
    }
}

/// The named reason a non-interactive `peer add` gives.
pub const NO_TERMINAL_REFUSAL: &str = concat!(
    "Refusing to pin without a confirm: `peer add` needs an interactive terminal so ",
    "you can compare the fingerprint out of band, and there is no flag that skips ",
    "that step. Nothing was written. Run it from a terminal, or run `rustain peer ",
    "show <alias>` there first to see what is already pinned."
);

/// Print the confirm card and read one decision from the operator.
///
/// Returns `Ok(true)` only for an explicit `y`. Anything else — `n`, an empty
/// line, EOF, or an unrecognised key — declines. ⛔ There is no default-yes.
pub fn confirm_on_terminal(
    card: &str,
    input: &mut impl BufRead,
    out: &mut impl Write,
) -> std::io::Result<bool> {
    write!(out, "{card}")?;
    out.flush()?;
    let mut answer = String::new();
    if input.read_line(&mut answer)? == 0 {
        return Ok(false);
    }
    Ok(matches!(answer.trim(), "y" | "Y"))
}

/// The line printed after a successful pin.
///
/// A journaled fact plus the one true consequence. ⛔ It does not claim the peer
/// is now reachable, connected, or trustworthy.
#[must_use]
pub fn pinned_text(alias: &str, peer_id: &crate::domain::models::PeerId) -> String {
    let alias = sanitize_for_terminal(alias);
    let short = crate::domain::models::peer_fingerprint(peer_id);
    format!(
        "Pinned {alias} ({short}) in .rustain/p2p.json.\n\
         This says who may reach this host — not that what they send is true.\n\
         Admission is checked per frame, so it takes effect on their next frame\n\
         with no restart. `rustain peer show {alias}` prints the key whole."
    )
}
