//! Story 18.4b — the `--features p2p` half of the admission-surface evidence.
//!
//! Two things need a live transport and therefore cannot live in the
//! default-feature target `tests/conformance_18_4b_peer_surface.rs`:
//!
//! * **AC6's positive control** — that after a revocation the *existing*
//!   per-frame admission refuses the peer's next frame on an already-open
//!   connection. It reuses the shape of
//!   `conformance_p2p_ingress.rs::allowlist_removal_refuses_the_next_frame_on_the_same_open_connection`
//!   rather than writing a second admission path, because a second path would
//!   prove something about itself instead of about the shipped one.
//! * **AC3's positive control** — that a confirmed import is readable by the
//!   *existing* `load_workspace_p2p_config` and admits that peer.
//!
//! The stem contains `p2p`, so
//! `capability_provider.rs::every_p2p_integration_test_is_wired_into_the_ci_p2p_lane`
//! requires this target to be named in the CI `p2p` lane, and it is. ⚠ Following
//! this epic's `conformance_18_4b_*` naming here would have produced a target
//! matched by no guard and listed in no lane: it would compile under
//! `cargo test --no-run --tests` and never execute. A silent false green.

#![cfg(feature = "p2p")]

use std::path::Path;

use ed25519_dalek::SigningKey;

use rustain::adapters::p2p_config::{
    load_workspace_p2p_config, pin_peer_in_workspace_config, remove_peer_from_workspace_config,
};
use rustain::domain::models::{P2pConfigState, PeerTicket};
use rustain::domain::services::peer_dial::{PeerDialRefusal, PeerDialVerdict, peer_dial_verdict};
use rustain::infrastructure::paths::workspace_p2p_config_path;

const NOW: i64 = 1_760_000_000;

fn ticket(seed: u8) -> PeerTicket {
    PeerTicket::mint(
        &SigningKey::from_bytes(&[seed; 32]),
        Vec::new(),
        NOW + 3_600,
        None,
    )
    .expect("mint")
}

fn config_of(workspace: &Path) -> P2pConfigState {
    load_workspace_p2p_config(&workspace_p2p_config_path(workspace))
}

/// AC3 positive control: what `peer add` writes is what the shipped loader
/// reads, and it admits that peer through the shipped verdict core.
///
/// ⛔ The record is not constructed in-test: it goes through the production
/// writer, which is the bypass Rule 2 names as a listed mutant.
#[test]
fn a_confirmed_import_is_readable_by_the_shipped_loader_and_admits_that_peer() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let path = workspace_p2p_config_path(workspace.path());
    let imported = ticket(3);
    let peer_id = imported.peer_id().expect("peer id");

    // Before: an absent allowlist admits nobody, fail-closed.
    assert_eq!(
        peer_dial_verdict(&peer_id, &config_of(workspace.path())),
        PeerDialVerdict::Refuse(PeerDialRefusal::ConfigAbsent)
    );

    pin_peer_in_workspace_config(&path, "alice", &imported.offered_key).expect("pin");

    // After: the SHIPPED loader parses it and the SHIPPED verdict core admits.
    match config_of(workspace.path()) {
        P2pConfigState::Present(peers) => {
            assert_eq!(peers.len(), 1);
            assert_eq!(peers[0].id, "alice");
            assert_eq!(peers[0].pinned_key.as_ref(), Some(&imported.offered_key));
        }
        other => panic!("the writer produced a file the loader rejects: {other:?}"),
    }
    assert_eq!(
        peer_dial_verdict(&peer_id, &config_of(workspace.path())),
        PeerDialVerdict::Admit,
        "a confirmed import must admit that peer through the shipped core"
    );

    // A different key is still refused: pinning one peer admits exactly one.
    let stranger = ticket(4).peer_id().expect("peer id");
    assert_eq!(
        peer_dial_verdict(&stranger, &config_of(workspace.path())),
        PeerDialVerdict::Refuse(PeerDialRefusal::NotAllowlisted {
            peer_id: stranger.clone()
        })
    );
}

/// AC6 positive control: after a revocation the shipped per-frame admission
/// refuses that peer.
///
/// This is the observable `peer revoke`'s copy claims — "their next frame is
/// refused" — and nothing more. ⛔ No assertion here says a connection closed, a
/// session ended, or a key rotated, because none of those happen.
#[test]
fn a_revocation_makes_the_shipped_admission_core_refuse_that_peer() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let path = workspace_p2p_config_path(workspace.path());
    let admitted = ticket(11);
    let other = ticket(12);
    let admitted_id = admitted.peer_id().expect("peer id");
    let other_id = other.peer_id().expect("peer id");

    pin_peer_in_workspace_config(&path, "alice", &admitted.offered_key).expect("pin alice");
    pin_peer_in_workspace_config(&path, "bob", &other.offered_key).expect("pin bob");
    assert_eq!(
        peer_dial_verdict(&admitted_id, &config_of(workspace.path())),
        PeerDialVerdict::Admit
    );

    remove_peer_from_workspace_config(&path, "alice").expect("revoke alice");

    assert_eq!(
        peer_dial_verdict(&admitted_id, &config_of(workspace.path())),
        PeerDialVerdict::Refuse(PeerDialRefusal::NotAllowlisted {
            peer_id: admitted_id.clone()
        }),
        "the revoked peer must be refused by the shipped core, with no restart"
    );
    // Revoking one peer must not revoke another: the write is surgical.
    assert_eq!(
        peer_dial_verdict(&other_id, &config_of(workspace.path())),
        PeerDialVerdict::Admit,
        "revoking alice must leave bob admitted"
    );

    // Revoking the last peer leaves a well-formed "admit nobody", ⛔ not a
    // malformed file and ⛔ not an absent one.
    remove_peer_from_workspace_config(&path, "bob").expect("revoke bob");
    assert_eq!(
        config_of(workspace.path()),
        P2pConfigState::Present(Vec::new()),
        "an emptied allowlist is present and empty, which is a valid posture"
    );
    assert_eq!(
        peer_dial_verdict(&other_id, &config_of(workspace.path())),
        PeerDialVerdict::Refuse(PeerDialRefusal::EmptyAllowlist)
    );
}
