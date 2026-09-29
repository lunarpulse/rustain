//! Where a peer can be reached, as a durable local fact (Story 18.4d, FR161).
//!
//! # Reach is not trust
//!
//! An entry here makes a peer **dialable**; it never makes one **admitted**.
//! Admission lives in `.rustain/p2p.json` and is re-read per frame. These two
//! facts are deliberately in two files (D1): `p2p.json`'s loader uses
//! `deny_unknown_fields`, so an unknown key there makes the whole allowlist
//! `Malformed`, and a malformed allowlist means *"this host admits no peer"*.
//! Putting a reachability hint in that file would let a *reachability* fact break
//! *admission* on an older binary — the exact inversion of `reach ≠ trust`.
//!
//! The asymmetry is therefore load bearing and is asserted with a mutant: an
//! absent or malformed reach store means **dial nobody**, and leaves admission
//! byte-for-byte unchanged.
//!
//! # The address is opaque
//!
//! A [`PeerAddress`] is adapter-encoded bytes. Nothing in this module inspects
//! them, and no iroh type appears here (NFR74). The one place their contents are
//! read is the import filter
//! ([`crate::domain::services::peer_reach_filter`]), which treats them as
//! untrusted remote input.

use std::collections::BTreeMap;

use crate::domain::ports::PeerAddress;

/// Schema version of `.rustain/p2p-reach.json`.
///
/// A file naming any other version is [`PeerReachState::Malformed`], which
/// degrades to "dial nobody" rather than guessing at a shape this build does not
/// know. ⛔ It must never fall back to treating a future file as empty *and*
/// rewriting it: that would silently discard a newer build's records.
pub const PEER_REACH_SCHEMA_VERSION: u32 = 1;

/// One recorded reach fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerReach {
    /// Adapter-encoded, dialable coordinates.
    pub address: PeerAddress,
    /// When this host wrote the record, unix seconds.
    ///
    /// ⚠ It bounds nothing. The endpoint binds an ephemeral port, so a record
    /// from a previous run may name a port nobody is listening on; a ticket's
    /// `not_after` bounds the *ticket*, never the address. A dead-but-known
    /// address is a `Dial` failure, never `Unreachable`.
    pub captured_at: i64,
}

/// The reach store: this host's own record plus one record per pinned alias.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeerReachStore {
    /// This host's last bound address, written at listener bind.
    ///
    /// Named `own` because `self` is a keyword; it serializes as `"self"`.
    pub own: Option<PeerReach>,
    /// Imported peer reach, keyed by the operator's own alias.
    pub peers: BTreeMap<String, PeerReach>,
}

impl PeerReachStore {
    #[must_use]
    pub fn peer(&self, alias: &str) -> Option<&PeerReach> {
        self.peers.get(alias)
    }
}

/// The four states a reach store can be in, mirroring `P2pConfigState` so the
/// two files are read with the same discipline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeerReachState {
    /// No file. The normal state before a listener has ever bound.
    Absent,
    /// Present and unreadable. Degrades to "dial nobody"; ⛔ never to an
    /// admission failure.
    Malformed { reason: String },
    /// Present and readable.
    Present(PeerReachStore),
}

impl PeerReachState {
    #[must_use]
    pub fn store(&self) -> Option<&PeerReachStore> {
        match self {
            Self::Present(store) => Some(store),
            _ => None,
        }
    }

    /// This host's own recorded address, if one is readable.
    #[must_use]
    pub fn own(&self) -> Option<&PeerReach> {
        self.store()?.own.as_ref()
    }
}
