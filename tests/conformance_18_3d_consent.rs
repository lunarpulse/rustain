use rustain::adapters::policy::JournalConsentProjection;
use rustain::domain::models::{JournalEntry, JournalRecord, PeerId, RoomEvent};
use rustain::domain::ports::{ConsentProjectionQuery, ConsentState};
use rustain::domain::services::transparency::{TransparencyKind, fold_transparency};

fn sender(byte: u8) -> PeerId {
    PeerId::from_public_key(&[byte; 32]).expect("32-byte Ed25519 key")
}

fn entry(seq: u64, event: RoomEvent) -> JournalEntry {
    JournalEntry::new(seq, JournalRecord::Room(event), seq as i64 * 10)
}

#[test]
fn consent_event_fixtures_round_trip_and_missing_sender_fails_closed() {
    let peer = sender(1);
    for event in [
        RoomEvent::ConsentGranted {
            sender: Some(peer.clone()),
            granted_at: 10,
        },
        RoomEvent::ConsentRevoked {
            sender: Some(peer.clone()),
            revoked_at: 20,
        },
    ] {
        let json = serde_json::to_string(&event).unwrap();
        let replayed: RoomEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(replayed, event);
    }

    let fieldless: RoomEvent =
        serde_json::from_str(r#"{"event":"consent_granted","granted_at":10}"#).unwrap();
    assert!(matches!(
        fieldless,
        RoomEvent::ConsentGranted { sender: None, .. }
    ));
    let old_event: RoomEvent = serde_json::from_str(
        r#"{"event":"peer_draft_resolved","node":"n","agent_composed":false,"sent":false}"#,
    )
    .unwrap();
    assert!(matches!(old_event, RoomEvent::PeerDraftResolved { .. }));
}

#[test]
fn replay_projection_is_latest_act_idempotent_and_never_synthesizes_unknown_sender() {
    let alice = sender(2);
    let unknown = sender(3);
    let entries = vec![
        entry(
            1,
            RoomEvent::ConsentGranted {
                sender: Some(alice.clone()),
                granted_at: 10,
            },
        ),
        entry(
            2,
            RoomEvent::ConsentGranted {
                sender: Some(alice.clone()),
                granted_at: 11,
            },
        ),
        entry(
            3,
            RoomEvent::ConsentRevoked {
                sender: Some(alice.clone()),
                revoked_at: 12,
            },
        ),
        entry(
            4,
            RoomEvent::ConsentRevoked {
                sender: Some(unknown.clone()),
                revoked_at: 13,
            },
        ),
    ];

    let projection = JournalConsentProjection::from_entries(&entries);
    assert_eq!(projection.known_senders(), vec![alice.clone()]);
    assert_eq!(projection.consent_for(&alice), ConsentState::Revoked);
    assert_eq!(projection.consent_for(&unknown), ConsentState::None);
}

#[test]
fn team_log_projection_renders_grant_and_revocation_on_the_existing_spine() {
    let peer = sender(4);
    let entries = vec![
        entry(
            1,
            RoomEvent::ConsentGranted {
                sender: Some(peer.clone()),
                granted_at: 10,
            },
        ),
        entry(
            2,
            RoomEvent::ConsentRevoked {
                sender: Some(peer),
                revoked_at: 20,
            },
        ),
    ];

    let rows = fold_transparency(&entries);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].kind, TransparencyKind::ConsentGranted);
    assert_eq!(rows[1].kind, TransparencyKind::ConsentRevoked);
    assert!(rows[0].one_line().contains("consent-granted"));
    assert!(rows[1].one_line().contains("consent-revoked"));
}

#[test]
fn policy_explainer_consumes_real_trusted_and_revoked_projection_states() {
    let workspace = tempfile::TempDir::new().unwrap();
    let trusted = sender(5);
    let revoked = sender(6);
    let entries = vec![
        entry(
            1,
            RoomEvent::ConsentGranted {
                sender: Some(trusted.clone()),
                granted_at: 10,
            },
        ),
        entry(
            2,
            RoomEvent::ConsentGranted {
                sender: Some(revoked.clone()),
                granted_at: 11,
            },
        ),
        entry(
            3,
            RoomEvent::ConsentRevoked {
                sender: Some(revoked.clone()),
                revoked_at: 12,
            },
        ),
    ];
    let projection = JournalConsentProjection::from_entries(&entries);

    let (_, explanation) =
        rustain::adapters::policy::resolve_workspace_policy(workspace.path(), &[], &projection)
            .unwrap();
    assert!(
        explanation
            .consent
            .iter()
            .any(|line| { line.sender == trusted.as_str() && line.state == ConsentState::Trusted })
    );
    assert!(
        explanation
            .consent
            .iter()
            .any(|line| { line.sender == revoked.as_str() && line.state == ConsentState::Revoked })
    );
    assert!(
        !explanation
            .rows
            .iter()
            .any(|row| row.detail.contains("no journaled consent grants recorded"))
    );
}

#[test]
fn production_wiring_uses_the_journal_projection_and_central_a2a_gate() {
    let daemon = std::fs::read_to_string("src/adapters/daemon/mod.rs").unwrap();
    let startup = std::fs::read_to_string("src/adapters/daemon/policy_startup.rs").unwrap();
    let doctor = std::fs::read_to_string("src/adapters/cli/doctor/policy_check.rs").unwrap();
    let a2a = std::fs::read_to_string("src/adapters/a2a/server.rs").unwrap();
    let startup_production = startup.split("#[cfg(test)]").next().unwrap();

    assert!(daemon.contains("JournalConsentProjection::load_workspace"));
    assert!(daemon.contains("consent_projection.as_ref()"));
    assert!(!startup_production.contains("EmptyConsentProjection"));
    assert!(!doctor.contains("EmptyConsentProjection"));
    assert!(a2a.contains("runtime.enforces_sender_consent()"));
    assert!(daemon.contains("new_with_node_tree_bus_policy_and_journal"));
}
