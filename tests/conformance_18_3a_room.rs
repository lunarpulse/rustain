//! Story 18.3a conformance — durable room viewer, room roles, host-bound honesty.
//!
//! Wired into CI's Check job (`.github/workflows/ci.yml`); a conformance file
//! that is green locally and unenforced is the 18.3d shipped defect.
//!
//! Deliberately **not** named `*a2a*`: `every_a2a_integration_test_is_wired_into
//! _the_ci_a2a_lane` (`src/domain/ports/capability_provider.rs`) fails the
//! default lane for an unlisted `tests/*a2a*.rs`.

use rustain::domain::models::{
    AgentId, HostBinding, JournalEntry, JournalRecord, NodeOrigin, OrchestrationRoom,
    OrchestrationRoomId, PeerId, RoomEditDecision, RoomEditKind, RoomEvent, RoomRole,
};
use rustain::domain::services::room_role::room_edit_decision;
use rustain::domain::services::transparency::{TransparencyKind, fold_transparency};

fn peer(byte: u8) -> PeerId {
    PeerId::from_public_key(&[byte; 32]).expect("32-byte Ed25519 key")
}

fn entry(seq: u64, event: RoomEvent) -> JournalEntry {
    JournalEntry::new(seq, JournalRecord::Room(event), seq as i64 * 10)
}

/// AC3 keystone — the full `RoomRole × RoomEditKind` cross-product, including
/// `Unknown`, driven through `room_edit_decision` rather than re-derived here
/// (`architecture.md` "do not re-implement the core in the test body").
///
/// Mutants this must turn RED:
///   1. `Viewer` returns `Allow` for a role edit.
///   3. `room_edit_decision(Unknown, _)` returns `Allow`.
#[test]
fn room_edit_decision_covers_every_role_and_edit_kind() {
    use RoomEditDecision::{Allow, Deny};
    use RoomEditKind::{DurableContent, RoleAssignment};
    use RoomRole::{Editor, Owner, Unknown, Viewer};

    let table = [
        // Positive control: Owner may make role edits.
        (Owner, RoleAssignment, Allow),
        (Owner, DurableContent, Allow),
        // Positive control: Editor may make a NON-role room edit.
        (Editor, RoleAssignment, Deny),
        (Editor, DurableContent, Allow),
        // A viewer cannot mutate.
        (Viewer, RoleAssignment, Deny),
        (Viewer, DurableContent, Deny),
        // A role this build does not understand grants nothing.
        (Unknown, RoleAssignment, Deny),
        (Unknown, DurableContent, Deny),
    ];

    for (role, edit, expected) in table {
        assert_eq!(
            room_edit_decision(role, edit),
            expected,
            "room_edit_decision({role:?}, {edit:?})"
        );
    }
}

/// AC3 third mutant — least privilege is the only safe default. A
/// `room_role_granted` line whose `role` field is missing or unreadable must
/// grant `Viewer`, never `Owner`.
#[test]
fn room_role_defaults_and_absorbs_unknown_wire_values_without_widening() {
    assert_eq!(RoomRole::default(), RoomRole::Viewer);
    let future: RoomRole = serde_json::from_str(r#""archivist""#).expect("future role parses");
    assert_eq!(future, RoomRole::Unknown);
    assert_eq!(
        room_edit_decision(future, RoomEditKind::RoleAssignment),
        RoomEditDecision::Deny
    );
}

/// AC3 second mutant, structural (Rule 4) — no behavioural test can
/// exhaustively prove "a room role never widens execution authority", so a
/// negative source ratchet does it: the capability/authority seams must not
/// mention `RoomRole` at all.
///
/// **Extended by Story 18.3a-b (AC3 ratchet #2)** with the addressing
/// vocabulary. The same argument applies verbatim: a ticket addressee is a
/// durable attribution and a room role is a room-edit permission; neither may
/// contribute to a `CapabilityToken`, an `AuthorityProvider` decision, or an
/// approval fingerprint. Mutant: reference `TicketAddressee` from
/// `capability_token.rs` → this fires.
///
/// ⚑ Extend this needle list rather than adding a second test — one ratchet,
/// one place to look.
#[test]
fn room_role_never_reaches_the_capability_or_authority_seams() {
    for relative in [
        "/src/domain/models/capability_token.rs",
        "/src/domain/ports/authority_provider.rs",
    ] {
        let path = format!("{}{relative}", env!("CARGO_MANIFEST_DIR"));
        let source = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        for needle in ["RoomRole", "TicketAddressee", "local_operator"] {
            assert!(
                !source.contains(needle),
                "{relative} references {needle} — a room role and a ticket addressee must never \
                 contribute to a CapabilityToken, an AuthorityProvider decision, or an approval \
                 fingerprint (epics.md contracted scope: roles govern room edits only; FR152: \
                 addressing a human is a zero-authority attribution)"
            );
        }
    }
}

// ─────────────────────────────── AC4 · Task 2 ───────────────────────────────

/// OPEN-DR-3's first obligation: do not add a variant without exercising the
/// `Unrecognized` fallback. Also pins every field as `#[serde(default)]` — 18.3c
/// shipped a variant with one field missing the attribute, asymmetrically.
///
/// Fourth mutant: an unknown `event` tag fails the whole journal load.
#[test]
fn room_role_events_round_trip_and_absent_fields_fail_closed_to_least_privilege() {
    for event in [
        RoomEvent::RoomRoleGranted {
            peer: Some(peer(1)),
            role: RoomRole::Editor,
            granted_at: 10,
        },
        RoomEvent::RoomRoleRevoked {
            peer: Some(peer(1)),
            revoked_at: 20,
        },
    ] {
        let json = serde_json::to_string(&event).expect("serialize");
        let back: RoomEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(event, back, "round-trip must be lossless: {json}");
    }

    // Every field absent → identity is `None` (never synthesized) and the role
    // falls to Viewer, never Owner.
    let fieldless: RoomEvent =
        serde_json::from_str(r#"{"event":"room_role_granted"}"#).expect("fieldless grant parses");
    assert_eq!(
        fieldless,
        RoomEvent::RoomRoleGranted {
            peer: None,
            role: RoomRole::Viewer,
            granted_at: 0,
        }
    );
    let fieldless_revoke: RoomEvent =
        serde_json::from_str(r#"{"event":"room_role_revoked"}"#).expect("fieldless revoke parses");
    assert_eq!(
        fieldless_revoke,
        RoomEvent::RoomRoleRevoked {
            peer: None,
            revoked_at: 0,
        }
    );
    // A role string a newer build wrote is absorbed, not fatal, and denies.
    let future: RoomEvent =
        serde_json::from_str(r#"{"event":"room_role_granted","role":"archivist"}"#)
            .expect("future role parses");
    let RoomEvent::RoomRoleGranted { role, .. } = future else {
        panic!("expected a grant");
    };
    assert_eq!(role, RoomRole::Unknown);
    assert_eq!(
        room_edit_decision(role, RoomEditKind::RoleAssignment),
        RoomEditDecision::Deny
    );

    // An entirely unknown tag folds to `Unrecognized` instead of failing the
    // line — and a whole journal containing one still loads.
    let unknown: RoomEvent = serde_json::from_str(r#"{"event":"room_role_transferred","to":"x"}"#)
        .expect("an unknown tag must not fail the load");
    assert_eq!(unknown, RoomEvent::Unrecognized);
    let line = serde_json::to_string(&entry(1, unknown)).expect("serialize entry");
    let parsed: JournalEntry = serde_json::from_str(&line).expect("journal line still parses");
    assert!(matches!(
        parsed.record,
        JournalRecord::Room(RoomEvent::Unrecognized)
    ));
}

/// NFR70(d) — the room read model is unchanged by role events, and replaying
/// the same stream twice yields an identical room. The role fact has exactly
/// one fold (`JournalRoomRoleProjection`); putting it in the node read model
/// too would be two read models for one fact.
#[test]
fn role_events_are_room_read_model_no_ops_and_replay_idempotently() {
    let node = AgentId::parse("a2a-n1").expect("valid agent id");
    let registration = RoomEvent::NodeRegistered {
        node,
        origin: NodeOrigin::Remote,
        host: HostBinding::new("host-A", "ws"),
    };
    let events = vec![
        registration.clone(),
        RoomEvent::RoomRoleGranted {
            peer: Some(peer(2)),
            role: RoomRole::Editor,
            granted_at: 1,
        },
        RoomEvent::RoomRoleRevoked {
            peer: Some(peer(2)),
            revoked_at: 2,
        },
    ];
    let id = OrchestrationRoomId::parse("room-idem").expect("room id");
    let baseline = OrchestrationRoom::project_for_host(id.clone(), [registration], "host-A");
    let once = OrchestrationRoom::project_for_host(id.clone(), events.clone(), "host-A");
    let twice = OrchestrationRoom::project_for_host(
        id,
        events.iter().chain(events.iter()).cloned(),
        "host-A",
    );
    assert_eq!(
        once, baseline,
        "role events must not alter the room read model"
    );
    assert_eq!(
        twice, baseline,
        "repeated role events remain read-model no-ops"
    );
    assert_eq!(once.nodes().len(), 1);
}

/// **Trap 2, in executable form.** Three revocations, three meanings: a
/// `ConsentRevoked` and a `RoomRoleRevoked` must never render as the same row.
/// A fold that cannot distinguish "lost edit rights on a room" from "standing
/// delivery consent withdrawn" is a fold that lies to `/team log`.
#[test]
fn the_revocations_render_as_distinct_transparency_rows() {
    let entries = vec![
        entry(
            1,
            RoomEvent::RoomRoleGranted {
                peer: Some(peer(3)),
                role: RoomRole::Editor,
                granted_at: 10,
            },
        ),
        entry(
            2,
            RoomEvent::RoomRoleRevoked {
                peer: Some(peer(3)),
                revoked_at: 20,
            },
        ),
        entry(
            3,
            RoomEvent::ConsentRevoked {
                sender: Some(peer(3)),
                revoked_at: 30,
            },
        ),
    ];
    let rows = fold_transparency(&entries);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].kind, TransparencyKind::RoomRoleGranted);
    assert_eq!(rows[1].kind, TransparencyKind::RoomRoleRevoked);
    assert_eq!(rows[2].kind, TransparencyKind::ConsentRevoked);
    assert!(rows[0].one_line().contains("room-role-granted"));
    assert!(rows[0].one_line().contains("editor"));
    assert!(rows[1].one_line().contains("room-role-revoked"));
    assert!(rows[2].one_line().contains("consent-revoked"));
    assert_ne!(
        rows[1].kind, rows[2].kind,
        "a room-role revocation and a delivery-consent revocation are different facts"
    );
    // ⛔ No rekey consequence, and no enforcement claim.
    for row in &rows {
        let line = row.one_line().to_ascii_lowercase();
        for forbidden in ["rekey", "key rotation", "can no longer read", "excluded"] {
            assert!(!line.contains(forbidden), "forbidden wording in: {line}");
        }
    }
}

// ─────────────────────────────── AC4 · Task 3 ───────────────────────────────

/// **The keystone Story 18.3d was missing**, at the projection layer: grant →
/// visible → revoke → gone, in the **same live process, with no restart**,
/// against a real on-disk journal read through the production
/// `WorkspaceJournalReader`.
///
/// First mutant: hold the startup projection and never refold → the revoke is
/// inert until restart. That is 18.3d's exact shipped defect and this test is
/// what catches it.
#[tokio::test]
async fn the_role_projection_refolds_live_without_a_restart() {
    use rustain::adapters::policy::JournalRoomRoleProjection;
    use rustain::domain::ports::{RoomRoleProjectionQuery, RoomRoleState};
    use rustain::infrastructure::subagent::NodeJournal;
    use rustain::infrastructure::subagent::node_journal::WorkspaceJournalReader;

    let workspace = tempfile::tempdir().expect("workspace");
    let journal = NodeJournal::open_workspace(workspace.path())
        .await
        .expect("open journal");
    let reader: std::sync::Arc<dyn rustain::domain::ports::RoomJournalReader> =
        std::sync::Arc::new(WorkspaceJournalReader::open_workspace(workspace.path()));

    // ONE long-lived holder for the whole test. Nothing below reconstructs it.
    let projection = JournalRoomRoleProjection::with_reader(reader);
    projection.refresh().await;
    let target = peer(7);
    assert_eq!(projection.role_for(&target), RoomRole::Viewer);
    assert!(projection.journaled_roles().is_empty());

    // Grant, appended durable-first by a writer that is not the holder.
    journal
        .append_room(RoomEvent::RoomRoleGranted {
            peer: Some(target.clone()),
            role: RoomRole::Editor,
            granted_at: 1_000,
        })
        .await
        .expect("append grant");
    projection.refresh().await;
    assert_eq!(
        projection.role_for(&target),
        RoomRole::Editor,
        "the grant must be visible without a restart"
    );
    assert_eq!(
        projection.journaled_roles(),
        vec![(target.clone(), RoomRoleState::Granted(RoomRole::Editor))]
    );

    // Revoke — the assertion that the projection's ANSWER CHANGES live.
    journal
        .append_room(RoomEvent::RoomRoleRevoked {
            peer: Some(target.clone()),
            revoked_at: 2_000,
        })
        .await
        .expect("append revoke");
    projection.refresh().await;
    assert_eq!(
        projection.role_for(&target),
        RoomRole::Viewer,
        "the revocation must take effect in the SAME process — no restart"
    );
    assert_eq!(
        projection.journaled_roles(),
        vec![(target.clone(), RoomRoleState::Revoked)]
    );

    // A duplicate revoke is a no-op.
    journal
        .append_room(RoomEvent::RoomRoleRevoked {
            peer: Some(target.clone()),
            revoked_at: 3_000,
        })
        .await
        .expect("append duplicate revoke");
    projection.refresh().await;
    assert_eq!(
        projection.journaled_roles(),
        vec![(target.clone(), RoomRoleState::Revoked)]
    );

    // Re-granting works: the fold is latest-act-per-peer, not write-once.
    journal
        .append_room(RoomEvent::RoomRoleGranted {
            peer: Some(target.clone()),
            role: RoomRole::Owner,
            granted_at: 4_000,
        })
        .await
        .expect("append re-grant");
    projection.refresh().await;
    assert_eq!(projection.role_for(&target), RoomRole::Owner);

    // NFR70(d): a full independent replay of the same durable stream produces
    // the identical answer — the live refold is not accumulating state.
    let replayed = JournalRoomRoleProjection::from_entries(
        &rustain::domain::ports::RoomJournalReader::load_entries(
            &WorkspaceJournalReader::open_workspace(workspace.path()),
        )
        .await
        .expect("reload"),
    );
    assert_eq!(replayed.journaled_roles(), projection.journaled_roles());
}

/// Trap 3's deliberate divergence: the consent precedent proceeds on cached
/// state after a read failure; a **role** projection must not, because a stale
/// snapshot answering `Owner` after a revoke is the defect itself.
#[tokio::test]
async fn a_journal_read_failure_drops_every_role_to_least_privilege() {
    use rustain::adapters::policy::JournalRoomRoleProjection;
    use rustain::domain::ports::{RoomJournalError, RoomJournalReader, RoomRoleProjectionQuery};

    struct Flaky {
        fail: std::sync::atomic::AtomicBool,
        entries: Vec<JournalEntry>,
    }

    #[async_trait::async_trait]
    impl RoomJournalReader for Flaky {
        async fn load_entries(&self) -> Result<Vec<JournalEntry>, RoomJournalError> {
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(RoomJournalError::Read("disk went away".to_owned()));
            }
            Ok(self.entries.clone())
        }
    }

    let owner = peer(8);
    let reader = std::sync::Arc::new(Flaky {
        fail: std::sync::atomic::AtomicBool::new(false),
        entries: vec![entry(
            1,
            RoomEvent::RoomRoleGranted {
                peer: Some(owner.clone()),
                role: RoomRole::Owner,
                granted_at: 1,
            },
        )],
    });
    let projection = JournalRoomRoleProjection::with_reader(reader.clone());
    projection.refresh().await;
    assert_eq!(projection.role_for(&owner), RoomRole::Owner);
    assert!(projection.read_error().is_none());

    reader.fail.store(true, std::sync::atomic::Ordering::SeqCst);
    projection.refresh().await;
    assert_eq!(
        projection.role_for(&owner),
        RoomRole::Viewer,
        "fail-closed: a projection that cannot read the journal must not keep answering Owner"
    );
    assert!(
        projection.read_error().is_some(),
        "the failure must be surfaced, not swallowed"
    );

    // And it recovers once the journal is readable again, even though the
    // high-water mark did not advance.
    reader
        .fail
        .store(false, std::sync::atomic::Ordering::SeqCst);
    projection.refresh().await;
    assert_eq!(projection.role_for(&owner), RoomRole::Owner);
    assert!(projection.read_error().is_none());
}

// ──────────────────────────── AC1 / AC2 · Tasks 4–5 ─────────────────────────

fn event_loop_source() -> String {
    std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/infrastructure/runtime/event_loop.rs"
    ))
    .expect("read event_loop.rs")
}

fn room_bridge_source() -> String {
    std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/infrastructure/runtime/room_bridge.rs"
    ))
    .expect("read room_bridge.rs")
}

/// **AC1 layer (a), the mutant that is the whole point.** Remove the `room`
/// entry from the `submit_message` allowlist and `/room` falls through to
/// `SubmitWithContext`, resolves no command file, and silently never runs —
/// the exact 14.3c failure `/fanout` shipped with. A registry-presence
/// assertion stays green while the command is dead; this does not.
#[test]
fn slash_room_is_routed_as_an_execute_command_not_a_missing_custom_command() {
    use rustain::adapters::tui::app::{InputAction, submit_message_for_test};
    use rustain::adapters::tui::state::TuiState;

    for input in [
        "/room",
        "/room role list",
        "/room role grant alice editor",
        "/room role revoke alice",
    ] {
        let mut state = TuiState::new(120, 24);
        state.input_buffer = input.to_owned();
        match submit_message_for_test(&mut state) {
            InputAction::ExecuteCommand { name, args } => {
                assert_eq!(name, "room", "{input}");
                let tail = input.strip_prefix("/room").unwrap().trim();
                if tail.is_empty() {
                    assert!(args.is_none(), "{input} → {args:?}");
                } else {
                    assert_eq!(args.as_deref(), Some(tail), "{input}");
                }
            }
            other => panic!("`{input}` must reach the /room dispatch arm, got {other:?}"),
        }
    }
}

/// The chord is real, not a `Noop`, and it is lower-case so `lookup_chord`
/// (which lowercases) can actually find it.
#[test]
fn ctrl_x_r_dispatches_the_durable_room_panel() {
    use rustain::adapters::tui::app::{InputAction, handle_input};
    use rustain::adapters::tui::state::TuiState;
    use rustain::domain::events::{DomainInputEvent, DomainKey};
    use rustain::domain::models::visual::PanelType;

    for key in ['r', 'R'] {
        let mut state = TuiState::new(160, 24);
        handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlX));
        assert!(state.which_key.active);
        assert_eq!(
            handle_input(&mut state, &DomainInputEvent::KeyPress(key)),
            InputAction::OpenPanel(PanelType::Room),
            "Ctrl+X, {key} must open the real panel"
        );
        assert!(!state.feedback_blocks.contains_key("chord-r"));
    }
}

/// **AC1 layer (b), structural.** Without the dispatch arm the allowlist entry
/// routes `ExecuteCommand { name: "room" }` into the adapter-override path.
/// Both render arms are asserted too: the dashboard match has a `_` arm, so a
/// missing `Room` case there compiles clean and renders nothing.
#[test]
fn the_room_dispatch_and_render_arms_are_wired_and_precede_the_catch_all() {
    let source = event_loop_source();
    let room = source
        .find(r#"else if cmd_name == "room""#)
        .expect("the /room dispatch arm exists");
    let override_arm = source
        .find("port_dimension_from_command_name(cmd_name)")
        .expect("the adapter-override catch-all exists");
    assert!(
        room < override_arm,
        "`/room` must be intercepted BEFORE the adapter-override path"
    );
    assert!(
        source.contains(
            "room_bridge::room_command(&mut state, &conversation.id, cmd_arg, &app_state).await;"
        ),
        "the production command future must be awaited"
    );
    assert!(
        source.contains("room_bridge::open_panel(&app_state, &mut state).await;"),
        "the production chord refresh future must be awaited"
    );
    assert!(
        source.contains("room_bridge::refresh_head(") && source.contains(").await;"),
        "the open Room panel must poll and await the durable head"
    );
    let bridge = room_bridge_source();
    assert!(
        bridge.contains("RoomCommandArgs::View => match open_room_view(app_state, state).await")
            && bridge.contains("open_room_view_with_source("),
        "bare /room must await the production View bridge exercised by its behavioral keystone"
    );
    assert_eq!(
        source.matches("room_panel::render(").count(),
        2,
        "both the sidebar AND the dashboard render dispatch must have a Room arm; \
         the dashboard match has a `_` arm, so a missing case renders nothing silently"
    );
}

/// The palette and the help screen both advertise the surface and its faster
/// path, and the registry description names every sub-verb (Trap 5, point 6).
#[test]
fn the_room_surface_is_discoverable_with_its_chord_and_every_subverb() {
    let registry = rustain::adapters::command_registry::CommandRegistry::new();
    let mut palette = rustain::adapters::palette_registry::PaletteRegistry::new();
    palette.populate_from_command_registry(&registry);
    let entry = palette
        .all_entries()
        .iter()
        .find(|entry| entry.name == "/room")
        .cloned()
        .expect("/room is in the palette");
    assert_eq!(entry.shortcut.as_deref(), Some("Ctrl+X, R"));
    for subverb in ["/room role list", "/room role grant", "/room role revoke"] {
        assert!(entry.description.contains(subverb), "{entry:?}");
    }

    let categories = rustain::adapters::tui::help_data::help_categories();
    let bindings: Vec<_> = categories
        .iter()
        .flat_map(|category| category.bindings.iter())
        .collect();
    for key in [
        "/room",
        "/room role list",
        "/room role grant <peer> <role>",
        "/room role revoke <peer>",
        "Ctrl+X, R",
    ] {
        let binding = bindings
            .iter()
            .find(|binding| binding.key == key)
            .unwrap_or_else(|| panic!("{key} is documented in help"));
        assert!(binding.available, "{key}");
    }
}

/// **AC1 + AC2 layer (a), behavioural.** Seeds a temp workspace through the
/// production `NodeJournal` writer, reads it back through the production
/// `WorkspaceJournalReader`, folds it through the production
/// `room_bridge::fold_room_read`, and renders the real widget into a real
/// `Buffer`. Then asserts the pixels.
///
/// **Forbidden bypass, deliberately not used:** constructing an
/// `OrchestrationRoom` in the test and calling the row renderer directly. That
/// is the constructed-row false-green (`DF-CR-14-3a-1`); every fact below
/// travels through a real journal file.
///
/// Second mutant: fold with `project()` instead of `project_for_host()` → the
/// host-bound marker disappears and this goes RED.
/// Third mutant: drop the unknown-variant row → the unrecognised record
/// vanishes from the room and this goes RED.
#[tokio::test]
async fn the_room_panel_renders_a_replayed_host_bound_row_from_a_real_journal() {
    use ratatui::prelude::{Buffer, Rect};
    use rustain::adapters::tui::state::RoomPanelState;
    use rustain::adapters::tui::theme::Theme;
    use rustain::domain::models::{FocusState, NodeState, OrchestrationRoomId};
    use rustain::infrastructure::runtime::room_bridge::fold_room_read;
    use rustain::infrastructure::subagent::NodeJournal;
    use rustain::infrastructure::subagent::node_journal::WorkspaceJournalReader;

    let workspace = tempfile::tempdir().expect("workspace");
    let journal = NodeJournal::open_workspace(workspace.path())
        .await
        .expect("open journal");
    let stranded = AgentId::parse("spoke-stranded").expect("agent id");
    let local = AgentId::parse("spoke-local").expect("agent id");
    for event in [
        RoomEvent::NodeRegistered {
            node: stranded.clone(),
            origin: NodeOrigin::Interactive,
            host: HostBinding::new("host-A", "ws"),
        },
        RoomEvent::NodeStateChanged {
            node: stranded.clone(),
            from: NodeState::Created,
            to: NodeState::Suspended,
        },
        RoomEvent::NodeRegistered {
            node: local.clone(),
            origin: NodeOrigin::Interactive,
            host: HostBinding::new("host-B", "ws"),
        },
        RoomEvent::NodeStateChanged {
            node: local.clone(),
            from: NodeState::Created,
            to: NodeState::Suspended,
        },
    ] {
        journal.append_room(event).await.expect("append");
    }
    // A line a NEWER build wrote, appended raw to the same durable file.
    let path = workspace
        .path()
        .join(".rustain")
        .join("rooms")
        .join(format!(
            "room-{}.jsonl",
            rustain::infrastructure::paths::workspace_hash(workspace.path())
        ));
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open journal file");
    std::io::Write::write_all(
        &mut file,
        b"{\"schema_version\":1,\"seq\":5,\"recorded_at_ms\":1,\
          \"record\":{\"kind\":\"room\",\"payload\":{\"event\":\"room_teleported\"}}}\n",
    )
    .expect("append a future record");
    drop(file);

    let entries = rustain::domain::ports::RoomJournalReader::load_entries(
        &WorkspaceJournalReader::open_workspace(workspace.path()),
    )
    .await
    .expect("an unknown tag must not fail the whole load");
    let (room, max_seq, unknown_records) = fold_room_read(
        entries,
        OrchestrationRoomId::parse("room-render").expect("room id"),
        "host-B",
    );
    assert_eq!(max_seq, 5);
    assert_eq!(unknown_records, 1, "the unknown record must be counted");

    let mut panel = RoomPanelState::default();
    let mut selected = 0;
    panel.apply_read(
        room,
        "host-B".to_owned(),
        max_seq,
        unknown_records,
        1_700_000_000_000,
        &mut selected,
    );
    // A 120-column terminal allocates a 36-column sidebar with 34 inner
    // columns. The keystone must exercise that production minimum, not a
    // fictitious 120-column panel.
    let area = Rect::new(0, 0, 36, 24);
    let mut buf = Buffer::empty(area);
    rustain::adapters::tui::widgets::room_panel::render(
        area,
        &mut buf,
        &mut panel,
        selected,
        &FocusState::Chat,
        &Theme::dark(),
    );
    let painted = buf
        .content()
        .chunks(area.width as usize)
        .map(|row| {
            row.iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n");
    // AC2 — the host-bound row REPLACES `resumable`, names the host, hazards.
    assert!(painted.contains("· host-bound: host-A"), "{painted}");
    assert!(painted.contains('▲'), "{painted}");
    // AC2 positive control — the same-host parked spoke is untouched.
    assert!(painted.contains("· resumable"), "{painted}");
    // AC1 — the honest header, the aggregate, the unknown row, the footer.
    assert!(painted.contains("as of"), "{painted}");
    let lowered = painted.to_ascii_lowercase();
    let positive_claims = lowered.replace("not live", "").replace("never live", "");
    assert!(
        !positive_claims
            .split(|ch: char| !ch.is_ascii_alphanumeric())
            .any(|word| word == "live"),
        "never makes a positive live claim: {painted}"
    );
    assert!(painted.contains("wave ✓0 ✗0 ▶0"), "{painted}");
    assert!(painted.contains("here: host-B"), "{painted}");
    assert!(
        painted.contains("unrecognised record"),
        "an unrecognised record renders explicitly, never silently dropped: {painted}"
    );
    assert!(
        painted.contains("append-only, crash-safe") && painted.contains("structurally replayable"),
        "the wrapped footer must be bound to STRUCTURAL_REPLAY_CLAIM: {painted}"
    );
    // ⛔ Wording ceiling (DF-18-2-AUTHENTICATED-JOURNAL): no POSITIVE integrity
    // claim. The footer's honest negation ("not payload- or provenance-
    // authenticated") is required, so a bare "authentic" substring scan would
    // fire on the very clause that keeps the panel honest.
    let lowered = painted.to_ascii_lowercase();
    for forbidden in [
        "tamper",
        "cryptograph",
        "provably",
        "authentic journal",
        "authenticated record",
        "is authentic",
    ] {
        assert!(
            !lowered.contains(forbidden),
            "forbidden integrity wording '{forbidden}': {painted}"
        );
    }
    assert!(
        lowered.contains("structurally replayable (not"),
        "{painted}"
    );
    assert!(lowered.contains("payload- or"), "{painted}");
    assert!(
        lowered.contains("provenance-authenticated"),
        "the footer must state what the journal does NOT prove: {painted}"
    );
}

/// AC2 — N host-bound nodes render N rows, each naming its host. An aggregate
/// would hide *which* work is stranded, which is the information the row
/// exists to carry.
#[test]
fn multiple_host_bound_nodes_render_one_row_each_with_no_roll_up() {
    use rustain::adapters::tui::widgets::room_panel::node_row;
    use rustain::domain::models::{NodeState, OrchestrationRoomId};

    let mut events = Vec::new();
    for index in 0..5 {
        let node = AgentId::parse(&format!("spoke-{index}")).expect("agent id");
        events.push(RoomEvent::NodeRegistered {
            node: node.clone(),
            origin: NodeOrigin::Interactive,
            host: HostBinding::new("host-A", "ws"),
        });
        events.push(RoomEvent::NodeStateChanged {
            node,
            from: NodeState::Created,
            to: NodeState::Suspended,
        });
    }
    let room = OrchestrationRoom::project_for_host(
        OrchestrationRoomId::parse("room-many").expect("room id"),
        events,
        "host-B",
    );
    let rows: Vec<String> = room.nodes().values().map(node_row).collect();
    assert_eq!(rows.len(), 5);
    for row in &rows {
        assert!(row.contains("· host-bound: host-A"), "{row}");
        assert!(!row.contains("resumable"), "{row}");
    }
}
