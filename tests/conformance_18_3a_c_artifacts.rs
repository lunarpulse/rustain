//! Story 18.3a-c — artifact surfaces and the patch-review verdict.
//!
//! ⛔ **No `a2a` in this filename.**
//! `every_a2a_integration_test_is_wired_into_the_ci_a2a_lane`
//! (`domain/ports/capability_provider.rs`) fails the **default** lane for an
//! unlisted `tests/*a2a*.rs`.
//!
//! Every visual AC carries two layers, per the peer-policy addendum's
//! two-layer rule: a behavioural test through the real handler/render state
//! **and** a structural ratchet pinning the real dispatch arm. A constructed-row
//! renderer test alone is the false-green that recipe exists to prevent
//! (`DF-CR-14-3a-1`).

use rustain::domain::models::{
    AgentId, ArtifactId, ArtifactKind, ArtifactRef, CapabilityTokenId, ContentHash,
    EvidenceArtifact, HostBinding, JournalEntry, JournalRecord, OrchestrationRoom,
    OrchestrationRoomId, OwnershipKind, PermissionMode, ProvenanceTag, ReviewStatus, ReviewVerdict,
    RoomEditDecision, RoomEditKind, RoomEvent, RoomRole,
};
use rustain::domain::services::patch_review::{
    ApplyDecision, MergeBackPolicy, PatchDisposition, may_apply_patch, patch_disposition,
};

// ─────────────────────────────── helpers ───────────────────────────────

fn hash(seed: char) -> ContentHash {
    ContentHash::parse_hex(&std::iter::repeat_n(seed, 64).collect::<String>()).expect("hex")
}

fn aid(seed: char) -> ArtifactId {
    ArtifactId::from(hash(seed))
}

fn agent(name: &str) -> AgentId {
    AgentId::parse(name).expect("agent id")
}

fn artifact(seed: char, kind: ArtifactKind, producer: &str) -> ArtifactRef {
    EvidenceArtifact {
        id: aid(seed),
        kind,
        producer: agent(producer),
        content_hash: hash('0'),
        authority: CapabilityTokenId::default(),
        provenance: vec![ProvenanceTag::UserOriginated],
        depends_on: vec![],
        review: Some(ReviewStatus::Pending),
        host: HostBinding::new("host-A", "ws"),
    }
}

fn fixture_path(name: &str) -> String {
    format!(
        "{}/tests/fixtures/18_3a_c/{name}",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn source(relative: &str) -> String {
    let path = format!("{}/{relative}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {path}: {error}"))
}

fn event_loop_source() -> String {
    source("src/infrastructure/runtime/event_loop.rs")
}

fn artifact_bridge_source() -> String {
    source("src/infrastructure/runtime/artifact_bridge.rs")
}

/// Paint a panel state into a real `Buffer` and return the text.
fn paint(state: &mut rustain::adapters::tui::state::ArtifactsPanelState, width: u16) -> String {
    use ratatui::prelude::{Buffer, Rect};
    use rustain::adapters::tui::theme::Theme;
    use rustain::domain::models::FocusState;

    let area = Rect::new(0, 0, width, 30);
    let mut buf = Buffer::empty(area);
    let selected = 0;
    rustain::adapters::tui::widgets::artifacts_panel::render(
        area,
        &mut buf,
        state,
        selected,
        &FocusState::Chat,
        &Theme::dark(),
    );
    buf.content()
        .chunks(area.width as usize)
        .map(|row| {
            row.iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Fold a `RoomEvent` stream and hand the panel a real read.
fn panel_from(
    events: Vec<RoomEvent>,
    policy: MergeBackPolicy,
    mode: PermissionMode,
) -> rustain::adapters::tui::state::ArtifactsPanelState {
    let room = OrchestrationRoom::project_for_host(
        OrchestrationRoomId::parse("room-test").expect("room id"),
        events,
        "host-A",
    );
    let mut panel = rustain::adapters::tui::state::ArtifactsPanelState::default();
    let mut selected = 0;
    panel.apply_read(
        room,
        "host-A".to_owned(),
        1,
        0,
        1_700_000_000_000,
        policy,
        mode,
        &mut selected,
    );
    panel
}

// ═══════════════════════════════ AC1 ═══════════════════════════════

/// **AC1 keystone.** A journal line written by a newer build carries an
/// `ArtifactKind`, a `ReviewStatus` and a `ReviewVerdict` this build has never
/// heard of. All four lines must parse, the fold must produce four entries, and
/// the unknown values must render as explicit unknown rows.
///
/// **Front door:** the vendored fixture is written to a real workspace journal
/// file and read back through the production `WorkspaceJournalReader` →
/// `fold_artifacts_read`. ⛔ **Forbidden bypass, deliberately not used:**
/// constructing `ArtifactKind::Unknown` in Rust and asserting it renders. That
/// proves the enum compiles, not that a newer build's *bytes* survive the
/// reader.
///
/// Mutants this must turn RED:
///   1. Remove `#[serde(other)]` from `ArtifactKind` → line 2 fails and takes
///      lines 1, 3 and 4 with it.
///   2. Remove `#[serde(other)]` from `ReviewStatus` → same.
///   3. Remove `#[serde(other)]` from `ReviewVerdict` → **line 3** fails and
///      takes the file with it. (Against a three-line fixture this mutant is
///      un-fireable; that is exactly why line 3 exists — `ReviewStatus` is
///      internally tagged, so an unknown status resolves the whole enum and
///      serde never reads a `verdict` field at all.)
#[tokio::test]
async fn a_newer_builds_values_survive_the_reader_and_render_as_explicit_unknown_rows() {
    use rustain::domain::ports::RoomJournalReader;
    use rustain::infrastructure::runtime::artifact_bridge::fold_artifacts_read;
    use rustain::infrastructure::subagent::node_journal::WorkspaceJournalReader;

    let workspace = tempfile::tempdir().expect("workspace");
    let rooms = workspace.path().join(".rustain").join("rooms");
    std::fs::create_dir_all(&rooms).expect("rooms dir");
    let path = rooms.join(format!(
        "room-{}.jsonl",
        rustain::infrastructure::paths::workspace_hash(workspace.path())
    ));
    let fixture = std::fs::read(fixture_path("forward_compat_room.jsonl")).expect("fixture");
    std::fs::write(&path, &fixture).expect("seed journal");

    let entries =
        RoomJournalReader::load_entries(&WorkspaceJournalReader::open_workspace(workspace.path()))
            .await
            .expect("an unknown nested VALUE must not fail the whole load");
    assert_eq!(entries.len(), 4, "all four lines parse and fold");

    // Lines 1 and 4 carry only known values and must re-serialize byte-identically.
    let raw: Vec<&str> = std::str::from_utf8(&fixture)
        .expect("utf8")
        .lines()
        .collect();
    for index in [0usize, 3usize] {
        let round_tripped = serde_json::to_string(&entries[index]).expect("re-serialize");
        assert_eq!(
            round_tripped,
            raw[index],
            "line {} must round-trip byte-identically — the fallback arms are inert on the \
             existing corpus",
            index + 1
        );
    }

    let (room, max_seq, unknown_records) = fold_artifacts_read(
        entries,
        OrchestrationRoomId::parse("room-unknown").expect("room id"),
        "host-A",
    );
    assert_eq!(max_seq, 4);
    assert_eq!(
        unknown_records, 0,
        "every RECORD TAG here is known; only nested field VALUES are new"
    );

    let unknown_kind = room.artifacts().get(&aid('b')).expect("artifact 2 folded");
    assert_eq!(unknown_kind.kind, ArtifactKind::Unknown);
    assert_eq!(unknown_kind.review, Some(ReviewStatus::Unknown));

    let unknown_verdict = room.artifacts().get(&aid('a')).expect("artifact 1 folded");
    assert!(
        matches!(
            unknown_verdict.review,
            Some(ReviewStatus::Reviewed {
                verdict: ReviewVerdict::Unknown,
                ..
            })
        ),
        "the patch folds to an unknown verdict, not to `approved`: {:?}",
        unknown_verdict.review
    );

    let mut panel = rustain::adapters::tui::state::ArtifactsPanelState::default();
    let mut selected = 0;
    panel.apply_read(
        room,
        "host-A".to_owned(),
        max_seq,
        unknown_records,
        1_700_000_000_000,
        MergeBackPolicy::default(),
        PermissionMode::Yolo,
        &mut selected,
    );
    let painted = paint(&mut panel, 100);
    assert!(
        painted.contains("unknown bbbbbb"),
        "the unknown KIND renders as an explicit unknown row: {painted}"
    );
    assert!(
        painted.contains("review: unknown"),
        "the unknown STATUS renders explicitly: {painted}"
    );
    assert!(
        painted.contains("verdict: unknown"),
        "the unknown VERDICT renders explicitly, never as `approved`: {painted}"
    );
    assert!(
        !painted.contains("approved"),
        "an unreadable verdict must never render as approved: {painted}"
    );
}

/// **AC1 fourth mutant.** Flip the `ReviewStatus::Unknown` arm in
/// `patch_disposition` to an applying disposition and this fires. This is the
/// one that would actually hurt someone: an unreadable review state that reads
/// as approved.
#[test]
fn an_unreadable_review_state_is_never_an_applying_disposition() {
    let policy = MergeBackPolicy {
        auto_approve_user_originated: true,
    };
    let mut unreadable_status = artifact('a', ArtifactKind::Patch, "spoke-1");
    unreadable_status.review = Some(ReviewStatus::Unknown);
    assert_eq!(
        patch_disposition(
            &unreadable_status,
            OwnershipKind::Owned,
            PermissionMode::Yolo,
            &policy
        ),
        PatchDisposition::AwaitingReview,
        "an unreadable review STATUS fails closed"
    );

    let mut unreadable_verdict = artifact('a', ArtifactKind::Patch, "spoke-1");
    unreadable_verdict.review = Some(ReviewStatus::Reviewed {
        reviewer: agent("jun"),
        verdict: ReviewVerdict::Unknown,
    });
    assert_eq!(
        patch_disposition(
            &unreadable_verdict,
            OwnershipKind::Owned,
            PermissionMode::Yolo,
            &policy
        ),
        PatchDisposition::AwaitingReview,
        "an unreadable VERDICT fails closed"
    );
    assert_eq!(
        may_apply_patch(
            &unreadable_verdict,
            OwnershipKind::Owned,
            PermissionMode::Yolo,
            &policy
        ),
        ApplyDecision::Refuse
    );
}

/// **AC1 positive control.** The additive arms are inert on the existing
/// corpus: a journal of only known values folds exactly as it did before.
#[test]
fn the_schema_version_is_unchanged_and_known_values_are_untouched() {
    assert_eq!(
        rustain::domain::models::NODE_JOURNAL_SCHEMA_VERSION,
        1,
        "a bump makes every existing room file unreadable — parse_entries rejects mismatches"
    );
    for (json, expected) in [
        ("\"evidence\"", ArtifactKind::Evidence),
        ("\"patch\"", ArtifactKind::Patch),
        ("\"test_result\"", ArtifactKind::TestResult),
        ("\"decision\"", ArtifactKind::Decision),
        ("\"review\"", ArtifactKind::Review),
        ("\"input_request\"", ArtifactKind::InputRequest),
    ] {
        let parsed: ArtifactKind = serde_json::from_str(json).expect(json);
        assert_eq!(parsed, expected);
        assert_eq!(serde_json::to_string(&expected).expect("ser"), json);
    }
    for (json, expected) in [
        ("\"approved\"", ReviewVerdict::Approved),
        ("\"changes_requested\"", ReviewVerdict::ChangesRequested),
        ("\"rejected\"", ReviewVerdict::Rejected),
    ] {
        let parsed: ReviewVerdict = serde_json::from_str(json).expect(json);
        assert_eq!(parsed, expected);
        assert_eq!(serde_json::to_string(&expected).expect("ser"), json);
    }
}

// ═══════════════════════════════ AC2 ═══════════════════════════════

/// **AC2 keystone (Rule 4 — structural, exhaustive, deterministic).** The full
/// cross-product, asserting `may_apply_patch(x) == patch_disposition(x).decision()`
/// at **every** point. It cannot be satisfied by a sampled race and it fails the
/// instant the two functions disagree.
///
/// Mutant: give `may_apply_patch` back one branch of its own — e.g. re-add
/// `if permission_mode == Plan { return Refuse; }` above the projection — and
/// flip the corresponding arm in `patch_disposition` to an applying
/// disposition. A mutant that edits only one side is too weak: this must prove
/// the two are actually one.
#[test]
fn may_apply_patch_is_a_total_projection_of_patch_disposition() {
    let kinds = [
        ArtifactKind::Evidence,
        ArtifactKind::Patch,
        ArtifactKind::TestResult,
        ArtifactKind::Decision,
        ArtifactKind::Review,
        ArtifactKind::InputRequest,
        ArtifactKind::Unknown,
    ];
    let ownerships = [OwnershipKind::Owned, OwnershipKind::Peer];
    let modes = [
        PermissionMode::Plan,
        PermissionMode::Normal,
        PermissionMode::AutoEdit,
        PermissionMode::Yolo,
    ];
    let provenances = [
        vec![],
        vec![ProvenanceTag::SelfOriginated],
        vec![ProvenanceTag::UserOriginated],
    ];
    let policies = [
        MergeBackPolicy::default(),
        MergeBackPolicy {
            auto_approve_user_originated: true,
        },
    ];
    let producer = agent("spoke-1");
    let reviewers = [producer.clone(), agent("jun")];
    let mut points = 0usize;
    for kind in kinds {
        for ownership in ownerships {
            for mode in modes {
                for provenance in &provenances {
                    for policy in policies {
                        for reviewer in &reviewers {
                            let mut reviews = vec![
                                None,
                                Some(ReviewStatus::Pending),
                                Some(ReviewStatus::Unknown),
                            ];
                            for verdict in [
                                ReviewVerdict::Approved,
                                ReviewVerdict::ChangesRequested,
                                ReviewVerdict::Rejected,
                                ReviewVerdict::Unknown,
                            ] {
                                reviews.push(Some(ReviewStatus::Reviewed {
                                    reviewer: reviewer.clone(),
                                    verdict,
                                }));
                            }
                            for review in reviews {
                                let mut candidate = artifact('a', kind, "spoke-1");
                                candidate.provenance = provenance.clone();
                                candidate.review = review;
                                let disposition =
                                    patch_disposition(&candidate, ownership, mode, &policy);
                                assert_eq!(
                                    may_apply_patch(&candidate, ownership, mode, &policy),
                                    disposition.decision(),
                                    "the gate and the reason diverged at {candidate:?} \
                                     {ownership:?} {mode:?} {policy:?}"
                                );
                                points += 1;
                            }
                        }
                    }
                }
            }
        }
    }
    assert_eq!(
        points,
        7 * 2 * 4 * 3 * 2 * 2 * 7,
        "the cross-product is complete"
    );
}

/// **AC2 second keystone — the Rule-2 discharge for the unreachable arm.**
/// A **direct-domain** unit test, deliberately not a render test: gate-review
/// NON-BLOCKER 2 requires it precisely because `kind != Patch` cannot be reached
/// through the patch-row front door.
///
/// Mutant: collapse `RefusedNotAPatch` back into `RefusedPeerOwned` → this
/// fires, and the patch row would then render a **false reason** for a non-patch.
#[test]
fn a_non_patch_is_refused_as_not_a_patch_never_as_peer_owned() {
    let policy = MergeBackPolicy::default();
    let evidence = artifact('a', ArtifactKind::Evidence, "spoke-1");
    assert_eq!(
        patch_disposition(
            &evidence,
            OwnershipKind::Owned,
            PermissionMode::Yolo,
            &policy
        ),
        PatchDisposition::RefusedNotAPatch,
        "ruling A4 — `kind != Patch` and `ownership == Peer` are two causes, not one"
    );
    let patch = artifact('a', ArtifactKind::Patch, "spoke-1");
    assert_eq!(
        patch_disposition(&patch, OwnershipKind::Peer, PermissionMode::Yolo, &policy),
        PatchDisposition::RefusedPeerOwned
    );
    // Behaviour is unchanged: both still project to `Refuse`.
    assert_eq!(
        PatchDisposition::RefusedNotAPatch.decision(),
        ApplyDecision::Refuse
    );
    assert_eq!(
        PatchDisposition::RefusedPeerOwned.decision(),
        ApplyDecision::Refuse
    );
}

/// **AC2 third keystone — the corner the six pre-existing positive controls
/// miss.** The applying guard is a **disjunction**: `!self_originated ||
/// reviewer != producer`. A *user-originated* patch approved by its own
/// producer therefore APPLIES, because `!self_originated` already satisfies the
/// disjunct.
///
/// No existing test can falsify a `reviewer != producer`-only implementation:
/// `conformance_cow_mergeback.rs` sets `provenance = [SelfOriginated]` before
/// its `reviewer == producer` assert, and uses a different reviewer afterwards.
/// An implementation that dropped the disjunct would silently refuse these — on
/// the live `/fanout` path.
#[test]
fn a_user_originated_patch_approved_by_its_own_producer_still_applies() {
    let mut candidate = artifact('a', ArtifactKind::Patch, "spoke-1");
    candidate.provenance = vec![ProvenanceTag::UserOriginated];
    candidate.review = Some(ReviewStatus::Reviewed {
        reviewer: candidate.producer.clone(),
        verdict: ReviewVerdict::Approved,
    });
    assert_eq!(
        patch_disposition(
            &candidate,
            OwnershipKind::Owned,
            PermissionMode::Yolo,
            &MergeBackPolicy::default()
        ),
        PatchDisposition::Applies,
        "dropping the `!self_originated` disjunct would refuse this on the live /fanout path"
    );

    // The paired negative: SELF-originated + self-reviewed is the four-eyes refusal.
    let mut self_reviewed = candidate.clone();
    self_reviewed.provenance = vec![ProvenanceTag::SelfOriginated];
    assert_eq!(
        patch_disposition(
            &self_reviewed,
            OwnershipKind::Owned,
            PermissionMode::Yolo,
            &MergeBackPolicy::default()
        ),
        PatchDisposition::RefusedSelfReview
    );
}

/// **AC2 third mutant.** Return `AwaitingReview` instead of `AutoApplies` for
/// the pending + policy + not-self-originated outcome → this fires on every
/// policy-on point, and AC4's "inverse hazard" row goes RED with it.
#[test]
fn the_shipped_policy_makes_a_pending_user_originated_patch_auto_apply() {
    let candidate = artifact('a', ArtifactKind::Patch, "spoke-1");
    assert_eq!(
        patch_disposition(
            &candidate,
            OwnershipKind::Owned,
            PermissionMode::Yolo,
            &MergeBackPolicy {
                auto_approve_user_originated: true
            }
        ),
        PatchDisposition::AutoApplies,
        "production ships auto_approve_user_originated: true — this is the COMMON case"
    );
    assert_eq!(
        patch_disposition(
            &candidate,
            OwnershipKind::Owned,
            PermissionMode::Yolo,
            &MergeBackPolicy::default()
        ),
        PatchDisposition::AwaitingReview
    );
}

/// **AC2 positive control (structural).** `may_apply_patch` must contain no
/// condition of its own after ruling A2 — its entire body is the projection.
/// Two derivations of one gate is `ADR-17-CC-01`'s rejected alternative rebuilt
/// inside a module.
#[test]
fn may_apply_patch_has_no_branch_of_its_own() {
    let module = source("src/domain/services/patch_review.rs");
    let start = module
        .find("pub fn may_apply_patch(")
        .expect("may_apply_patch exists");
    let body = &module[start..];
    let end = body.find("\n}\n").expect("function closes");
    let body = &body[..end];
    for forbidden in ["if ", "match ", "return "] {
        assert!(
            !body.contains(forbidden),
            "may_apply_patch regrew a `{forbidden}` of its own — it must be a total \
             projection of patch_disposition: {body}"
        );
    }
    assert!(
        body.contains("patch_disposition(artifact, ownership, permission_mode, policy).decision()"),
        "{body}"
    );
    // Ruling P4: the projection is a METHOD on the disposition, not a free
    // function beside it, so an `ApplyDecision` cannot be computed without
    // going through a disposition.
    assert!(
        module.contains("impl PatchDisposition {") && module.contains("pub fn decision(self)"),
        "the projection must be subordinated as PatchDisposition::decision"
    );
}

// ═══════════════════════════════ AC3 ═══════════════════════════════

/// **AC3 layer (a), routing.** Both spellings must reach the dispatch arm.
///
/// Mutant: omit the `app.rs` input-routing allowlist entry → the command falls
/// through to `SubmitWithContext`, resolves no command file, and **silently
/// never runs**. That is the 14.3c `/fanout` failure the in-source comment
/// documents. A registry-presence assertion stays green while the command is
/// dead; this does not.
#[test]
fn the_artifact_commands_route_as_execute_commands_not_missing_custom_commands() {
    use rustain::adapters::tui::app::{InputAction, submit_message_for_test};
    use rustain::adapters::tui::state::TuiState;

    for (input, expected_name) in [
        ("/artifacts", "artifacts"),
        ("/artifact show a3f2c1", "artifact"),
        ("/artifact apply a3f2c1", "artifact"),
        ("/artifact review a3f2c1 approve", "artifact"),
        ("/artifact review a3f2c1 request-changes", "artifact"),
        ("/artifact review a3f2c1 reject", "artifact"),
    ] {
        let mut state = TuiState::new(120, 24);
        state.input_buffer = input.to_owned();
        match submit_message_for_test(&mut state) {
            InputAction::ExecuteCommand { name, args } => {
                assert_eq!(name, expected_name, "{input}");
                let tail = input
                    .strip_prefix(&format!("/{expected_name}"))
                    .unwrap()
                    .trim();
                if tail.is_empty() {
                    assert!(args.is_none(), "{input} → {args:?}");
                } else {
                    assert_eq!(args.as_deref(), Some(tail), "{input}");
                }
            }
            other => panic!("`{input}` must reach the /artifact dispatch arm, got {other:?}"),
        }
    }
}

/// **AC3 fourth mutant's guard.** The chord is real, not a `Noop`, and it is
/// lower-case so `lookup_chord` (which lowercases) can find it.
///
/// A command-only keystone is GREEN against a deleted `InputAction::OpenPanel`
/// arm, so the chord must be driven too.
#[test]
fn ctrl_x_e_dispatches_the_artifacts_panel() {
    use rustain::adapters::tui::app::{InputAction, handle_input};
    use rustain::adapters::tui::state::TuiState;
    use rustain::domain::events::{DomainInputEvent, DomainKey};
    use rustain::domain::models::visual::PanelType;

    for key in ['e', 'E'] {
        let mut state = TuiState::new(160, 24);
        handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlX));
        assert!(state.which_key.active);
        assert_eq!(
            handle_input(&mut state, &DomainInputEvent::KeyPress(key)),
            InputAction::OpenPanel(PanelType::Artifacts),
            "Ctrl+X, {key} must open the real panel"
        );
    }
    // `a` stays Adapters — the chord table has no collision.
    let mut state = TuiState::new(160, 24);
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlX));
    assert_eq!(
        handle_input(&mut state, &DomainInputEvent::KeyPress('a')),
        InputAction::OpenPanel(PanelType::Adapters)
    );
}

/// **AC3 layer (b), structural (Rule 4).** Without the dispatch arms the
/// allowlist entries route `ExecuteCommand { name: "artifacts" }` into the
/// adapter-override catch-all. Both render arms are asserted too.
///
/// 🔴 **The headline mutant: delete the DASHBOARD render arm.** It sits above a
/// `_` arm, so the match is **not** exhaustive — it compiles clean, renders
/// nothing in Dashboard density, and then falls through the `_` arm's `Agents`
/// re-check so the *wrong* panel may render. 18.3a's mutant 13 was exactly this.
///
/// 🔴 **Fourth mutant: delete the `InputAction::OpenPanel` arm.** `event_loop`'s
/// panel-open path is an `if / else if` chain, not a match, so `Ctrl+X, E` would
/// open an **empty** pane while `/artifacts` keeps working — compiles clean.
#[test]
fn the_artifact_dispatch_and_render_arms_are_wired_and_precede_the_catch_all() {
    let source = event_loop_source();
    let artifacts = source
        .find("cmd_name == \"artifacts\"")
        .expect("the /artifacts dispatch arm exists");
    let artifact = source
        .find("cmd_name == \"artifact\"")
        .expect("the /artifact dispatch arm exists");
    let override_arm = source
        .find("port_dimension_from_command_name(cmd_name)")
        .expect("the adapter-override catch-all exists");
    assert!(
        artifacts < override_arm && artifact < override_arm,
        "`/artifacts` and `/artifact` must be intercepted BEFORE the adapter-override path"
    );
    assert!(
        source.contains(
            "artifact_bridge::artifacts_command(&mut state, &conversation.id, cmd_arg, \
             &app_state, security.current_mode()).await;"
        ),
        "the production list future must be awaited"
    );
    assert!(
        source.contains(
            "artifact_bridge::artifact_command(&mut state, &conversation.id, cmd_arg, \
             &app_state, security.current_mode()).await;"
        ),
        "the production verb future must be awaited"
    );
    assert!(
        source.contains(
            "artifact_bridge::open_panel(&app_state, &mut state, security.current_mode()).await;"
        ),
        "Ctrl+X, E needs its OWN InputAction::OpenPanel arm — its absence compiles clean \
         and opens a blank pane"
    );
    assert_eq!(
        source.matches("artifacts_panel::render(").count(),
        2,
        "both the sidebar AND the dashboard render dispatch must have an Artifacts arm; \
         the dashboard match has a `_` arm, so a missing case renders nothing silently"
    );
    // The fold must be host-honest.
    let bridge = artifact_bridge_source();
    assert!(
        bridge.contains("OrchestrationRoom::project_for_host(room_id, events, host_id)"),
        "fold via project_for_host, never bare project()"
    );
    assert!(
        !bridge.contains("OrchestrationRoom::project("),
        "bare project() loses the host-bound derivation"
    );
}

/// **AC3 discoverability.** The palette advertises the faster path and the
/// registry description names every sub-verb.
#[test]
fn the_artifact_surface_is_discoverable_with_its_chord_and_every_subverb() {
    let registry = rustain::adapters::command_registry::CommandRegistry::new();
    let mut palette = rustain::adapters::palette_registry::PaletteRegistry::new();
    palette.populate_from_command_registry(&registry);
    let entries = palette.all_entries();
    let list = entries
        .iter()
        .find(|entry| entry.name == "/artifacts")
        .expect("/artifacts is in the palette");
    assert_eq!(list.shortcut.as_deref(), Some("Ctrl+X, E"));
    let verb = entries
        .iter()
        .find(|entry| entry.name == "/artifact")
        .expect("/artifact is in the palette");
    for subverb in ["/artifact show", "/artifact review", "/artifact apply"] {
        assert!(verb.description.contains(subverb), "{verb:?}");
    }

    let categories = rustain::adapters::tui::help_data::help_categories();
    let bindings: Vec<_> = categories
        .iter()
        .flat_map(|category| category.bindings.iter())
        .collect();
    for key in [
        "/artifacts",
        "/artifact show <id>",
        "/artifact review <id> <verdict>",
        "/artifact apply <id>",
        "Ctrl+X, E",
    ] {
        let binding = bindings
            .iter()
            .find(|binding| binding.key == key)
            .unwrap_or_else(|| panic!("{key} is documented in help"));
        assert!(binding.available, "{key}");
    }
}

/// **AC3 behavioural.** A real fold with real `depends_on` edges renders every
/// artifact, its lineage, and the honest "as of" header — in both the sidebar
/// width and the wide Dashboard width.
///
/// Third mutant (fold with bare `project()`): covered structurally above; the
/// host line here is what a reader checks.
#[test]
fn the_panel_renders_every_kind_with_real_lineage_at_both_densities() {
    let mut evidence = artifact('a', ArtifactKind::Evidence, "spoke-1");
    evidence.review = None;
    let mut patch = artifact('b', ArtifactKind::Patch, "spoke-2");
    patch.depends_on = vec![evidence.id.clone()];
    let mut ticket = artifact('c', ArtifactKind::InputRequest, "spoke-3");
    ticket.review = None;

    let panel_events = vec![
        RoomEvent::ArtifactCreated {
            artifact: evidence.clone(),
        },
        RoomEvent::ArtifactCreated {
            artifact: patch.clone(),
        },
        RoomEvent::ArtifactCreated {
            artifact: ticket.clone(),
        },
    ];
    for width in [36u16, 120u16] {
        let mut panel = panel_from(
            panel_events.clone(),
            MergeBackPolicy::default(),
            PermissionMode::Yolo,
        );
        let painted = paint(&mut panel, width);
        assert!(painted.contains("as of"), "{width}: {painted}");
        assert!(
            !painted.to_ascii_lowercase().contains(" live"),
            "{width}: never claims live: {painted}"
        );
        assert!(painted.contains("aaaaaa"), "{width}: {painted}");
        if width == 36 {
            assert!(painted.contains("no reviewer"), "{width}: {painted}");
        } else {
            assert!(painted.contains("bbbbbb"), "{width}: {painted}");
        }
        assert!(painted.contains("cccccc"), "{width}: {painted}");
        assert!(
            painted.contains("└ depends on aaaaaa"),
            "{width}: the one-level lineage line must render: {painted}"
        );
    }

    // Wide layout keeps the canonical grammar, including the producer.
    let mut wide = panel_from(
        panel_events.clone(),
        MergeBackPolicy::default(),
        PermissionMode::Yolo,
    );
    let painted = paint(&mut wide, 120);
    assert!(
        painted.contains("evidence aaaaaa · unreviewed · from spoke-1"),
        "{painted}"
    );
    assert!(painted.contains("input-request cccccc"), "{painted}");
    assert!(
        painted.contains("input requests are listed as inventory"),
        "ruling A5 — the row says answering is not available here: {painted}"
    );
    // Ruling P6 — inventory, not a worklist.
    assert!(
        !painted.contains(" open") || !painted.contains("input-request"),
        "no queue count may appear beside an input-request row: {painted}"
    );
}

/// **AC3 keystone (a), behavioural, through the named front door.** Real
/// input text → real `InputAction::ExecuteCommand` → the exact dispatch-arm
/// callee (`artifact_bridge::artifacts_command`, the one function the
/// `event_loop.rs` arm invokes) with a real `AppState` → a painted buffer, at
/// **both** production densities (the sidebar arm and the Dashboard arm call
/// the same `artifacts_panel::render`; both call sites are pinned by the
/// structural ratchet above).
///
/// ⛔ What this deliberately still does NOT drive: `event_loop::run` itself.
/// Its input branch reads a real TTY via `crossterm::EventStream`, and the
/// end-to-end bus→TUI harness that could feed it does not exist
/// (`DF-CR-14-4a-6`). The two `else if cmd_name == …` lines inside `run` are
/// the only code on the path this test cannot execute, and they are covered by
/// the structural ratchet — everything downstream of them runs here for real.
///
/// Mutants this turns RED: a broken bridge wiring (journal reader or policy
/// source detached from `AppState`), a dispatch callee that never folds, and a
/// panel state the command never populates — all green under the
/// constructed-panel tests alone.
#[tokio::test]
async fn the_artifacts_command_reaches_a_painted_buffer_through_the_real_dispatch_path() {
    use rustain::adapters::tui::app::{InputAction, submit_message_for_test};
    use rustain::adapters::tui::state::TuiState;
    use rustain::domain::models::visual::PanelType;
    use rustain::infrastructure::subagent::NodeJournal;

    let workspace = tempfile::tempdir().expect("workspace");
    let journal = NodeJournal::open_workspace(workspace.path())
        .await
        .expect("journal");
    let host = rustain::infrastructure::subagent::current_host_id(workspace.path());
    let mut evidence = artifact('a', ArtifactKind::Evidence, "spoke-1");
    evidence.review = None;
    evidence.host = HostBinding::new(&host, "ws");
    let mut patch = artifact('b', ArtifactKind::Patch, "spoke-2");
    patch.host = HostBinding::new(&host, "ws");
    patch.depends_on = vec![evidence.id.clone()];
    let mut ticket = artifact('c', ArtifactKind::InputRequest, "spoke-3");
    ticket.review = None;
    ticket.host = HostBinding::new(&host, "ws");
    for artifact in [&evidence, &patch, &ticket] {
        journal
            .append_room(RoomEvent::ArtifactCreated {
                artifact: artifact.clone(),
            })
            .await
            .expect("append");
    }

    let app_state = artifact_app_state(workspace.path(), None, None);
    let mut state = TuiState::new(120, 40);

    // The named front door: real input routing, not a constructed action.
    state.input_buffer = "/artifacts".to_owned();
    let action = submit_message_for_test(&mut state);
    let InputAction::ExecuteCommand { name, args } = action else {
        panic!("/artifacts must route to ExecuteCommand, got {action:?}");
    };
    assert_eq!(name, "artifacts");
    assert!(args.is_none(), "{args:?}");

    // The exact callee the dispatch arm invokes.
    rustain::infrastructure::runtime::artifact_bridge::artifacts_command(
        &mut state,
        "conv",
        args.as_deref(),
        &app_state,
        PermissionMode::Yolo,
    )
    .await;

    assert!(state.sidebar_visible, "the panel opened");
    assert_eq!(state.sidebar_panel, Some(PanelType::Artifacts));
    assert_eq!(
        state.sidebar_entry_count, 3,
        "three folded artifacts listed"
    );

    // Both densities: 36 columns is the production sidebar inner width at a
    // 120-column terminal; 120 is Dashboard.
    for width in [36u16, 120u16] {
        let painted = paint(&mut state.artifacts_panel, width);
        assert!(painted.contains("as of"), "{width}: {painted}");
        assert!(painted.contains("aaaaaa"), "{width}: {painted}");
        if width == 36 {
            assert!(painted.contains("no reviewer"), "{width}: {painted}");
        } else {
            assert!(painted.contains("bbbbbb"), "{width}: {painted}");
        }
        assert!(painted.contains("cccccc"), "{width}: {painted}");
        assert!(
            painted.contains("└ depends on aaaaaa"),
            "{width}: the one-level lineage line must render: {painted}"
        );
    }
}

/// **AC3 truncation order is a SAFETY property.** At 60 columns the decision
/// suffix must survive; the identifier is a hash and must yield to it first.
#[test]
fn the_decision_suffix_outranks_the_identifier_at_narrow_widths() {
    let mut patch = artifact('b', ArtifactKind::Patch, "a-very-long-producer-node-name");
    patch.review = Some(ReviewStatus::Reviewed {
        reviewer: agent("a-very-long-producer-node-name"),
        verdict: ReviewVerdict::Approved,
    });
    patch.provenance = vec![ProvenanceTag::SelfOriginated];
    let events = vec![RoomEvent::ArtifactCreated {
        artifact: patch.clone(),
    }];
    // 34 is the production sidebar inner width at a 120-column terminal — the
    // narrowest layout that actually ships, not a convenient one.
    for width in [36u16, 60u16, 120u16] {
        let mut panel = panel_from(
            events.clone(),
            MergeBackPolicy::default(),
            PermissionMode::Yolo,
        );
        let painted = paint(&mut panel, width);
        assert!(
            painted.contains("refused: self-review"),
            "at {width} cols the reason must survive truncation: {painted}"
        );
    }
}

/// **AC3 positive control.** A workspace with exactly one `InputRequest` and no
/// patches renders one row plus the *no patches* chrome — proving the panel is
/// not a patch-only surface wearing a general name.
#[test]
fn a_room_with_only_an_input_request_renders_it_plus_the_no_patches_state() {
    let mut ticket = artifact('c', ArtifactKind::InputRequest, "spoke-3");
    ticket.review = None;
    let mut panel = panel_from(
        vec![RoomEvent::ArtifactCreated {
            artifact: ticket.clone(),
        }],
        MergeBackPolicy::default(),
        PermissionMode::Yolo,
    );
    let painted = paint(&mut panel, 100);
    assert!(painted.contains("input-request cccccc"), "{painted}");
    assert!(painted.contains("no patches in this room"), "{painted}");
}

/// **AC3 zero-states — four distinguishable, never a blank pane.**
#[test]
fn every_zero_state_says_which_one_it_is() {
    use rustain::adapters::tui::state::{ArtifactsPanelState, ArtifactsZeroState};
    use rustain::adapters::tui::widgets::artifacts_panel::zero_state_lines;

    // Read failure — distinct from an empty room.
    let mut failed = ArtifactsPanelState::default();
    failed.error = Some("permission denied".to_owned());
    let painted = paint(&mut failed, 100);
    assert!(
        painted.contains("could not read the room journal"),
        "{painted}"
    );
    assert!(
        painted.contains("This is a read failure, not an empty room."),
        "{painted}"
    );

    // Not attached.
    let mut detached = ArtifactsPanelState::default();
    detached.not_attached = true;
    let painted = paint(&mut detached, 100);
    assert!(painted.contains("not attached"), "{painted}");

    // No artifacts.
    let mut empty = panel_from(vec![], MergeBackPolicy::default(), PermissionMode::Yolo);
    let painted = paint(&mut empty, 100);
    assert!(painted.contains("holds no artifacts yet"), "{painted}");

    // All reviewed.
    let mut patch = artifact('b', ArtifactKind::Patch, "spoke-2");
    patch.review = Some(ReviewStatus::Reviewed {
        reviewer: agent("jun"),
        verdict: ReviewVerdict::Approved,
    });
    let mut reviewed = panel_from(
        vec![RoomEvent::ArtifactCreated { artifact: patch }],
        MergeBackPolicy::default(),
        PermissionMode::Yolo,
    );
    let painted = paint(&mut reviewed, 100);
    assert!(painted.contains("carries a recorded verdict"), "{painted}");

    // Four distinct headlines, no duplicates.
    let headlines: std::collections::BTreeSet<&str> = [
        ArtifactsZeroState::NotAttached,
        ArtifactsZeroState::NoArtifacts,
        ArtifactsZeroState::NoPatches,
        ArtifactsZeroState::AllReviewed,
    ]
    .into_iter()
    .map(|zero| zero_state_lines(zero)[0])
    .collect();
    assert_eq!(headlines.len(), 4, "a blank pane cannot tell them apart");
}

// ═══════════════════════════════ AC4 ═══════════════════════════════

/// **AC4 keystone (a).** One journal fixture per disposition, folded and
/// rendered, asserting the exact suffix string for each.
///
/// The five mutants UX-DR-ROOM-08 names, each of which this turns RED:
///   1. render the *verdict* instead of the *decision* → the self-review row;
///   2. drop the Plan-mode arm → a Plan-mode row claims it applies;
///   3. drop the empty-provenance arm → an unstamped patch claims it applies;
///   4. invert the auto-approve arm → the pending-auto row claims blocked;
///   5. collapse `ChangesRequested`/`Rejected` into "not approved" with no
///      suffix → the two rows become indistinguishable.
///
/// **Positive control (mandatory).** An `Approved`-by-a-**different**-reviewer
/// patch renders its eligible suffix with no refusal clause. Without it a suite
/// that renders every row as "refused" passes all five mutants vacuously.
#[test]
fn every_disposition_renders_its_exact_suffix_through_a_real_fold() {
    let auto = MergeBackPolicy {
        auto_approve_user_originated: true,
    };
    let none = MergeBackPolicy::default();

    let mut cases: Vec<(&str, ArtifactRef, MergeBackPolicy, PermissionMode)> = Vec::new();

    // Applies — POSITIVE CONTROL: the confirmed operator door exists.
    let mut applies = artifact('b', ArtifactKind::Patch, "spoke-2");
    applies.provenance = vec![ProvenanceTag::SelfOriginated];
    applies.review = Some(ReviewStatus::Reviewed {
        reviewer: agent("jun"),
        verdict: ReviewVerdict::Approved,
    });
    cases.push(("applies", applies, none, PermissionMode::Yolo));

    // AutoApplies — the inverse hazard, and the COMMON case in production.
    let pending = artifact('b', ArtifactKind::Patch, "spoke-2");
    cases.push(("auto-applies (policy)", pending, auto, PermissionMode::Yolo));

    // RefusedSelfReview.
    let mut self_review = artifact('b', ArtifactKind::Patch, "spoke-2");
    self_review.provenance = vec![ProvenanceTag::SelfOriginated];
    self_review.review = Some(ReviewStatus::Reviewed {
        reviewer: agent("spoke-2"),
        verdict: ReviewVerdict::Approved,
    });
    cases.push((
        "refused: self-review",
        self_review,
        none,
        PermissionMode::Yolo,
    ));

    // RefusedPlanMode.
    let plan = artifact('b', ArtifactKind::Patch, "spoke-2");
    cases.push(("refused: plan mode", plan, auto, PermissionMode::Plan));

    // RefusedNoProvenance.
    let mut unstamped = artifact('b', ArtifactKind::Patch, "spoke-2");
    unstamped.provenance = vec![];
    cases.push((
        "refused: no provenance",
        unstamped,
        auto,
        PermissionMode::Yolo,
    ));

    // RefusedChangesRequested / RefusedRejected — must stay distinguishable.
    let mut changes = artifact('b', ArtifactKind::Patch, "spoke-2");
    changes.review = Some(ReviewStatus::Reviewed {
        reviewer: agent("jun"),
        verdict: ReviewVerdict::ChangesRequested,
    });
    cases.push(("changes requested", changes, none, PermissionMode::Yolo));
    let mut rejected = artifact('b', ArtifactKind::Patch, "spoke-2");
    rejected.review = Some(ReviewStatus::Reviewed {
        reviewer: agent("jun"),
        verdict: ReviewVerdict::Rejected,
    });
    cases.push(("rejected", rejected, none, PermissionMode::Yolo));

    for (expected, subject, policy, mode) in cases {
        let mut panel = panel_from(
            vec![RoomEvent::ArtifactCreated {
                artifact: subject.clone(),
            }],
            policy,
            mode,
        );
        let painted = paint(&mut panel, 120);
        assert!(
            painted.contains(&format!("· {expected}")),
            "expected suffix `· {expected}`, got: {painted}"
        );
    }

    // 🔴 The two "not approved" rows must never collapse into one string.
    assert_ne!(
        rustain::adapters::tui::widgets::artifacts_panel::decision_suffix(
            PatchDisposition::RefusedChangesRequested
        ),
        rustain::adapters::tui::widgets::artifacts_panel::decision_suffix(
            PatchDisposition::RefusedRejected
        )
    );
}

/// **AC4 seventh mutant.** `AwaitingReview` has NO decision suffix — the row's
/// `<state>` field already reads `pending`. Emitting one renders `pending`
/// twice and stops matching the UX line.
///
/// The `AwaitingReview` row must equal the addendum's rendered shape: kind,
/// id prefix, `· pending`, `· from <producer>`, and the `▲ no reviewer` hazard,
/// with no decision clause between them.
#[test]
fn the_awaiting_review_row_matches_the_ux_line_and_renders_pending_exactly_once() {
    use rustain::adapters::tui::widgets::artifacts_panel::artifact_row;

    let subject = artifact('b', ArtifactKind::Patch, "spoke-2");
    let room = OrchestrationRoom::project_for_host(
        OrchestrationRoomId::parse("room-test").expect("room id"),
        vec![RoomEvent::ArtifactCreated {
            artifact: subject.clone(),
        }],
        "host-A",
    );
    let rendered = artifact_row(
        room.artifacts().get(&subject.id).expect("folded"),
        &room,
        PermissionMode::Yolo,
        &MergeBackPolicy::default(),
    );
    assert_eq!(
        rendered, "patch bbbbbb · pending · from spoke-2 · apply: never attempted  ▲ no reviewer",
        "UX-DR-ROOM-08's rendered shape, with the addendum's illustrative column padding \
         collapsed to `/room`'s existing two-space hazard separator"
    );
    assert_eq!(
        rendered.matches("pending").count(),
        1,
        "inventing a `· pending` decision suffix double-renders the state"
    );
    assert_eq!(
        rustain::adapters::tui::widgets::artifacts_panel::decision_suffix(
            PatchDisposition::AwaitingReview
        ),
        None
    );
}

/// **AC4 eighth mutant.** Eligible and policy-driven rows use distinct tense:
/// `applies` names what confirmation can do; `auto-applies (policy)` names the
/// fan-out path.
#[test]
fn eligible_rows_name_the_confirmed_apply_front_door() {
    use rustain::adapters::tui::widgets::artifacts_panel::decision_suffix;

    let applies = decision_suffix(PatchDisposition::Applies).expect("suffix");
    assert_eq!(applies, "applies");
    let auto = decision_suffix(PatchDisposition::AutoApplies).expect("suffix");
    assert_eq!(
        auto, "auto-applies (policy)",
        "AutoApplies describes a write that ALREADY HAPPENED and keeps its tense"
    );
    assert_ne!(applies, auto, "the two rows must stay distinguishable");
}

/// **AC4 sixth mutant / `DF-18-3a-MERGEBACK-POLICY-VISIBILITY`'s own keystone.**
/// Hardcode the policy clause instead of reading the live `MergeBackPolicy` and
/// this fires: flipping `auto_approve_user_originated` must move the rendered
/// annotation.
#[test]
fn flipping_the_merge_back_policy_moves_the_rendered_annotation() {
    let subject = artifact('b', ArtifactKind::Patch, "spoke-2");
    let events = vec![RoomEvent::ArtifactCreated {
        artifact: subject.clone(),
    }];

    let mut policy_on = panel_from(
        events.clone(),
        MergeBackPolicy {
            auto_approve_user_originated: true,
        },
        PermissionMode::Yolo,
    );
    let on = paint(&mut policy_on, 120);
    assert!(on.contains("· auto-applies (policy)"), "{on}");

    let mut policy_off = panel_from(events, MergeBackPolicy::default(), PermissionMode::Yolo);
    let off = paint(&mut policy_off, 120);
    assert!(!off.contains("auto-applies"), "{off}");
    assert!(off.contains("▲ no reviewer"), "{off}");
}

/// **AC4 drill-down.** `/artifact show <id>` renders the effective policy,
/// permission mode, front-door eligibility, and journal-projected outcome.
#[test]
fn the_drill_down_names_policy_eligibility_and_apply_state() {
    use rustain::adapters::tui::handlers::artifact_command::{disposition_sentence, render_show};

    let mut applies = artifact('b', ArtifactKind::Patch, "spoke-2");
    applies.provenance = vec![ProvenanceTag::SelfOriginated];
    applies.review = Some(ReviewStatus::Reviewed {
        reviewer: agent("jun"),
        verdict: ReviewVerdict::Approved,
    });
    let room = OrchestrationRoom::project_for_host(
        OrchestrationRoomId::parse("room-test").expect("room id"),
        vec![RoomEvent::ArtifactCreated {
            artifact: applies.clone(),
        }],
        "host-A",
    );
    let rendered = render_show(
        room.artifacts().get(&applies.id).expect("folded"),
        &room,
        PermissionMode::Yolo,
        &MergeBackPolicy {
            auto_approve_user_originated: true,
        },
        Ok(b"diff --git a/x b/x\n"),
    );
    let expected_decision = disposition_sentence(PatchDisposition::Applies);
    let decision = rendered
        .lines()
        .find_map(|line| line.strip_prefix("  decision  "))
        .expect("drill-down decision");
    assert_eq!(decision, expected_decision, "{rendered}");
    assert!(
        rendered.contains("auto_approve_user_originated = true"),
        "OPEN-DR-4: the effective policy is surfaced, sourced from the value the apply \
         path uses: {rendered}"
    );
    assert!(rendered.contains("permission mode = Yolo"), "{rendered}");
    // ⛔ Never label the id "the content hash": for a patch it is namespaced by
    // producer + authority while `content_hash` is body-only.
    assert!(!rendered.contains("content hash"), "{rendered}");
    assert!(rendered.contains("body hash"), "{rendered}");
}

// ═══════════════════════════════ AC5 ═══════════════════════════════

/// **AC5 keystone (a), behavioural round trip.** Drive the real
/// `record_verdict` seam against a real journal, read the **persisted file**
/// back, and assert it gained exactly one `PatchReviewed` line carrying the
/// operator's `AgentId` and `ReviewVerdict::Approved`. Then re-fold and assert
/// the row's disposition moved `AwaitingReview → Applies`.
///
/// 🔴 **Fixture is pinned deliberately.** A captured patch is `Pending` with
/// **non-empty** provenance, so under production's `auto_approve_user_originated:
/// true` a *non*-self-originated pending patch is already `AutoApplies` and the
/// pre-verdict assertion would simply be false. The fixture is
/// `SelfOriginated` **and** the recorder is composed with
/// `MergeBackPolicy::default()`, and the policy value is asserted in this same
/// test so the precondition cannot drift silently.
///
/// **Positive control:** a `request-changes` verdict afterwards produces a
/// **second** journal line and the row moves `Applies → RefusedChangesRequested`
/// — proving the verb is not approve-only and that re-review behaves as
/// documented.
///
/// Mutant: emit to the bus before appending to the journal → durable-first is
/// violated. Mutant: bypass `PatchMergeBack::review` and append
/// `RoomEvent::PatchReviewed` directly → the store-identity check and the
/// `kind != Patch` guard are skipped.
#[tokio::test]
async fn the_verdict_verb_appends_durably_and_moves_the_row_on_the_next_fold() {
    let harness = VerdictHarness::new().await;
    let patch = harness.capture_self_originated_patch().await;

    assert!(
        !harness.policy().auto_approve_user_originated,
        "the pre-verdict disposition is only AwaitingReview under a policy-off composition"
    );
    let room = harness.fold().await;
    assert_eq!(
        patch_disposition(
            room.artifacts().get(&patch.id).expect("folded"),
            OwnershipKind::Owned,
            PermissionMode::Yolo,
            &harness.policy()
        ),
        PatchDisposition::AwaitingReview
    );

    let before = harness.journal_lines();
    let (message, event) = harness
        .review(&patch.id.as_str()[..6], ReviewVerdict::Approved)
        .await
        .expect("the verdict verb records");
    assert!(message.contains("recorded approved"), "{message}");
    assert!(
        message.contains("Nothing was applied yet") && message.contains("/artifact apply <id>"),
        "approval must point to the separate confirmed write: {message}"
    );
    assert!(matches!(event, Some(RoomEvent::PatchReviewed { .. })));

    let after = harness.journal_lines();
    assert_eq!(
        after.len(),
        before.len() + 1,
        "exactly one durable line is appended"
    );
    let appended = after.last().expect("the appended line");
    assert!(
        appended.contains("\"event\":\"patch_reviewed\""),
        "{appended}"
    );
    assert!(appended.contains("\"verdict\":\"approved\""), "{appended}");
    assert!(
        appended.contains(&format!(
            "\"reviewer\":\"{}\"",
            AgentId::local_operator().as_str()
        )),
        "the operator's AgentId is the attribution: {appended}"
    );

    let room = harness.fold().await;
    assert_eq!(
        patch_disposition(
            room.artifacts().get(&patch.id).expect("folded"),
            OwnershipKind::Owned,
            PermissionMode::Yolo,
            &harness.policy()
        ),
        PatchDisposition::Applies,
        "the row's decision moves on the next fold"
    );

    // Positive control — re-review is permitted and the latest verdict governs.
    let (_, _) = harness
        .review(&patch.id.as_str()[..6], ReviewVerdict::ChangesRequested)
        .await
        .expect("the verb is not approve-only");
    let final_lines = harness.journal_lines();
    assert_eq!(
        final_lines.len(),
        after.len() + 1,
        "the second verdict is a SECOND durable line — both survive in the log (NFR63)"
    );
    let room = harness.fold().await;
    assert_eq!(
        patch_disposition(
            room.artifacts().get(&patch.id).expect("folded"),
            OwnershipKind::Owned,
            PermissionMode::Yolo,
            &harness.policy()
        ),
        PatchDisposition::RefusedChangesRequested
    );

    // ⛔ No file outside `.rustain/` was written.
    harness.assert_no_write_outside_rustain();
}

/// **AC5 keystone (a), behavioural, through the named front door.** Real
/// input text → real `InputAction::ExecuteCommand` → the exact dispatch-arm
/// callee (`artifact_bridge::artifact_command`) with a real `AppState` whose
/// composition-root slots hold the real journal and the real
/// `JournalPatchReview` port → the persisted journal file gains exactly one
/// `PatchReviewed` line, and the panel's refold moves the row's disposition
/// `AwaitingReview → Applies` **without a reopen** (the dispatch arm's
/// `refresh_panel` ran).
///
/// The seam-level round trip above proves the gated write; this proves the
/// operator can REACH it — command parsing, `AppState` composition, warning
/// delivery, and the post-verdict refold all run here for real. Same
/// `event_loop::run` caveat as the AC3 front-door keystone: the two dispatch
/// lines inside `run` are TTY-bound (`DF-CR-14-4a-6`) and stay pinned by the
/// structural ratchet.
#[tokio::test]
async fn the_verdict_command_reaches_the_journal_through_the_real_dispatch_path() {
    use rustain::adapters::tui::app::{InputAction, submit_message_for_test};
    use rustain::adapters::tui::state::TuiState;

    let harness = VerdictHarness::new().await;
    let patch = harness.capture_self_originated_patch().await;
    assert!(
        !harness.policy().auto_approve_user_originated,
        "the pre-verdict disposition is only AwaitingReview under a policy-off composition"
    );
    let app_state = artifact_app_state(
        harness.workspace.path(),
        Some(harness.recorder.clone()),
        None,
    );
    let mut state = TuiState::new(120, 40);

    let before = harness.journal_lines();
    state.input_buffer = format!("/artifact review {} approve", &patch.id.as_str()[..6]);
    let action = submit_message_for_test(&mut state);
    let InputAction::ExecuteCommand { name, args } = action else {
        panic!("/artifact review must route to ExecuteCommand, got {action:?}");
    };
    assert_eq!(name, "artifact");

    // The exact callee the dispatch arm invokes.
    rustain::infrastructure::runtime::artifact_bridge::artifact_command(
        &mut state,
        "conv",
        args.as_deref(),
        &app_state,
        PermissionMode::Yolo,
    )
    .await;

    let after = harness.journal_lines();
    assert_eq!(
        after.len(),
        before.len() + 1,
        "exactly one durable line is appended"
    );
    let appended = after.last().expect("the appended line");
    assert!(
        appended.contains("\"event\":\"patch_reviewed\""),
        "{appended}"
    );
    assert!(appended.contains("\"verdict\":\"approved\""), "{appended}");
    assert!(
        appended.contains(&format!(
            "\"reviewer\":\"{}\"",
            AgentId::local_operator().as_str()
        )),
        "the operator's AgentId is the attribution: {appended}"
    );

    // The dispatch arm refolded the panel: the row's decision moved without a
    // reopen.
    let room = state
        .artifacts_panel
        .room
        .as_ref()
        .expect("the panel holds the post-verdict fold");
    let folded = room.artifacts().get(&patch.id).expect("artifact in fold");
    assert!(
        matches!(
            folded.review,
            Some(ReviewStatus::Reviewed {
                verdict: ReviewVerdict::Approved,
                ..
            })
        ),
        "the refold carries the verdict: {:?}",
        folded.review
    );
    assert_eq!(
        patch_disposition(
            folded,
            OwnershipKind::Owned,
            PermissionMode::Yolo,
            &harness.policy()
        ),
        PatchDisposition::Applies,
        "the row's decision moved AwaitingReview → Applies"
    );

    // ⛔ Nothing outside `.rustain/` was written.
    harness.assert_no_write_outside_rustain();
}

#[tokio::test]
async fn apply_command_previews_without_mutation_then_accept_runs_the_real_effect_arm() {
    use rustain::adapters::tui::app::{InputAction, handle_input, submit_message_for_test};
    use rustain::adapters::tui::handlers::artifact_command::resolve_apply_card;
    use rustain::adapters::tui::state::TuiState;
    use rustain::domain::events::DomainInputEvent;

    let harness = VerdictHarness::new().await;
    std::fs::write(harness.workspace.path().join("x"), "").expect("empty target");
    let patch = harness.capture_self_originated_patch().await;
    harness
        .review(&patch.id.as_str()[..6], ReviewVerdict::Approved)
        .await
        .expect("approve");
    let executor = Some(harness.merge_back.clone()
        as std::sync::Arc<dyn rustain::domain::ports::PatchApplyExecutor>);
    let app_state = artifact_app_state(
        harness.workspace.path(),
        Some(harness.recorder.clone()),
        executor,
    );
    let mut state = TuiState::new(120, 40);
    let before = harness.journal_lines();

    state.input_buffer = format!("/artifact apply {}", &patch.id.as_str()[..6]);
    let InputAction::ExecuteCommand { name, args } = submit_message_for_test(&mut state) else {
        panic!("apply command must route to ExecuteCommand");
    };
    assert_eq!(name, "artifact");
    rustain::infrastructure::runtime::artifact_bridge::artifact_command(
        &mut state,
        "conv",
        args.as_deref(),
        &app_state,
        PermissionMode::Yolo,
    )
    .await;

    let card = state.pending_artifact_card.as_ref().expect("decision card");
    assert!(
        card.files.iter().any(|path| path == "x"),
        "{:?}",
        card.files
    );
    assert_eq!(card.workspace, harness.workspace.path());
    assert_eq!(
        std::fs::read_to_string(harness.workspace.path().join("x")).expect("target"),
        "",
        "opening the card must not mutate the workspace"
    );
    assert_eq!(
        harness.journal_lines(),
        before,
        "no apply record before accept"
    );

    assert_eq!(
        handle_input(&mut state, &DomainInputEvent::KeyPress('y')),
        InputAction::ApplyCardAccept
    );
    let card = resolve_apply_card(&mut state, true).expect("accepted card");
    rustain::infrastructure::runtime::artifact_bridge::apply_confirmed_card(
        &mut state,
        &app_state,
        card,
        PermissionMode::Yolo,
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(harness.workspace.path().join("x")).expect("applied file"),
        "x\n"
    );
    let after = harness.journal_lines();
    assert_eq!(after.len(), before.len() + 2, "Started + Resolved");
    assert!(after[before.len()].contains("\"applier\":\"operator\""));
    assert!(after[before.len() + 1].contains("\"outcome\":\"applied\""));
}

/// **AC5 second mutant (ruling P3's, the preflight's highest-value find).**
/// Build the `ArtifactRef` from the **store** instead of resolving through
/// `room.artifacts()` and a verdict on an id absent from this room's fold is
/// journaled and **vanishes** — `review()` validates against the store only, and
/// the fold is a silent no-op.
///
/// The command must be refused with a named error and must append **nothing**.
#[tokio::test]
async fn an_artifact_in_the_store_but_absent_from_this_rooms_fold_is_refused() {
    let harness = VerdictHarness::new().await;
    let orphan = harness.store_only_patch().await;

    let before = harness.journal_lines();
    let error = harness
        .review(&orphan.id.as_str()[..6], ReviewVerdict::Approved)
        .await
        .expect_err("a verdict that would vanish must be refused, never Ok(())");
    assert!(
        error.contains("no artifact in this room's fold"),
        "the refusal names the reason: {error}"
    );
    assert_eq!(
        harness.journal_lines().len(),
        before.len(),
        "nothing may be appended for an unresolvable id"
    );
}

/// **AC5 third mutant (ruling P3's).** Resolve an ambiguous prefix by taking the
/// first match and this fires: the command must refuse and NAME the candidates.
#[tokio::test]
async fn an_ambiguous_prefix_is_refused_with_its_candidates_named() {
    use rustain::adapters::tui::handlers::artifact_command::{ResolveError, resolve_artifact};

    let mut first = artifact('a', ArtifactKind::Patch, "spoke-1");
    first.id = ArtifactId::from(
        ContentHash::parse_hex(&format!("abcdef{}", "0".repeat(58))).expect("hex"),
    );
    let mut second = artifact('a', ArtifactKind::Patch, "spoke-1");
    second.id = ArtifactId::from(
        ContentHash::parse_hex(&format!("abcdef{}", "1".repeat(58))).expect("hex"),
    );
    let room = OrchestrationRoom::project_for_host(
        OrchestrationRoomId::parse("room-test").expect("room id"),
        vec![
            RoomEvent::ArtifactCreated {
                artifact: first.clone(),
            },
            RoomEvent::ArtifactCreated {
                artifact: second.clone(),
            },
        ],
        "host-A",
    );
    match resolve_artifact(&room, "abcdef") {
        Err(ResolveError::Ambiguous { candidates, .. }) => {
            assert_eq!(candidates.len(), 2, "{candidates:?}");
            assert!(candidates.iter().any(|c| c.starts_with("abcdef0")));
            assert!(candidates.iter().any(|c| c.starts_with("abcdef1")));
        }
        other => panic!("an ambiguous prefix must never silently pick one: {other:?}"),
    }
    // Positive control: a longer prefix resolves.
    assert!(resolve_artifact(&room, &first.id.as_str()[..8]).is_ok());
    // Unknown id refuses.
    assert!(matches!(
        resolve_artifact(&room, "ffffff"),
        Err(ResolveError::Unknown(_))
    ));
}

/// **AC5 — the `DurableContent` seam, ruling P2.**
///
/// ⛔ **Not a behavioural test that a `Viewer` is refused in production.**
/// `acting_principal()` returns `AgentId::local_operator()` unconditionally, so
/// `local_room_role` is always `Owner` and the decision is a **constant
/// function**: such a test is un-fireable and would be a Rule-0 vacuous
/// keystone. This drives the pure decision core, and is labelled as covering
/// the **core**, not the production path.
#[test]
fn the_room_edit_core_denies_a_viewer_durable_content_edits() {
    use rustain::domain::services::room_role::{local_room_role, room_edit_decision};

    assert_eq!(
        room_edit_decision(RoomRole::Viewer, RoomEditKind::DurableContent),
        RoomEditDecision::Deny
    );
    assert_eq!(
        room_edit_decision(RoomRole::Unknown, RoomEditKind::DurableContent),
        RoomEditDecision::Deny
    );
    assert_eq!(
        room_edit_decision(RoomRole::Editor, RoomEditKind::DurableContent),
        RoomEditDecision::Allow
    );
    assert_eq!(
        room_edit_decision(RoomRole::Owner, RoomEditKind::DurableContent),
        RoomEditDecision::Allow
    );
    // The production principal, named. This is why the behavioural half cannot
    // exist: there is exactly one constructible local principal today.
    assert_eq!(local_room_role(&AgentId::local_operator()), RoomRole::Owner);
}

/// **AC5 structural ratchet (Rule 4) — the seam's actual evidence.**
///
/// A behavioural test cannot prove "every durable-write path in the artifact
/// bridge routes through `room_edit_decision`", because the gate never refuses
/// in this build. The ratchet does: the bridge must contain the gate, and the
/// gate must precede the recorder call.
///
/// ⛔ **This is a room-content seam and nothing more.** It must not reach a
/// `CapabilityToken`, an `AuthorityProvider` decision, or an approval
/// fingerprint.
#[test]
fn every_durable_write_in_the_artifact_bridge_routes_through_the_room_edit_gate() {
    let bridge = artifact_bridge_source();
    let review = &bridge[bridge
        .find("pub async fn record_verdict(")
        .expect("review seam")..];
    let review_gate = review
        .find("room_edit_decision(local_room_role(acting), RoomEditKind::DurableContent)")
        .expect("review gate");
    let recorder = review.find(".record_verdict(").expect("review port call");
    assert!(review_gate < recorder, "review gate precedes its port");

    let apply = &bridge[bridge
        .find("pub async fn apply_artifact(")
        .expect("apply seam")..];
    let apply_gate = apply
        .find("room_edit_decision(local_room_role(acting), RoomEditKind::DurableContent)")
        .expect("apply gate");
    let executor = apply.find(".apply_patch(").expect("apply port call");
    assert!(apply_gate < executor, "apply gate precedes its port");
    assert_eq!(bridge.matches(".record_verdict(").count(), 1);
    assert_eq!(bridge.matches(".apply_patch(").count(), 1);
    assert!(
        !bridge.contains("append_room") && !bridge.contains("RoomJournal>"),
        "the bridge must not hold a raw journal writer — the port is the only write path"
    );
    // ⛔ Never describe the constant-function seam as enforcement.
    let lowered = bridge.to_ascii_lowercase();
    assert!(
        lowered.contains("seam, not enforcement"),
        "ruling P2 requires the constant-function caveat to be stated in source"
    );
    for forbidden in ["capabilitytoken", "authorityprovider", "fingerprint"] {
        assert!(
            !lowered.contains(forbidden),
            "the room-content seam must not reach `{forbidden}`"
        );
    }
    assert!(
        !bridge.contains("review_and_apply"),
        "the operator front door must not revive the dead review-and-apply wrapper"
    );

    // 🔴 Durable-first, bus-second — proven STRUCTURALLY (Rule 4), because the
    // behavioural difference is only observable on a journal failure the gated
    // seam cannot be made to produce from a test. `record_verdict` holds no bus
    // handle at all: the ONE bus emission lives in `PatchMergeBack::persist`,
    // strictly after `append_room` succeeds.
    let gated = &bridge[bridge
        .find("pub async fn record_verdict(")
        .expect("the gated seam exists")..];
    let gated = &gated[..gated.find("\n}\n").expect("it closes")];
    for forbidden in ["event_bus", "emit_domain", "emit("] {
        assert!(
            !gated.contains(forbidden),
            "the gated durable write must not reach the bus — durable-first is the \
             ordering contract, and `persist` emits only after `Ok`: `{forbidden}`"
        );
    }
    // ⛔ The dispatch shell must NOT re-emit the returned event: `persist`
    // already put it on the bus, so a second emit shows every verdict TWICE to
    // daemon, wire-log and transcript subscribers. The event is returned as
    // evidence of what was appended, not as a re-emission source.
    let dispatch = &bridge[bridge
        .find("async fn dispatch(")
        .expect("the dispatch shell exists")..];
    let dispatch = &dispatch[..dispatch.find("\n}\n").expect("it closes")];
    assert!(
        !dispatch.contains(".emit_domain("),
        "the verdict path emits exactly once, inside `PatchMergeBack::persist` — \
         a dispatch-side emit double-counts every verdict on the bus"
    );
    // And the single real emission is durable-first in `persist` itself.
    let merge_back = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/infrastructure/orchestrator/merge_back.rs"
    ))
    .expect("read merge_back.rs");
    let persist = &merge_back[merge_back
        .find("async fn persist(")
        .expect("the persist shell exists")..];
    let persist = &persist[..persist.find("\n}\n").expect("it closes")];
    let append = persist.find(".append_room(").expect("the journal append");
    let emit = persist.find(".emit_domain(").expect("the bus emit");
    assert!(
        append < emit,
        "durable-first: the journal append must precede the bus emission"
    );
    assert_eq!(
        persist.matches(".emit_domain(").count(),
        1,
        "exactly one bus emit on the durable path"
    );
}

// ═══════════════════════════════ AC6 ═══════════════════════════════

/// **AC6 wording ratchet — PHRASES, never bare substrings.**
///
/// `STRUCTURAL_REPLAY_CLAIM` deliberately contains the word `authenticated`
/// ("not payload- or provenance-authenticated") and `live` is a substring of
/// `delivered`, so a bare-substring scan is RED against the very clause that
/// keeps the panel honest. The needle list is 18.3a's, verbatim, extended with
/// this story's apply-claim prohibitions.
#[test]
fn the_artifact_panel_makes_no_integrity_or_apply_claim_it_cannot_back() {
    let mut applies = artifact('b', ArtifactKind::Patch, "spoke-2");
    applies.provenance = vec![ProvenanceTag::SelfOriginated];
    applies.review = Some(ReviewStatus::Reviewed {
        reviewer: agent("jun"),
        verdict: ReviewVerdict::Approved,
    });
    let mut panel = panel_from(
        vec![RoomEvent::ArtifactCreated {
            artifact: applies.clone(),
        }],
        MergeBackPolicy::default(),
        PermissionMode::Yolo,
    );
    let painted = paint(&mut panel, 120);
    let lowered = painted.to_ascii_lowercase();
    for forbidden in [
        "tamper",
        "cryptograph",
        "provably",
        "authentic journal",
        "authenticated record",
        "is authentic",
        "is live",
    ] {
        assert!(
            !lowered.contains(forbidden),
            "forbidden wording '{forbidden}': {painted}"
        );
    }
    // The required half: the honest negation must be present, painted verbatim.
    assert!(
        lowered.contains("structurally replayable (not"),
        "{painted}"
    );
    assert!(lowered.contains("payload- or"), "{painted}");
    assert!(
        lowered.contains("provenance-authenticated"),
        "the footer must state what the journal does NOT prove: {painted}"
    );

    // The drill-down must name eligibility without claiming success.
    use rustain::adapters::tui::handlers::artifact_command::{disposition_sentence, render_show};
    let room = OrchestrationRoom::project_for_host(
        OrchestrationRoomId::parse("room-test").expect("room id"),
        vec![RoomEvent::ArtifactCreated {
            artifact: applies.clone(),
        }],
        "host-A",
    );
    let rendered = render_show(
        room.artifacts().get(&applies.id).expect("folded"),
        &room,
        PermissionMode::Yolo,
        &MergeBackPolicy::default(),
        Ok(b""),
    );
    let expected_decision = disposition_sentence(PatchDisposition::Applies);
    let decision = rendered
        .lines()
        .find_map(|line| line.strip_prefix("  decision  "))
        .expect("drill-down decision");
    assert_eq!(decision, expected_decision, "{rendered}");
    assert!(rendered.contains("apply: never attempted"), "{rendered}");
}

/// **AC6 scope ratchet.** This cut adds the confirmed apply front door but
/// still mints no assignment vocabulary.
#[test]
fn this_cut_ships_the_apply_path_and_no_assignment_vocabulary() {
    for relative in [
        "src/infrastructure/runtime/artifact_bridge.rs",
        "src/adapters/tui/handlers/artifact_command.rs",
        "src/adapters/tui/widgets/artifacts_panel.rs",
        "src/infrastructure/orchestrator/artifact_review.rs",
    ] {
        let body = std::fs::read_to_string(format!("{}/{relative}", env!("CARGO_MANIFEST_DIR")))
            .unwrap_or_else(|error| panic!("read {relative}: {error}"));
        if relative.ends_with("artifact_bridge.rs") {
            assert!(body.contains("ArtifactCommandArgs::Apply"), "{relative}");
            assert!(body.contains("pub async fn apply_artifact("), "{relative}");
            assert_eq!(body.matches(".apply_patch(").count(), 1, "{relative}");
        }
        assert!(
            !body.contains("TicketAddressee::Node"),
            "{relative}: DF-18-3a-b-ASSIGN-MINT ships the assign verb, not this cut"
        );
    }
    // ⛔ The handlers layer stays free of infrastructure IMPORTS.
    //
    // Scanned per `use` line, not as a bare substring: `room_command.rs` and
    // this handler both carry an intra-doc link naming the bridge that performs
    // their I/O, and a substring scan would be RED against the very comment
    // that documents the boundary. Prose is not an import.
    let handler = std::fs::read_to_string(format!(
        "{}/src/adapters/tui/handlers/artifact_command.rs",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("read handler");
    for line in handler.lines() {
        let trimmed = line.trim_start();
        assert!(
            !(trimmed.starts_with("use ") && trimmed.contains("crate::infrastructure::")),
            "adapters/tui/handlers must not import infrastructure: {line}"
        );
        assert!(
            trimmed.starts_with("//") || !trimmed.contains("crate::infrastructure::"),
            "adapters/tui/handlers must not reference infrastructure in code: {line}"
        );
    }
    // ⛔ 18.4's role-parameterised projection is untouched.
    let projection_path = format!(
        "{}/src/adapters/a2a/projection.rs",
        env!("CARGO_MANIFEST_DIR")
    );
    if let Ok(projection) = std::fs::read_to_string(&projection_path) {
        assert!(
            !projection.contains("PatchDisposition") && !projection.contains("artifacts_panel"),
            "DF-18-3a-ROLE-PROJECTION belongs to 18.4"
        );
    }
}

// ─────────────────────────── verdict harness ───────────────────────────

/// A real `AppState` composed over `workspace`: the real journal read path
/// (`transparency` present) and, when supplied, the real `JournalPatchReview`
/// port. Everything else is inert noop composition, in the shape
/// `conformance_cancellation.rs` established.
///
/// Exists for the two front-door keystones: they drive the exact callees the
/// `InputAction::ExecuteCommand` dispatch arms invoke
/// (`artifact_bridge::artifacts_command` / `artifact_command`), which read the
/// journal and the port **through `AppState`** — a harness that bypasses it
/// proves the seam, not that the operator can reach it.
fn artifact_app_state(
    workspace: &std::path::Path,
    recorder: Option<std::sync::Arc<dyn rustain::domain::ports::PatchReviewRecorder>>,
    executor: Option<std::sync::Arc<dyn rustain::domain::ports::PatchApplyExecutor>>,
) -> rustain::infrastructure::runtime::app_state::AppState {
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use clap::Parser;
    use rustain::adapters::noop::{NoOpProvider, NoOpStorage};
    use rustain::domain::ports::StreamingProvider;
    use rustain::domain::services::plan_manager::PlanManager;
    use rustain::domain::services::plan_mode_injector::DefaultPlanInjector;
    use rustain::infrastructure::composition::ComposeContext;
    use rustain::infrastructure::runtime::agent_core::AgentCore;
    use rustain::infrastructure::runtime::app_state::AppState;
    use rustain::infrastructure::runtime::event_bus::EventBus;

    let approval_runtime = rustain::domain::services::approval_runtime::ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let provider_swap = Arc::new(ArcSwap::from_pointee(
        Arc::new(NoOpProvider) as Arc<dyn StreamingProvider>
    ));
    let (event_bus, domain_rx) = EventBus::new(16);
    let compose_snapshot = Arc::new(ComposeContext {
        workspace_path: workspace.to_path_buf(),
        project_context: rustain::domain::models::project_context::ProjectContext::empty(),
        storage: Arc::new(NoOpStorage) as Arc<dyn rustain::domain::ports::StoragePort>,
        skill_activator: Arc::new(rustain::adapters::skill_activation::SkillActivator::new()),
        mcp_servers: Vec::new(),
        include_builtin_tools: true,
        domain_tx: None,
        channel_turn_tx: None,
        tool_exposure: "static-full".into(),
        assembler: "passthrough".into(),
        skill_exposure: "l1-metadata".into(),
        skill_cache: Arc::new(rustain::infrastructure::skill_cache::SkillCache::new_in_memory()),
        sandbox_adapter: "noop".into(),
        sandbox_startup_policy: rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        sandbox_slot: Arc::new(ArcSwap::from_pointee(Arc::new(
            rustain::adapters::sandbox::NoOpSandbox,
        )
            as Arc<dyn rustain::domain::ports::SandboxManager>)),
        memory_slot: Arc::new(ArcSwap::from_pointee(
            Arc::new(rustain::adapters::noop::NoOpMemory)
                as Arc<dyn rustain::domain::ports::MemoryPort>,
        )),
        sandbox_policy: Arc::new(tokio::sync::RwLock::new(
            rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        )),
        memory_write_gate: Arc::new(tokio::sync::RwLock::new(())),
        #[cfg(feature = "meta-search")]
        search_config: rustain::domain::models::SearchConfig::default(),
        #[cfg(feature = "meta-search")]
        meta_search_engine: None,
        a2a_peers: Vec::new(),
    });
    let (mut app_state, _domain_rx) = AppState::new(
        Arc::new(event_bus),
        domain_rx,
        approval_runtime,
        Arc::new(tokio::sync::RwLock::new(
            rustain::domain::models::SandboxPolicy::Permissive,
        )),
        Arc::new(PlanManager::new(workspace.to_path_buf())),
        Arc::new(DefaultPlanInjector::new()),
        provider_swap,
        Arc::new(rustain::adapters::provider::ProviderRegistry::new()),
        Arc::new(rustain::adapters::noop::NoOpUsageLedger),
        Arc::new(rustain::adapters::budget::BudgetStateStore::new()),
        Arc::new(ArcSwap::from_pointee(
            rustain::domain::models::AppConfig::default(),
        )),
        Arc::new(AgentCore::test_noop()),
        None,
        compose_snapshot,
        Arc::new(ArcSwap::from_pointee(Arc::new(
            rustain::adapters::profile_resolver::noop::NoopProfileResolver,
        )
            as Arc<dyn rustain::domain::ports::ProfileResolver>)),
        rustain::adapters::cli::commands::Cli::try_parse_from(["rustain"]).expect("bare cli"),
        None,
        rustain::infrastructure::telemetry::ActiveRatioWindow::new_in_memory(),
        #[cfg(feature = "meta-search")]
        None,
    );
    // The composition-root slots, assigned exactly as startup.rs assigns them.
    app_state.transparency = Some(Arc::new(
        rustain::infrastructure::transparency::TransparencyService::new(
            Arc::new(
                rustain::infrastructure::subagent::node_journal::WorkspaceJournalReader::open_workspace(
                    workspace,
                ),
            ),
            workspace.to_path_buf(),
        ),
    ));
    app_state.patch_review = recorder;
    app_state.patch_apply = executor;
    app_state
}

/// Seeds a real workspace, a real `NodeJournal`, a real `FileSystemArtifactStore`
/// and the real `JournalPatchReview` port, then drives the **production**
/// `artifact_bridge::record_verdict` seam.
///
/// ⛔ **Forbidden bypass, deliberately not used:** calling
/// `PatchMergeBack::review` directly. That proves the service works — which
/// `conformance_cow_mergeback.rs` already proves — not that the operator can
/// reach it, which is the entire point of the story.
struct VerdictHarness {
    workspace: tempfile::TempDir,
    recorder: std::sync::Arc<rustain::infrastructure::orchestrator::JournalPatchReview>,
    merge_back: std::sync::Arc<rustain::infrastructure::orchestrator::PatchMergeBack>,
    store: std::sync::Arc<dyn rustain::domain::ports::ArtifactStore>,
    policy: MergeBackPolicy,
}

impl VerdictHarness {
    async fn new() -> Self {
        use rustain::adapters::artifact::FileSystemArtifactStore;
        use rustain::infrastructure::orchestrator::{JournalPatchReview, PatchMergeBack};
        use rustain::infrastructure::runtime::event_bus::EventBus;
        use rustain::infrastructure::subagent::NodeJournal;

        let workspace = tempfile::tempdir().expect("workspace");
        let store: std::sync::Arc<dyn rustain::domain::ports::ArtifactStore> =
            std::sync::Arc::new(FileSystemArtifactStore::new(workspace.path()));
        let journal = std::sync::Arc::new(
            NodeJournal::open_workspace(workspace.path())
                .await
                .expect("journal"),
        );
        let (event_bus, _rx) = EventBus::new(64);
        let merge_back = std::sync::Arc::new(PatchMergeBack::new(
            workspace.path().to_path_buf(),
            store.clone(),
            journal.clone(),
            std::sync::Arc::new(event_bus),
            std::sync::Arc::new(rustain::adapters::merge_back::GitPatchApplier),
        ));
        // 🔴 Policy OFF, deliberately and assertedly: a captured patch carries
        // non-empty provenance, so under production's `true` a non-self-originated
        // pending patch is already `AutoApplies`.
        let policy = MergeBackPolicy::default();
        let recorder = std::sync::Arc::new(JournalPatchReview::new(
            merge_back.clone(),
            store.clone(),
            policy,
        ));
        Self {
            workspace,
            recorder,
            merge_back,
            store,
            policy,
        }
    }

    fn policy(&self) -> MergeBackPolicy {
        use rustain::domain::ports::PatchReviewRecorder;
        self.recorder.effective_policy()
    }

    fn journal_path(&self) -> std::path::PathBuf {
        self.workspace
            .path()
            .join(".rustain")
            .join("rooms")
            .join(format!(
                "room-{}.jsonl",
                rustain::infrastructure::paths::workspace_hash(self.workspace.path())
            ))
    }

    fn journal_lines(&self) -> Vec<String> {
        std::fs::read_to_string(self.journal_path())
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Capture through the real `PatchMergeBack::capture` production path, so
    /// the artifact exists in both the store and the room fold.
    async fn capture_self_originated_patch(&self) -> ArtifactRef {
        use rustain::domain::models::UnifiedDiff;
        self.merge_back
            .capture(
                agent("spoke-2"),
                CapabilityTokenId::default(),
                vec![ProvenanceTag::SelfOriginated],
                vec![],
                HostBinding::new(
                    rustain::infrastructure::subagent::current_host_id(self.workspace.path()),
                    "ws",
                ),
                &UnifiedDiff::new(
                    rustain::domain::models::ProvisioningTier::ScratchCopy,
                    "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -0,0 +1 @@\n+x\n".to_owned(),
                ),
            )
            .await
            .expect("capture")
    }

    /// Put a patch in the STORE only — no `ArtifactCreated` line, so the room's
    /// fold has never seen it. This is the silent-no-op case ruling P3 names.
    async fn store_only_patch(&self) -> ArtifactRef {
        use rustain::domain::models::EvidenceArtifactDraft;
        self.store
            .put(
                EvidenceArtifactDraft {
                    kind: ArtifactKind::Patch,
                    producer: agent("spoke-9"),
                    authority: CapabilityTokenId::default(),
                    provenance: vec![ProvenanceTag::SelfOriginated],
                    depends_on: vec![],
                    review: Some(ReviewStatus::Pending),
                    host: HostBinding::new("host-A", "ws"),
                },
                b"diff --git a/y b/y\n",
            )
            .await
            .expect("store put")
    }

    async fn fold(&self) -> OrchestrationRoom {
        use rustain::domain::ports::RoomJournalReader;
        use rustain::infrastructure::runtime::artifact_bridge::fold_artifacts_read;
        use rustain::infrastructure::subagent::node_journal::WorkspaceJournalReader;

        let entries = RoomJournalReader::load_entries(&WorkspaceJournalReader::open_workspace(
            self.workspace.path(),
        ))
        .await
        .expect("load");
        fold_artifacts_read(
            entries,
            OrchestrationRoomId::parse("room-harness").expect("room id"),
            &rustain::infrastructure::subagent::current_host_id(self.workspace.path()),
        )
        .0
    }

    /// Drive the **production** gated seam.
    async fn review(
        &self,
        typed: &str,
        verdict: ReviewVerdict,
    ) -> Result<(String, Option<RoomEvent>), String> {
        let room = self.fold().await;
        rustain::infrastructure::runtime::artifact_bridge::record_verdict(
            &room,
            self.recorder.as_ref(),
            &AgentId::local_operator(),
            typed,
            verdict,
            PermissionMode::Yolo,
        )
        .await
    }

    /// ⛔ Nothing outside `.rustain/` may be written by a verdict.
    fn assert_no_write_outside_rustain(&self) {
        let mut stray = Vec::new();
        for entry in std::fs::read_dir(self.workspace.path()).expect("read workspace") {
            let entry = entry.expect("dir entry");
            if entry.file_name() != ".rustain" {
                stray.push(entry.path());
            }
        }
        assert!(
            stray.is_empty(),
            "the verdict verb wrote outside .rustain/: {stray:?}"
        );
    }
}

/// Compile-time witness that the harness fields are all load bearing.
#[allow(dead_code)]
fn _harness_fields_are_used(harness: &VerdictHarness) -> (&tempfile::TempDir, MergeBackPolicy) {
    (&harness.workspace, harness.policy)
}

/// A `JournalEntry` helper kept beside the fixture assertions so a future
/// author extends this file rather than minting a second fixture shape.
#[allow(dead_code)]
fn entry(seq: u64, event: RoomEvent) -> JournalEntry {
    JournalEntry::new(seq, JournalRecord::Room(event), 1_700_000_000_000)
}

/// **AC6 — the new target actually runs in CI.**
///
/// ⚠ A conformance file does NOT run unless a line names it. ⛔ And the
/// filename must carry no `a2a`:
/// `every_a2a_integration_test_is_wired_into_the_ci_a2a_lane` fails the
/// **default** lane for an unlisted `tests/*a2a*.rs`.
#[test]
fn ci_executes_this_story_target_in_both_lanes() {
    let ci = source(".github/workflows/ci.yml");
    let default_lane = ci
        .split("\n  check:\n")
        .nth(1)
        .expect("default check lane exists")
        .split("\n  skills-validation:\n")
        .next()
        .expect("default check lane is bounded");
    assert!(
        default_lane.contains("cargo test --test conformance_18_3a_c_artifacts"),
        "the default lane must execute this story target"
    );
    let a2a_lane = ci
        .split("\n  a2a:\n")
        .nth(1)
        .expect("A2A lane exists")
        .split("\n  mcp:\n")
        .next()
        .expect("A2A lane is bounded");
    assert!(
        a2a_lane.contains("--test conformance_18_3a_c_artifacts"),
        "the A2A lane must execute this story target"
    );
    assert!(
        !concat!(file!(), "").contains("a2a"),
        "this filename must not contain `a2a`"
    );
}

/// **AC6 — the `event_loop.rs` line budget, measured here as well as in the
/// 18.3c ratchet** so a reader of *this* story sees the number it spent.
///
/// The binding cap is `tests/conformance_18_3c_response_modes.rs`'s 11_321,
/// which runs in BOTH CI jobs; `tests/conformance.rs` runs only in the a2a job.
#[test]
fn the_artifact_surface_stayed_inside_the_event_loop_line_budget() {
    let lines = event_loop_source().lines().count();
    assert!(
        lines <= 11_321,
        "event_loop.rs has {lines} lines — put logic in artifact_bridge.rs, do not bump the cap"
    );
}
