use std::path::{Path, PathBuf};

use base64::Engine as _;
use rustain::adapters::p2p_config::{
    P2pConfigState, load_workspace_p2p_config, peer_dial_verdict_from_workspace,
};
use rustain::domain::models::PeerId;
use rustain::domain::services::peer_dial::{PeerDialRefusal, PeerDialVerdict, peer_dial_verdict};
use rustain::infrastructure::paths::{workspace_a2a_config_path, workspace_p2p_config_path};

fn write_config(workspace: &Path, name: &str, contents: &str) {
    let dir = workspace.join(".rustain");
    std::fs::create_dir_all(&dir).expect("create .rustain");
    std::fs::write(dir.join(name), contents).expect("write config");
}

fn pin(seed: u8) -> (String, PeerId) {
    let public_key = [seed; 32];
    let x = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(public_key);
    let peer_id = PeerId::from_public_key(&public_key).expect("valid peer id");
    (x, peer_id)
}

#[test]
fn workspace_peer_config_paths_are_single_sources_of_truth() {
    let workspace = PathBuf::from("/workspace");
    assert_eq!(
        workspace_a2a_config_path(&workspace),
        workspace.join(".rustain/a2a.json")
    );
    assert_eq!(
        workspace_p2p_config_path(&workspace),
        workspace.join(".rustain/p2p.json")
    );
}

#[test]
fn dial_verdict_distinguishes_absent_empty_malformed_and_unpinned() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (_, presented) = pin(7);
    let path = workspace_p2p_config_path(tmp.path());

    let absent = load_workspace_p2p_config(&path);
    assert_eq!(absent, P2pConfigState::Absent);
    assert_eq!(
        peer_dial_verdict(&presented, &absent),
        PeerDialVerdict::Refuse(PeerDialRefusal::ConfigAbsent)
    );

    write_config(tmp.path(), "p2p.json", r#"{"agents":{}}"#);
    let empty = load_workspace_p2p_config(&path);
    assert!(matches!(&empty, P2pConfigState::Present(peers) if peers.is_empty()));
    assert_eq!(
        peer_dial_verdict(&presented, &empty),
        PeerDialVerdict::Refuse(PeerDialRefusal::EmptyAllowlist)
    );

    write_config(tmp.path(), "p2p.json", "{not-json");
    let malformed = load_workspace_p2p_config(&path);
    assert!(matches!(malformed, P2pConfigState::Malformed { .. }));
    assert!(matches!(
        peer_dial_verdict(&presented, &malformed),
        PeerDialVerdict::Refuse(PeerDialRefusal::MalformedConfig { .. })
    ));

    write_config(
        tmp.path(),
        "p2p.json",
        r#"{"agents":{"known-over-http":{}}}"#,
    );
    let unpinned = load_workspace_p2p_config(&path);
    assert_eq!(
        peer_dial_verdict(&presented, &unpinned),
        PeerDialVerdict::Refuse(PeerDialRefusal::MissingPinnedKey {
            peer: "known-over-http".to_owned(),
        })
    );

    let reasons = [
        peer_dial_verdict(&presented, &absent).to_string(),
        peer_dial_verdict(&presented, &empty).to_string(),
        peer_dial_verdict(&presented, &malformed).to_string(),
        peer_dial_verdict(&presented, &unpinned).to_string(),
    ];
    for (index, left) in reasons.iter().enumerate() {
        for right in &reasons[index + 1..] {
            assert_ne!(left, right, "operator-facing refusal reasons collapsed");
        }
    }
}

#[test]
fn dial_verdict_admits_only_the_matching_pinned_peer() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (allowed_x, allowed) = pin(11);
    let (_, drive_by) = pin(12);
    write_config(
        tmp.path(),
        "p2p.json",
        &format!(
            r#"{{"agents":{{"trusted":{{"pinnedKey":{{"alg":"EdDSA","x":"{allowed_x}"}}}}}}}}"#
        ),
    );
    let config = load_workspace_p2p_config(&workspace_p2p_config_path(tmp.path()));

    assert_eq!(peer_dial_verdict(&allowed, &config), PeerDialVerdict::Admit);
    assert_eq!(
        peer_dial_verdict(&drive_by, &config),
        PeerDialVerdict::Refuse(PeerDialRefusal::NotAllowlisted { peer_id: drive_by })
    );
}

/// AC4, review decision D4. A list that carries even one comparable pin can
/// answer "not you" honestly, so an unrelated missing pin must not speak for a
/// stranger's refusal — otherwise `NotAllowlisted` is unreachable in every
/// realistic configuration and the record never names the peer that knocked.
#[test]
fn an_unpinned_entry_does_not_answer_for_an_unknown_peer() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (pinned_x, pinned) = pin(31);
    let (_, drive_by) = pin(37);
    write_config(
        tmp.path(),
        "p2p.json",
        &format!(
            r#"{{"agents":{{"trusted":{{"pinnedKey":{{"alg":"EdDSA","x":"{pinned_x}"}}}},"not-pinned-yet":{{}}}}}}"#
        ),
    );
    let mixed = load_workspace_p2p_config(&workspace_p2p_config_path(tmp.path()));

    assert_eq!(peer_dial_verdict(&pinned, &mixed), PeerDialVerdict::Admit);
    assert_eq!(
        peer_dial_verdict(&drive_by, &mixed),
        PeerDialVerdict::Refuse(PeerDialRefusal::NotAllowlisted {
            peer_id: drive_by.clone()
        }),
        "the refusal must name the peer that presented itself"
    );

    // The unpinned state is not lost: it is what a list with nothing to compare
    // against still says, and it names the entry the operator must fix.
    write_config(
        tmp.path(),
        "p2p.json",
        r#"{"agents":{"not-pinned-yet":{}}}"#,
    );
    let unanswerable = load_workspace_p2p_config(&workspace_p2p_config_path(tmp.path()));
    assert_eq!(
        peer_dial_verdict(&drive_by, &unanswerable),
        PeerDialVerdict::Refuse(PeerDialRefusal::MissingPinnedKey {
            peer: "not-pinned-yet".to_owned()
        })
    );
}

#[test]
fn a2a_http_allow_policy_never_relaxes_the_peer_transport_allowlist() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (_, drive_by) = pin(19);
    write_config(
        tmp.path(),
        "a2a.json",
        r#"{"server":{"admission":"allow"},"agents":{}}"#,
    );

    assert_eq!(
        peer_dial_verdict_from_workspace(tmp.path(), &drive_by),
        PeerDialVerdict::Refuse(PeerDialRefusal::ConfigAbsent),
        "HTTP admission policy must not open the QUIC peer transport"
    );
}
