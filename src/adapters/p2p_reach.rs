//! The reach store: `.rustain/p2p-reach.json` (Story 18.4d, FR161).
//!
//! # Why this is a sibling file and not a field in `p2p.json`
//!
//! `p2p.json`'s root and nested inputs use `deny_unknown_fields`, so an unknown
//! key there makes the loader return `Malformed` — and a malformed allowlist
//! means *"this host admits no peer."* A **reachability** fact living in the
//! **admission** file could therefore break admission on an older binary, which
//! is the exact inversion of `reach ≠ trust`. Two files is the structural form of
//! that invariant, not a filing preference.
//!
//! # The asymmetry, stated once
//!
//! * A read failure here **degrades**: absent or malformed ⇒ dial nobody, and
//!   `p2p.json` is untouched, byte for byte.
//! * A write failure here **refuses**: an unreadable store is not a store to
//!   overwrite, exactly as `p2p.json`'s writer refuses.
//!
//! # Why this writer round-trips typed values and `p2p.json`'s does not
//!
//! `p2p.json` is hand-authored, so its writer preserves the operator's key
//! spelling, order and omissions through `RawValue`. This file is written only by
//! this module and read only by this module; there is no operator spelling to
//! preserve. What it *does* share is the transaction: the same
//! [`P2pConfigWriteLock`], the same writer-owned `0600` temp, fsync, atomic
//! rename and parent-directory fsync, and the same in-lock re-read and typed
//! schema validation before any mutation.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};

use crate::adapters::p2p_config::{
    P2pConfigWriteLock, load_workspace_p2p_config, write_config_atomically,
};
use crate::domain::models::{
    P2pConfigState, PEER_REACH_SCHEMA_VERSION, PeerId, PeerReach, PeerReachState, PeerReachStore,
    RelaySet,
};
use crate::domain::ports::PeerAddress;
use crate::domain::services::peer_reach_filter::{DialableReach, dialable_reach};
use crate::infrastructure::paths::{workspace_p2p_config_path, workspace_p2p_reach_path};

/// Temporary-file prefix, so a directory listing during a write names the store.
const REACH_TEMP_PREFIX: &str = ".p2p-reach-";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReachDocument {
    version: u32,
    #[serde(default, rename = "self", skip_serializing_if = "Option::is_none")]
    own: Option<ReachEntry>,
    #[serde(default)]
    peers: BTreeMap<String, ReachEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReachEntry {
    /// The opaque adapter-encoded address, base64url so one record is one line
    /// and nothing about its contents is implied by this file's shape.
    address: String,
    #[serde(rename = "capturedAt")]
    captured_at: i64,
}

impl ReachEntry {
    fn from_reach(reach: &PeerReach) -> Self {
        Self {
            address: URL_SAFE_NO_PAD.encode(reach.address.as_bytes()),
            captured_at: reach.captured_at,
        }
    }

    fn into_reach(self) -> Result<PeerReach, String> {
        let bytes = URL_SAFE_NO_PAD
            .decode(self.address.as_bytes())
            .map_err(|error| format!("reach address is not valid base64url: {error}"))?;
        Ok(PeerReach {
            address: PeerAddress::from_bytes(bytes).map_err(|error| error.to_string())?,
            captured_at: self.captured_at,
        })
    }
}

impl ReachDocument {
    fn from_store(store: &PeerReachStore) -> Self {
        Self {
            version: PEER_REACH_SCHEMA_VERSION,
            own: store.own.as_ref().map(ReachEntry::from_reach),
            peers: store
                .peers
                .iter()
                .map(|(alias, reach)| (alias.clone(), ReachEntry::from_reach(reach)))
                .collect(),
        }
    }

    fn into_store(self) -> Result<PeerReachStore, String> {
        if self.version != PEER_REACH_SCHEMA_VERSION {
            return Err(format!(
                "reach store is schema version {}, and this build reads {PEER_REACH_SCHEMA_VERSION}",
                self.version
            ));
        }
        let own = match self.own {
            Some(entry) => Some(entry.into_reach()?),
            None => None,
        };
        let mut peers = BTreeMap::new();
        for (alias, entry) in self.peers {
            if alias.trim().is_empty() {
                return Err("a reach alias must not be empty or whitespace".to_owned());
            }
            peers.insert(alias, entry.into_reach()?);
        }
        Ok(PeerReachStore { own, peers })
    }
}

/// Read the reach store.
///
/// Never fails: an unreadable store is a state, and its consequence is "dial
/// nobody" rather than an error the caller has to invent a policy for.
#[must_use]
pub fn load_workspace_p2p_reach(path: &Path) -> PeerReachState {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return PeerReachState::Absent;
        }
        Err(error) => {
            return PeerReachState::Malformed {
                reason: format!("failed to read {}: {error}", path.display()),
            };
        }
    };
    match serde_json::from_str::<ReachDocument>(&content).map_err(|error| error.to_string()) {
        Ok(document) => match document.into_store() {
            Ok(store) => PeerReachState::Present(store),
            Err(reason) => PeerReachState::Malformed { reason },
        },
        Err(reason) => PeerReachState::Malformed {
            reason: format!("invalid JSON in {}: {reason}", path.display()),
        },
    }
}

/// One read-modify-write transaction over the reach store.
///
/// The in-lock re-read is the point: a concurrent writer may have replaced the
/// file since the caller last looked, and a store that no longer parses must not
/// be clobbered by a mutation computed against the old contents.
fn rewrite_reach(
    path: &Path,
    mutate: impl FnOnce(&mut PeerReachStore) -> Result<bool, String>,
) -> Result<(), String> {
    let _lock = P2pConfigWriteLock::acquire(path)?;
    let mut store = match load_workspace_p2p_reach(path) {
        PeerReachState::Absent => PeerReachStore::default(),
        PeerReachState::Present(store) => store,
        PeerReachState::Malformed { reason } => {
            return Err(format!(
                "refusing to replace {}: its current contents did not parse ({reason})",
                path.display()
            ));
        }
    };
    if !mutate(&mut store)? {
        return Ok(());
    }
    let body = serde_json::to_string_pretty(&ReachDocument::from_store(&store))
        .map_err(|error| format!("encoding the reach store: {error}"))?;
    write_config_atomically(path, REACH_TEMP_PREFIX, &format!("{body}\n"))
}

/// Record this host's own bound address (AC1).
///
/// ⚠ Called **after** a successful bind and never before: a record written
/// before the endpoint exists is a placeholder that a ticket would publish as
/// fact. The endpoint binds an ephemeral port, so this is rewritten every bind
/// and a ticket from a previous run may name a port nobody is listening on.
pub fn publish_self_reach(
    path: &Path,
    address: &PeerAddress,
    captured_at: i64,
) -> Result<(), String> {
    rewrite_reach(path, |store| {
        store.own = Some(PeerReach {
            address: address.clone(),
            captured_at,
        });
        Ok(true)
    })
}

/// Re-record this host's own address, **only when it actually changed**
/// (Story 18.4c, AC3).
///
/// # Why the bind-time write is not enough
///
/// `Endpoint::addr()` reports what is known *now*, and a relay is established
/// after the bind returns — so on a relay-enabled host the bind-time record is
/// relay-less and `peer invite` would mint a ticket naming no relay. The
/// endpoint's own address watcher is the alternative its documentation names;
/// ⛔ `Endpoint::online()` is not, because with no relay configured it pends
/// forever, which is exactly the `disabled` host.
///
/// # Why "only when it changed" is the whole design and not a nicety
///
/// A relay that flaps up/down/up fires the watcher on every WAN twitch. Without
/// this bound the reach store — a **file** — is rewritten and fsynced each
/// time, and a `peer invite` landing mid-flap mints a **freshly signed,
/// unexpired** ticket naming a relay that is currently down. Story 18.4d ate
/// the same bug in a different costume: *a stale self record can mint a
/// freshly signed, unexpired ticket naming a dead port.*
///
/// The comparison happens **inside** the transaction, against what is on disk,
/// so a concurrent writer cannot open a window between the check and the write.
///
/// # Errors
///
/// Returns the reason. Every error means nothing was written.
///
/// Returns `Ok(false)` when the stored address already matched — ⛔ that is a
/// success, not a failure, and no bytes were touched.
pub fn publish_self_reach_on_change(
    path: &Path,
    address: &PeerAddress,
    captured_at: i64,
) -> Result<bool, String> {
    let mut wrote = false;
    rewrite_reach(path, |store| {
        if store
            .own
            .as_ref()
            .is_some_and(|own| &own.address == address)
        {
            return Ok(false);
        }
        store.own = Some(PeerReach {
            address: address.clone(),
            captured_at,
        });
        wrote = true;
        Ok(true)
    })?;
    Ok(wrote)
}

/// Record an imported peer's reach under `alias` (AC3).
///
/// `expected` is the identity the operator confirmed. It is re-checked against
/// `.rustain/p2p.json` **inside** this transaction — with the config lock held
/// across the reach transaction, so a concurrent revoke or re-pin between the
/// confirm and the write cannot leave reach attached to an alias that now
/// names a different key — or to no key at all. Lock order is always config →
/// reach; no path takes them in the other order.
///
/// # Errors
///
/// Returns the reason. Every error means nothing was written.
pub fn record_peer_reach(
    reach_path: &Path,
    config_path: &Path,
    alias: &str,
    expected: &PeerId,
    address: &PeerAddress,
    captured_at: i64,
) -> Result<(), String> {
    if alias.trim().is_empty() {
        return Err("a reach alias must not be empty or whitespace".to_owned());
    }
    let _config_lock = P2pConfigWriteLock::acquire(config_path)?;
    rewrite_reach(reach_path, |store| {
        let pinned = match load_workspace_p2p_config(config_path) {
            P2pConfigState::Present(peers) => peers
                .iter()
                .find(|peer| peer.id == alias)
                .and_then(|peer| peer.pinned_identity()),
            P2pConfigState::Absent => None,
            P2pConfigState::Malformed { reason } => {
                return Err(format!(
                    "refusing to record reach: the allowlist did not parse ({reason})"
                ));
            }
        };
        match pinned {
            Some(on_file) if &on_file == expected => {}
            Some(_) => {
                return Err(format!(
                    "refusing to record reach: {alias:?} now pins a different key"
                ));
            }
            None => {
                return Err(format!(
                    "refusing to record reach: {alias:?} has no pinned key"
                ));
            }
        }
        store.peers.insert(
            alias.to_owned(),
            PeerReach {
                address: address.clone(),
                captured_at,
            },
        );
        Ok(true)
    })
}

/// Drop any reach recorded under `alias` (review 2026-08-15).
///
/// The companion to [`record_peer_reach`] for the honest empty case: a ticket
/// carrying no address confirms a pin but records no reach — and any entry
/// already filed under that alias was recorded against a **previous** key's
/// ticket, so keeping it would let a stale address outlive the trust decision
/// that justified it. Absent entry is not an error.
///
/// # Errors
///
/// Returns the reason. Every error means nothing was written.
pub fn clear_peer_reach(reach_path: &Path, alias: &str) -> Result<(), String> {
    if alias.trim().is_empty() {
        return Err("a reach alias must not be empty or whitespace".to_owned());
    }
    rewrite_reach(reach_path, |store| Ok(store.peers.remove(alias).is_some()))
}

/// What the dial map builder produced.
///
/// It is more than a map because a miss has two very different meanings and the
/// operator surface has to tell them apart: *nothing on file* and *this peer
/// names a relay this host does not use* are different sentences.
#[derive(Clone, Debug, Default)]
pub struct PeerDialMap {
    dialable: HashMap<PeerId, PeerAddress>,
    relay_not_configured: BTreeSet<String>,
}

impl PeerDialMap {
    /// The addresses the transport binds with.
    #[must_use]
    pub fn addresses(&self) -> HashMap<PeerId, PeerAddress> {
        self.dialable.clone()
    }

    /// Take one peer's address out, as `peer ping` does.
    pub fn remove(&mut self, peer: &PeerId) -> Option<PeerAddress> {
        self.dialable.remove(peer)
    }

    /// The address recorded for one peer, if this host may dial them.
    #[must_use]
    pub fn get(&self, peer: &PeerId) -> Option<&PeerAddress> {
        self.dialable.get(peer)
    }

    /// Whether this alias has reach on file whose every address was a relay
    /// outside this host's configured set (D13).
    #[must_use]
    pub fn excluded_by_relay_set(&self, alias: &str) -> bool {
        self.relay_not_configured.contains(alias)
    }

    /// How many peers this host can dial.
    #[must_use]
    pub fn len(&self) -> usize {
        self.dialable.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.dialable.is_empty()
    }
}

/// **The** dial map builder (Story 18.4d AC4; extended by 18.4c AC4).
///
/// One symbol maps persisted reach to the `HashMap<PeerId, PeerAddress>` the
/// transport binds with, and both production consumers — the `peer ping` client
/// bind and the daemon listener bind — call exactly this. ⛔ A second builder,
/// or a map assembled inline at a call site, is the divergence this shape exists
/// to prevent, which is why the relay-set rule was **extended into** it rather
/// than added beside it.
///
/// # Honesty clause
///
/// Populating the daemon's map does **not** make the daemon dial. This cut adds
/// no daemon-initiated dial at all; the daemon is given the map so a future
/// dialer inherits reach without a second wiring story. The only exercised
/// dialer is `peer ping`.
///
/// # Why the allowlist is consulted here
///
/// Reach is keyed by the operator's alias; the transport is keyed by identity.
/// The alias→identity binding lives in `p2p.json`, which means an alias with no
/// pinned key — or one that was revoked — contributes **nothing** to the dial
/// map, and no stale reach record can outlive the trust decision that justified
/// it. ⛔ It does not work the other way round: being in this map admits nobody,
/// and admission is still re-read per frame.
///
/// # Why the relay set is consulted here (D13)
///
/// A relay URL inside a peer's ticket is a **claim**; `relays` is the
/// **fact** — the set this host's endpoint was actually composed with. Applying
/// it at the one place the dial map is built is what makes *"the relay hosts
/// this process contacts are exactly the ones the operator configured"* an
/// invariant rather than an assertion, and it is applied at **dial** rather
/// than at import so an entry recorded before the mode changed cannot be dialed
/// afterwards.
#[must_use]
pub fn peer_dial_map_from_workspace(workspace: &Path, relays: &RelaySet) -> PeerDialMap {
    let mut map = PeerDialMap::default();
    let reach = load_workspace_p2p_reach(&workspace_p2p_reach_path(workspace));
    let Some(store) = reach.store() else {
        return map;
    };
    let P2pConfigState::Present(peers) =
        load_workspace_p2p_config(&workspace_p2p_config_path(workspace))
    else {
        return map;
    };
    for peer in &peers {
        let Some(peer_id) = peer.pinned_identity() else {
            continue;
        };
        let Some(reach) = store.peer(&peer.id) else {
            continue;
        };
        match dialable_reach(&reach.address, relays) {
            DialableReach::Dialable(address) => {
                map.dialable.insert(peer_id, address);
            }
            DialableReach::RelayNotConfigured => {
                map.relay_not_configured.insert(peer.id.clone());
            }
            DialableReach::Nothing => {}
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::p2p_config::pin_peer_in_workspace_config;
    use crate::domain::models::{PeerTicket, PinnedKey};

    fn key(seed: u8) -> PinnedKey {
        PeerTicket::mint(
            &ed25519_dalek::SigningKey::from_bytes(&[seed; 32]),
            Vec::new(),
            i64::MAX,
            None,
        )
        .expect("ticket")
        .offered_key
    }

    fn address(marker: &str) -> PeerAddress {
        PeerAddress::from_bytes(marker.as_bytes().to_vec()).expect("address")
    }

    #[test]
    fn an_absent_store_reads_as_absent_and_dials_nobody() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            load_workspace_p2p_reach(&workspace_p2p_reach_path(dir.path())),
            PeerReachState::Absent
        );
        assert!(peer_dial_map_from_workspace(dir.path(), &RelaySet::empty()).is_empty());
    }

    /// D2: a malformed reach store degrades, and the allowlist it sits beside is
    /// untouched. Mutant: treating this as an admission failure.
    #[test]
    fn a_malformed_store_dials_nobody_and_leaves_admission_byte_identical() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = workspace_p2p_config_path(dir.path());
        pin_peer_in_workspace_config(&config_path, "b", &key(9)).expect("pin");
        let before = std::fs::read(&config_path).expect("read allowlist");

        let reach_path = workspace_p2p_reach_path(dir.path());
        std::fs::write(&reach_path, "{ this is not json").expect("write");
        assert!(matches!(
            load_workspace_p2p_reach(&reach_path),
            PeerReachState::Malformed { .. }
        ));
        assert!(peer_dial_map_from_workspace(dir.path(), &RelaySet::empty()).is_empty());
        assert_eq!(
            std::fs::read(&config_path).expect("read allowlist"),
            before,
            "a reach failure must not touch the allowlist"
        );
        // And a write refuses rather than clobbering it.
        assert!(publish_self_reach(&reach_path, &address("x"), 1).is_err());
    }

    #[test]
    fn a_future_schema_version_is_malformed_rather_than_silently_emptied() {
        let dir = tempfile::tempdir().expect("tempdir");
        let reach_path = workspace_p2p_reach_path(dir.path());
        std::fs::create_dir_all(reach_path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&reach_path, r#"{"version":2,"peers":{}}"#).expect("write");
        assert!(matches!(
            load_workspace_p2p_reach(&reach_path),
            PeerReachState::Malformed { .. }
        ));
    }

    #[test]
    fn the_self_record_round_trips_and_is_owner_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let reach_path = workspace_p2p_reach_path(dir.path());
        publish_self_reach(&reach_path, &address("bundle-bytes"), 4_242).expect("publish");

        let state = load_workspace_p2p_reach(&reach_path);
        let own = state.own().expect("a self record");
        assert_eq!(own.address, address("bundle-bytes"));
        assert_eq!(own.captured_at, 4_242);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&reach_path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "the reach store must not be world-readable");
        }
    }

    /// AC3 in-transaction revalidation. Mutant: dropping the alias check lets
    /// reach attach to an alias whose key changed under it.
    #[test]
    fn recording_reach_revalidates_the_alias_pin_inside_the_transaction() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = workspace_p2p_config_path(dir.path());
        let reach_path = workspace_p2p_reach_path(dir.path());
        let pinned = key(11);
        pin_peer_in_workspace_config(&config_path, "b", &pinned).expect("pin");
        let expected = pinned.peer_id().expect("peer id");

        record_peer_reach(
            &reach_path,
            &config_path,
            "b",
            &expected,
            &address("b-bundle"),
            7,
        )
        .expect("record");
        assert_eq!(
            load_workspace_p2p_reach(&reach_path)
                .store()
                .and_then(|store| store.peer("b"))
                .map(|reach| reach.address.clone()),
            Some(address("b-bundle"))
        );

        // A different identity for the same alias is refused, and nothing moves.
        let other = key(12).peer_id().expect("peer id");
        assert!(
            record_peer_reach(
                &reach_path,
                &config_path,
                "b",
                &other,
                &address("attacker"),
                8
            )
            .is_err()
        );
        assert_eq!(
            load_workspace_p2p_reach(&reach_path)
                .store()
                .and_then(|store| store.peer("b"))
                .map(|reach| reach.address.clone()),
            Some(address("b-bundle"))
        );

        // An unpinned alias is refused too: reach attaches to a pinned identity
        // or to nothing.
        assert!(
            record_peer_reach(
                &reach_path,
                &config_path,
                "ghost",
                &other,
                &address("ghost"),
                9
            )
            .is_err()
        );
    }

    /// AC4 positive control plus the revocation consequence: the builder yields
    /// one entry for a pinned alias, and none once the pin is gone.
    #[test]
    fn the_builder_maps_only_pinned_aliases() {
        use crate::adapters::p2p_config::remove_peer_from_workspace_config;

        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = workspace_p2p_config_path(dir.path());
        let reach_path = workspace_p2p_reach_path(dir.path());
        let pinned = key(13);
        pin_peer_in_workspace_config(&config_path, "b", &pinned).expect("pin");
        let expected = pinned.peer_id().expect("peer id");
        record_peer_reach(
            &reach_path,
            &config_path,
            "b",
            &expected,
            &address("b-bundle"),
            7,
        )
        .expect("record");

        let map = peer_dial_map_from_workspace(dir.path(), &RelaySet::empty());
        assert_eq!(map.len(), 1);
        assert_eq!(map.get(&expected), Some(&address("b-bundle")));

        remove_peer_from_workspace_config(&config_path, "b").expect("revoke");
        assert!(
            peer_dial_map_from_workspace(dir.path(), &RelaySet::empty()).is_empty(),
            "a revoked alias must not stay dialable through a stale reach record"
        );
    }

    /// Review 2026-08-15: an addressless re-pin under a recycled alias must not
    /// inherit the previous key's reach — the empty case clears the entry.
    #[test]
    fn clearing_reach_drops_the_entry_and_absence_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = workspace_p2p_config_path(dir.path());
        let reach_path = workspace_p2p_reach_path(dir.path());
        let pinned = key(14);
        pin_peer_in_workspace_config(&config_path, "b", &pinned).expect("pin");
        let expected = pinned.peer_id().expect("peer id");
        record_peer_reach(
            &reach_path,
            &config_path,
            "b",
            &expected,
            &address("b-bundle"),
            7,
        )
        .expect("record");

        clear_peer_reach(&reach_path, "b").expect("clear");
        assert!(
            load_workspace_p2p_reach(&reach_path)
                .store()
                .and_then(|store| store.peer("b"))
                .is_none(),
            "the stale entry must be gone"
        );
        // Clearing twice is a no-op, and the store still parses.
        clear_peer_reach(&reach_path, "b").expect("clearing nothing is fine");
        assert!(matches!(
            load_workspace_p2p_reach(&reach_path),
            PeerReachState::Present(_)
        ));
    }
}
