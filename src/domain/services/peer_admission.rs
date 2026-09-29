//! Pure decision cores for the transport admission surface (Story 18.4b).
//!
//! Every verb in the `peer` family asks this module *what should happen* before
//! any effect runs. Nothing here reads a file, writes a file, journals, or
//! prints — the callers ([`crate::adapters::cli::peer`] and
//! `infrastructure::runtime::peer_bridge`) own those, and they share these
//! cores so the CLI and the slash command cannot diverge.
//!
//! # The alias is the spine
//!
//! `.rustain/p2p.json`'s `agents` is a map whose **key is the operator's
//! alias**, and [`P2pPeerSpec::id`] is that key. It is the only stable
//! identifier a peer has that is not its key material, so it is the only thing
//! a key change can be detected *against*: a rotated key derives a different
//! [`PeerId`] (SHA-256 of the public key), so a roster keyed on `PeerId` alone
//! turns every import into a new entry and makes a mismatch impossible to
//! observe (ruling A2).
//!
//! # Deliberately a `P2pPeerSpec` twin, not a shared resolver (ruling A9)
//!
//! `room_bridge.rs::resolve_configured_peer_target` has the same *shape* and
//! resolves over `A2aPeerSpec` — the `.rustain/a2a.json` roster. Reusing it
//! here would let an alias that exists only in the A2A config resolve and be
//! "revoked" from a transport roster it was never in, which is exactly the
//! cross-contamination `ADR-18-4-01` D4 keeps the two files apart to prevent.
//! [`resolve_configured_p2p_target`] therefore resolves over `P2pPeerSpec`
//! only, and no `A2aPeerSpec` value reaches any `peer` verb.

// `PinnedKey` comes through the `domain::models` re-export, not the defining
// module path: the domain-purity ratchet forbids the token `a2a` in a
// `src/domain` import line, and the pin type is declared beside the A2A spec.
use crate::domain::models::{P2pConfigState, P2pPeerSpec, PeerId, PinnedKey};

/// The shipped doctrine sentence, single-sourced.
///
/// It is **not** this story's to invent: it is already operator copy on the
/// consent card (`transparency_bridge.rs::consent_card_text`) and it is the
/// literal behaviour of `peer_dial_verdict` — a different key derives a
/// different `PeerId`, matches no pin, and is refused. AC4's alarm and the
/// consent card now interpolate this one constant, so the two surfaces cannot
/// drift into two doctrines.
///
/// Lowercase and unpunctuated so both call sites can place it in their own
/// sentence.
pub const ROTATED_KEY_DOCTRINE: &str = "a rotated key is a new peer";

/// What a ticket import should do, decided before anything is written.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PeerImportVerdict {
    /// The alias is new (or the roster is empty/absent): pin it.
    Pin,
    /// The alias already carries this cryptographic identity. Idempotent no-op
    /// — write nothing and say nothing changed.
    AlreadyPinned,
    /// This identity is already pinned under a different alias. One
    /// cryptographic peer has one transport alias; renaming is revoke then add.
    AlreadyPinnedAs { alias: String },
    /// An empty or whitespace-only alias cannot be loaded back from
    /// `.rustain/p2p.json`, so it must never reach the writer.
    InvalidAlias,
    /// The alias exists with a **different** key. This is the alarm (AC4): the
    /// prior pin stands, nothing is written, and no accept affordance exists.
    KeyMismatch { on_file: PinnedKey },
    /// The roster could not be parsed. Refuse rather than clobber a
    /// hand-edited file whose contents are unknown.
    RefuseUnreadable { reason: String },
}

/// Decide what importing `offered` under `alias` should do.
#[must_use]
pub fn peer_import_verdict(
    alias: &str,
    offered: &PinnedKey,
    config: &P2pConfigState,
) -> PeerImportVerdict {
    if alias.trim().is_empty() {
        return PeerImportVerdict::InvalidAlias;
    }
    let peers = match config {
        P2pConfigState::Absent => return PeerImportVerdict::Pin,
        P2pConfigState::Malformed { reason } => {
            return PeerImportVerdict::RefuseUnreadable {
                reason: reason.clone(),
            };
        }
        P2pConfigState::Present(peers) => peers,
    };
    if let Some(existing) = peers.iter().find(|peer| peer.id == alias) {
        return match existing.pinned_key.as_ref() {
            Some(on_file) if same_pinned_identity(on_file, offered) => {
                PeerImportVerdict::AlreadyPinned
            }
            Some(on_file) => PeerImportVerdict::KeyMismatch {
                on_file: on_file.clone(),
            },
            None => peers
                .iter()
                .find(|peer| {
                    peer.id != alias
                        && peer
                            .pinned_key
                            .as_ref()
                            .is_some_and(|key| same_pinned_identity(key, offered))
                })
                .map_or(PeerImportVerdict::Pin, |peer| {
                    PeerImportVerdict::AlreadyPinnedAs {
                        alias: peer.id.clone(),
                    }
                }),
        };
    }
    peers
        .iter()
        .find(|peer| {
            peer.pinned_key
                .as_ref()
                .is_some_and(|key| same_pinned_identity(key, offered))
        })
        .map_or(PeerImportVerdict::Pin, |peer| {
            PeerImportVerdict::AlreadyPinnedAs {
                alias: peer.id.clone(),
            }
        })
}

fn same_pinned_identity(left: &PinnedKey, right: &PinnedKey) -> bool {
    left.alg == right.alg && left.x == right.x
}

/// What a revocation should do, decided before anything is written.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PeerRevokeVerdict {
    /// Remove this alias. `peer_id` is present only when the entry carried a
    /// pinned key — a revocation never manufactures identity (ruling A9).
    Remove {
        alias: String,
        peer_id: Option<PeerId>,
    },
    /// Nothing is recorded for this target. Write nothing.
    NothingRecorded,
    /// The roster could not be parsed. Refuse.
    RefuseUnreadable { reason: String },
}

/// Decide what revoking `target` should do. `target` is an alias or a
/// configured `PeerId` — never a syntactically valid phantom.
#[must_use]
pub fn peer_revoke_verdict(target: &str, config: &P2pConfigState) -> PeerRevokeVerdict {
    let peers = match config {
        P2pConfigState::Absent => return PeerRevokeVerdict::NothingRecorded,
        P2pConfigState::Malformed { reason } => {
            return PeerRevokeVerdict::RefuseUnreadable {
                reason: reason.clone(),
            };
        }
        P2pConfigState::Present(peers) => peers,
    };
    match resolve_configured_p2p_target(target, peers) {
        Some(spec) => PeerRevokeVerdict::Remove {
            alias: spec.id.clone(),
            peer_id: spec.pinned_identity(),
        },
        None => PeerRevokeVerdict::NothingRecorded,
    }
}

/// Resolve an operator-typed target against the **transport** roster only.
///
/// Accepts the configured alias, or the `PeerId` a configured pin derives.
/// Returns `None` for anything else, including a well-formed `PeerId` that no
/// entry derives — resolving that would manufacture a target.
#[must_use]
pub fn resolve_configured_p2p_target<'a>(
    target: &str,
    peers: &'a [P2pPeerSpec],
) -> Option<&'a P2pPeerSpec> {
    if let Some(exact_alias) = peers.iter().find(|spec| spec.id == target) {
        return Some(exact_alias);
    }
    let parsed = PeerId::parse(target.to_owned()).ok()?;
    peers
        .iter()
        .find(|spec| spec.pinned_identity().as_ref() == Some(&parsed))
}

/// One roster row. Carries only facts the domain holds: the alias, the pinned
/// identity when there is one, and nothing else. ⛔ No `last_seen`, no
/// connection status, no tier — a column with no producer is omitted, never
/// zeroed (`UX-DR-PT-04`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerRosterRow {
    pub alias: String,
    /// `None` when the entry carries no pinned key: *unanswerable*, so this
    /// entry admits nobody.
    pub peer_id: Option<PeerId>,
}

impl PeerRosterRow {
    /// A pinned entry is one that can admit its peer. `pinned` is the operator
    /// word for it; the shipped `TrustTier::Verified` spelling is a banned
    /// operator word and never reaches copy (ruling A1).
    #[must_use]
    pub fn is_pinned(&self) -> bool {
        self.peer_id.is_some()
    }
}

/// The four allowlist input states, kept distinct (preflight P8).
///
/// `Empty` is a **well-formed "admit nobody"**, ⛔ not an error and not a
/// misconfiguration — a valid, deliberate posture.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PeerRoster {
    Absent,
    Empty,
    Unreadable { reason: String },
    Populated(Vec<PeerRosterRow>),
}

/// Project the config read into the roster view the surfaces render.
#[must_use]
pub fn peer_roster(config: &P2pConfigState) -> PeerRoster {
    match config {
        P2pConfigState::Absent => PeerRoster::Absent,
        P2pConfigState::Malformed { reason } => PeerRoster::Unreadable {
            reason: reason.clone(),
        },
        P2pConfigState::Present(peers) if peers.is_empty() => PeerRoster::Empty,
        P2pConfigState::Present(peers) => PeerRoster::Populated(
            peers
                .iter()
                .map(|peer| PeerRosterRow {
                    alias: peer.id.clone(),
                    peer_id: peer.pinned_identity(),
                })
                .collect(),
        ),
    }
}

/// Find one roster entry by alias, for `peer show`.
#[must_use]
pub fn configured_peer_by_alias<'a>(
    alias: &str,
    peers: &'a [P2pPeerSpec],
) -> Option<&'a P2pPeerSpec> {
    peers.iter().find(|spec| spec.id == alias)
}
