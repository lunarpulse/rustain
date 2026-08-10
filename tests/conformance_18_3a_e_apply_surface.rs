//! Story 18.3a-e — operator patch-apply front door and outcome surface.
//!
//! Behavioural tests in this target enter through production command/card seams. Source
//! ratchets are secondary evidence only and always carry a positive control.

use rustain::adapters::tui::handlers::artifact_command::{
    ArtifactCommandArgs, parse_artifact_command,
};
use std::sync::Mutex;

use async_trait::async_trait;
use rustain::domain::models::{
    AgentId, ApplyOutcome, ArtifactId, ArtifactKind, ArtifactRef, CapabilityTokenId, ContentHash,
    EvidenceArtifact, HostBinding, OrchestrationRoom, OrchestrationRoomId, OwnershipKind,
    PermissionMode, ProvenanceTag, ReviewStatus, ReviewVerdict, RoomEvent,
};
use rustain::domain::ports::{PatchApplyExecutor, PatchApplyPortError};
use rustain::domain::services::patch_review::MergeBackPolicy;
use rustain::infrastructure::runtime::artifact_bridge::apply_artifact;

#[test]
fn the_apply_subverb_parses_one_projection_address() {
    assert_eq!(
        parse_artifact_command(false, Some("apply abc123")).expect("valid apply command"),
        ArtifactCommandArgs::Apply {
            id: "abc123".to_owned(),
        }
    );

    for invalid in ["apply", "apply abc123 extra"] {
        let error =
            parse_artifact_command(false, Some(invalid)).expect_err("invalid apply command");
        assert!(error.contains("/artifact apply <id>"), "{error}");
    }
}

fn hash(seed: char) -> ContentHash {
    ContentHash::parse_hex(&std::iter::repeat_n(seed, 64).collect::<String>()).expect("hash")
}

fn artifact(seed: char) -> ArtifactRef {
    EvidenceArtifact {
        id: ArtifactId::from(hash(seed)),
        kind: ArtifactKind::Patch,
        producer: AgentId::parse("spoke-1").expect("agent"),
        content_hash: hash('0'),
        authority: CapabilityTokenId::default(),
        provenance: vec![ProvenanceTag::UserOriginated],
        depends_on: Vec::new(),
        review: Some(ReviewStatus::Pending),
        host: HostBinding::new("host-A", "workspace"),
    }
}

fn room_with_verdict(patches: &[ArtifactRef], verdict: Option<ReviewVerdict>) -> OrchestrationRoom {
    let mut events = Vec::new();
    for patch in patches {
        events.push(RoomEvent::ArtifactCreated {
            artifact: patch.clone(),
        });
        events.push(RoomEvent::PatchCaptured {
            artifact: patch.id.clone(),
            producer: patch.producer.clone(),
        });
        if let Some(verdict) = verdict {
            events.push(RoomEvent::PatchReviewed {
                artifact: patch.id.clone(),
                reviewer: AgentId::local_operator(),
                verdict,
            });
        }
    }
    OrchestrationRoom::project(OrchestrationRoomId::default(), events)
}

#[derive(Default)]
struct RecordingExecutor {
    calls: Mutex<Vec<(ArtifactId, OwnershipKind, Option<AgentId>)>>,
}

#[async_trait]
impl PatchApplyExecutor for RecordingExecutor {
    async fn apply_patch(
        &self,
        artifact: ArtifactRef,
        ownership: OwnershipKind,
        _permission_mode: PermissionMode,
        _policy: MergeBackPolicy,
        applier: Option<AgentId>,
    ) -> Result<(), PatchApplyPortError> {
        self.calls
            .lock()
            .expect("calls")
            .push((artifact.id, ownership, applier));
        Ok(())
    }
}

#[tokio::test]
async fn the_operator_door_resolves_the_room_gates_once_and_attributes_the_apply() {
    let patch = artifact('a');
    let room = room_with_verdict(std::slice::from_ref(&patch), Some(ReviewVerdict::Approved));
    let executor = RecordingExecutor::default();
    let operator = AgentId::local_operator();

    let message = apply_artifact(
        &room,
        &executor,
        &operator,
        &patch.id.as_str()[..6],
        PermissionMode::Yolo,
        MergeBackPolicy::default(),
    )
    .await
    .expect("approved projected patch applies");

    assert!(message.contains("applied to the workspace"), "{message}");
    assert_eq!(
        *executor.calls.lock().expect("calls"),
        vec![(patch.id, OwnershipKind::Owned, Some(operator))]
    );
}

#[tokio::test]
async fn every_refusal_stops_before_the_apply_port() {
    let executor = RecordingExecutor::default();
    let operator = AgentId::local_operator();
    let approved = artifact('b');
    let rejected = room_with_verdict(
        std::slice::from_ref(&approved),
        Some(ReviewVerdict::Rejected),
    );
    let plan = room_with_verdict(
        std::slice::from_ref(&approved),
        Some(ReviewVerdict::Approved),
    );
    let auto = room_with_verdict(std::slice::from_ref(&approved), None);
    let mut first = artifact('c');
    first.id = ArtifactId::from(
        ContentHash::parse_hex(&format!("abcdef{}", "1".repeat(58))).expect("first id"),
    );
    let mut second = artifact('d');
    second.id = ArtifactId::from(
        ContentHash::parse_hex(&format!("abcdef{}", "2".repeat(58))).expect("second id"),
    );
    let ambiguous = room_with_verdict(&[first, second], Some(ReviewVerdict::Approved));

    for (room, typed, mode, policy, expected) in [
        (
            &rejected,
            &approved.id.as_str()[..6],
            PermissionMode::Yolo,
            MergeBackPolicy::default(),
            "rejected",
        ),
        (
            &plan,
            &approved.id.as_str()[..6],
            PermissionMode::Plan,
            MergeBackPolicy::default(),
            "plan",
        ),
        (
            &auto,
            &approved.id.as_str()[..6],
            PermissionMode::Yolo,
            MergeBackPolicy {
                auto_approve_user_originated: true,
            },
            "no completed apply is recorded yet",
        ),
        (
            &plan,
            "ffffff",
            PermissionMode::Yolo,
            MergeBackPolicy::default(),
            "no artifact",
        ),
        (
            &ambiguous,
            "abcdef",
            PermissionMode::Yolo,
            MergeBackPolicy::default(),
            "matches 2 artifacts",
        ),
    ] {
        let error = apply_artifact(room, &executor, &operator, typed, mode, policy)
            .await
            .expect_err("the door refuses");
        assert!(error.contains(expected), "{error}");
    }
    assert!(executor.calls.lock().expect("calls").is_empty());
}

/// Walk `dir` recursively and collect every `.rs` file's path + contents, for
/// structural ratchets that must hold across the whole production tree.
fn collect_sources(dir: &std::path::Path, out: &mut Vec<(std::path::PathBuf, String)>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            if let Ok(contents) = std::fs::read_to_string(&path) {
                out.push((path, contents));
            }
        }
    }
}

#[test]
fn the_production_bridge_has_one_apply_port_call_and_one_disposition_core() {
    let root = env!("CARGO_MANIFEST_DIR");
    let bridge = std::fs::read_to_string(format!(
        "{root}/src/infrastructure/runtime/artifact_bridge.rs"
    ))
    .expect("bridge source");
    let panel = std::fs::read_to_string(format!(
        "{root}/src/adapters/tui/widgets/artifacts_panel.rs"
    ))
    .expect("panel source");
    let core = std::fs::read_to_string(format!("{root}/src/domain/services/patch_review.rs"))
        .expect("decision core source");
    assert_eq!(bridge.matches(".apply_patch(").count(), 1);
    assert_eq!(core.matches("pub fn operator_patch_decision(").count(), 1);
    let derivation = &core[core
        .find("pub fn operator_patch_decision(")
        .expect("shared derivation")..];
    assert!(derivation.contains("patch_disposition("));
    assert!(
        bridge.contains("operator_patch_decision(") && panel.contains("operator_patch_decision("),
        "row and door must enter the shared domain derivation"
    );
    // Neither consumer may carry its own disposition derivation — that is the
    // AC1 mutant (a second `patch_disposition(` that agrees today but drifts
    // tomorrow).
    assert!(
        !bridge.contains("patch_disposition("),
        "the door must not carry a second disposition derivation"
    );
    assert!(
        !panel.contains("patch_disposition("),
        "the row must not carry a second disposition derivation"
    );
    // The disposition derivation needle lives in exactly one production file:
    // the shared core. Scanning all of `src/` catches a second core added
    // anywhere, not just in the two consumers above.
    let mut sources = Vec::new();
    collect_sources(std::path::Path::new(&format!("{root}/src")), &mut sources);
    let with_derivation: Vec<&std::path::Path> = sources
        .iter()
        .filter(|(_, src)| src.contains("patch_disposition("))
        .map(|(path, _)| path.as_path())
        .collect();
    assert_eq!(
        with_derivation,
        vec![std::path::Path::new(&format!(
            "{root}/src/domain/services/patch_review.rs"
        ))],
        "patch_disposition must be defined and called only in the shared core"
    );
}

fn pending_card(
    prior_focus: rustain::domain::models::FocusState,
) -> rustain::adapters::tui::state::PendingApplyCard {
    rustain::adapters::tui::state::PendingApplyCard {
        conversation_id: "conversation".to_owned(),
        artifact: artifact('e'),
        files: vec!["src/lib.rs".to_owned(), "tests/apply.rs".to_owned()],
        workspace: std::path::PathBuf::from("/workspace"),
        prior_focus,
        predates_apply_records: true,
    }
}

#[test]
fn decline_clears_the_card_and_restores_prior_focus_without_an_effect() {
    use rustain::adapters::tui::handlers::artifact_command::resolve_apply_card;
    use rustain::domain::models::FocusState;
    use rustain::domain::models::visual::{ConfirmationType, OverlayType};

    let prior = FocusState::Input;
    let mut state = rustain::adapters::tui::state::TuiState::new(80, 24);
    state.pending_apply_card = Some(pending_card(prior.clone()));
    state.focus = FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::ArtifactApply));
    assert!(resolve_apply_card(&mut state, false).is_none());
    assert!(state.pending_apply_card.is_none());
    assert_eq!(state.focus, prior);
}

#[test]
fn the_painted_card_names_impact_wedge_risk_and_its_dispatch_keys() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use rustain::adapters::tui::widgets::{apply_card, inline_card};

    let state = rustain::adapters::tui::state::TuiState::new(80, 24);
    let card = pending_card(rustain::domain::models::FocusState::Input);
    let area = Rect::new(0, 0, 80, 15);
    let mut buffer = Buffer::empty(area);
    let lines = apply_card::render_apply_card_lines(&card, &state.theme, area.width);
    inline_card::render_bottom_anchored_decision_card(
        &mut buffer,
        lines,
        state.theme.colors.accent,
        area,
    );
    let rendered = (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    for needle in [
        "Producer: spoke-1",
        "Files: src/lib.rs, tests/apply.rs",
        "Workspace: /workspace",
        "predates apply records",
        "may already be in the working tree",
        "indeterminate",
        "18-3a-f",
        "[y] Apply",
        "[n] Cancel (Esc)",
    ] {
        assert!(rendered.contains(needle), "missing {needle:?}:\n{rendered}");
    }
    for binding in apply_card::APPLY_CARD_BINDINGS {
        assert_eq!(
            apply_card::choice_for_key(binding.key),
            Some(binding.choice)
        );
        assert!(
            rendered.contains(&format!("[{}] {}", binding.key, binding.label)),
            "{rendered}"
        );
    }
}

#[test]
fn capture_order_and_apply_lattice_render_honestly_at_wide_and_narrow_widths() {
    use rustain::adapters::tui::handlers::artifact_command::render_show;
    use rustain::adapters::tui::widgets::artifacts_panel::{
        artifact_row, id_prefix, visible_artifact_row,
    };

    let old = artifact('0');
    let never = artifact('1');
    let applied = artifact('2');
    let conflict = artifact('3');
    let failed = artifact('4');
    let unknown = artifact('5');
    let indeterminate = artifact('6');
    let phantom = ArtifactId::from(hash('f'));
    let mut events = vec![
        RoomEvent::ArtifactCreated {
            artifact: old.clone(),
        },
        RoomEvent::PatchCaptured {
            artifact: old.id.clone(),
            producer: old.producer.clone(),
        },
        RoomEvent::PatchApplyStarted {
            artifact: phantom.clone(),
            applier: None,
            workspace_revision: None,
        },
        RoomEvent::PatchApplyResolved {
            artifact: phantom,
            outcome: ApplyOutcome::Failed,
        },
    ];
    for patch in [
        &never,
        &applied,
        &conflict,
        &failed,
        &unknown,
        &indeterminate,
    ] {
        events.push(RoomEvent::ArtifactCreated {
            artifact: patch.clone(),
        });
        events.push(RoomEvent::PatchCaptured {
            artifact: patch.id.clone(),
            producer: patch.producer.clone(),
        });
        events.push(RoomEvent::PatchReviewed {
            artifact: patch.id.clone(),
            reviewer: AgentId::local_operator(),
            verdict: ReviewVerdict::Approved,
        });
    }
    for (patch, outcome) in [
        (&applied, ApplyOutcome::Applied),
        (&conflict, ApplyOutcome::Conflict),
        (&failed, ApplyOutcome::Failed),
        (&unknown, ApplyOutcome::Unknown),
    ] {
        events.push(RoomEvent::PatchApplyStarted {
            artifact: patch.id.clone(),
            applier: None,
            workspace_revision: None,
        });
        events.push(RoomEvent::PatchApplyResolved {
            artifact: patch.id.clone(),
            outcome,
        });
    }
    events.push(RoomEvent::PatchApplyStarted {
        artifact: indeterminate.id.clone(),
        applier: None,
        workspace_revision: None,
    });
    let replay = OrchestrationRoom::project(OrchestrationRoomId::default(), events.clone());
    let room = OrchestrationRoom::project(OrchestrationRoomId::default(), events);
    assert_eq!(
        room, replay,
        "the capture-order stamp is replay deterministic"
    );
    let policy = MergeBackPolicy::default();

    assert!(room.predates_apply_records().contains(&old.id));
    assert!(!room.predates_apply_records().contains(&never.id));
    let old_row = artifact_row(&old, &room, PermissionMode::Yolo, &policy);
    assert!(
        old_row
            .contains("this journal predates apply records; patch may already be in working tree"),
        "{old_row}"
    );

    for (patch, expected) in [
        (&never, "apply: never attempted"),
        (&applied, "apply: applied"),
        (&conflict, "apply: conflict"),
        (&failed, "apply: failed"),
        (&unknown, "apply: unknown outcome"),
        (&indeterminate, "apply: indeterminate"),
    ] {
        let wide = artifact_row(patch, &room, PermissionMode::Yolo, &policy);
        let show = render_show(
            patch,
            &room,
            PermissionMode::Yolo,
            &policy,
            Err("body not loaded".to_owned()),
        );
        assert!(wide.contains(expected), "{wide}");
        assert!(show.contains(expected), "{show}");
        // The apply-outcome grammar must survive the painted buffer at the
        // spec's 36/60/120 reference widths, not just one mid value. The row's
        // narrow fallback reorders reason-before-identifier, so the outcome
        // stays visible at 60/120. At the 36-column extreme the kind/hazard
        // grammar leaves no room for the full outcome, so assert a bounded
        // render there rather than the needle.
        for width in [60usize, 120] {
            let narrow = visible_artifact_row(patch, &room, PermissionMode::Yolo, &policy, width);
            assert!(narrow.contains(expected), "width {width}: {narrow}");
        }
        let tiny = visible_artifact_row(patch, &room, PermissionMode::Yolo, &policy, 36);
        assert!(!tiny.is_empty(), "width 36 must still render a row");
        let narrow = visible_artifact_row(patch, &room, PermissionMode::Yolo, &policy, 64);
        if let (Some(decision_at), Some(id_at)) =
            (narrow.find("applies"), narrow.find(&id_prefix(&patch.id)))
        {
            assert!(decision_at < id_at, "{narrow}");
        }
    }
}

#[test]
fn applier_is_additive_on_the_wire_and_pre_field_entries_remain_readable() {
    let fixture = std::fs::read_to_string(format!(
        "{}/tests/fixtures/18_3a_e/pre_applier_apply.jsonl",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("fixture");
    // The applier field is additive metadata: a pre-field journal (applier
    // absent → serde default None) and an equivalent attributed stream must
    // fold to the SAME apply state, or the schema change silently moved the
    // fold. Decode + project both, then compare — deserialising alone proves
    // nothing about the journal-to-room fold path.
    let mut pre_events: Vec<RoomEvent> = Vec::new();
    let mut post_events: Vec<RoomEvent> = Vec::new();
    for line in fixture.lines() {
        let entry: rustain::domain::models::JournalEntry =
            serde_json::from_str(line).expect("pre-applier entry decodes");
        if let rustain::domain::models::JournalRecord::Room(event) = entry.record {
            pre_events.push(event.clone());
            let attributed = match event {
                RoomEvent::PatchApplyStarted {
                    artifact,
                    workspace_revision,
                    ..
                } => RoomEvent::PatchApplyStarted {
                    artifact,
                    applier: Some(AgentId::local_operator()),
                    workspace_revision,
                },
                other => other,
            };
            post_events.push(attributed);
        }
    }
    let pre_room = OrchestrationRoom::project(
        rustain::domain::models::OrchestrationRoomId::default(),
        pre_events,
    );
    let post_room = OrchestrationRoom::project(
        rustain::domain::models::OrchestrationRoomId::default(),
        post_events,
    );
    assert_eq!(
        pre_room.apply_state(),
        post_room.apply_state(),
        "the additive applier field must not change the folded apply state"
    );

    let event = RoomEvent::PatchApplyStarted {
        artifact: ArtifactId::from(hash('a')),
        applier: Some(AgentId::local_operator()),
        workspace_revision: None,
    };
    let value = serde_json::to_value(&event).expect("serialize");
    assert_eq!(
        value.get("applier").and_then(serde_json::Value::as_str),
        Some(AgentId::local_operator().as_str())
    );
    let decoded: RoomEvent = serde_json::from_value(value).expect("round trip");
    assert!(matches!(
        &decoded,
        RoomEvent::PatchApplyStarted {
            applier: Some(agent),
            ..
        } if agent == &AgentId::local_operator()
    ));

    let unattributed = serde_json::to_value(RoomEvent::PatchApplyStarted {
        artifact: ArtifactId::from(hash('b')),
        applier: None,
        workspace_revision: None,
    })
    .expect("serialize unattributed");
    assert!(
        unattributed.get("applier").is_none(),
        "None must preserve the pre-field wire shape: {unattributed}"
    );
}

#[test]
fn operator_result_vocabulary_distinguishes_busy_indeterminate_conflict_and_failure() {
    use rustain::adapters::tui::handlers::artifact_command::render_apply_result;

    let id = ArtifactId::from(hash('d'));
    let cases = [
        (PatchApplyPortError::WorkspaceBusy, "workspace is busy"),
        (
            PatchApplyPortError::ApplyIndeterminate,
            "no resolution verb exists yet (18-3a-f)",
        ),
        (
            PatchApplyPortError::Conflict("does not apply".to_owned()),
            "conflicted and did not mutate",
        ),
        (
            PatchApplyPortError::Failed("journal write failed".to_owned()),
            "apply failed",
        ),
        (
            PatchApplyPortError::ApplyUnresolved("resolution append failed".to_owned()),
            "may have been applied",
        ),
    ];
    for (error, expected) in cases {
        let rendered = render_apply_result(&id, Err(error));
        assert!(rendered.contains(expected), "{rendered}");
    }
    assert!(
        render_apply_result(&id, Ok(())).contains("applied to the workspace"),
        "success is the only outcome that may claim the workspace changed"
    );
}

#[cfg(unix)]
fn run_git_cmd(path: &std::path::Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?} failed");
}

/// **AC3 keystone — the C3 resurrection mutant, end to end through the real
/// merge-back service.** A patch that creates a new file re-applies cleanly
/// onto a tree where the operator deleted the file (`git apply` exit=0 both
/// times — its atomicity does NOT contain new-file resurrection, which is why
/// `NeverAttempted` on a pre-record-era journal must render as *unknown*, not
/// *never*). A readable `Applied` outcome is retryable, so the second apply is
/// not refused, the file is resurrected, and the durable record says `Applied`
/// — honestly, not `NeverAttempted`.
#[tokio::test]
#[cfg(unix)]
async fn a_new_file_patch_resurrects_on_reapply_and_records_honestly() {
    use rustain::adapters::artifact::FileSystemArtifactStore;
    use rustain::adapters::merge_back::GitPatchApplier;
    use rustain::domain::models::{
        ApplyOutcome, ApplyState, CapabilityTokenId, HostBinding, OwnershipKind,
        PermissionMode, ProvenanceTag, ProvisioningTier, UnifiedDiff,
    };
    use rustain::domain::ports::{ArtifactStore, PatchApplier};
    use rustain::domain::services::patch_review::MergeBackPolicy;
    use rustain::infrastructure::orchestrator::PatchMergeBack;
    use rustain::infrastructure::runtime::event_bus::EventBus;
    use rustain::infrastructure::subagent::NodeJournal;

    let policy = MergeBackPolicy {
        auto_approve_user_originated: true,
    };
    let workspace = tempfile::tempdir().expect("tempdir");
    let root = workspace.path();
    run_git_cmd(root, &["init", "-q"]);
    let journal = std::sync::Arc::new(NodeJournal::open_workspace(root).await.expect("journal"));
    let store: std::sync::Arc<dyn ArtifactStore> =
        std::sync::Arc::new(FileSystemArtifactStore::new(root));
    let (bus, rx) = EventBus::new(64);
    std::mem::forget(rx);
    let applier: std::sync::Arc<dyn PatchApplier> = std::sync::Arc::new(GitPatchApplier);
    let service = PatchMergeBack::new(
        root.to_path_buf(),
        store,
        journal.clone(),
        std::sync::Arc::new(bus),
        applier,
    );
    let artifact = service
        .capture(
            AgentId::new(),
            CapabilityTokenId::root(),
            vec![ProvenanceTag::UserOriginated],
            vec![],
            HostBinding::new("host-apply", "ws"),
            &UnifiedDiff::new(
                ProvisioningTier::ScratchCopy,
                "diff --git a/rising.txt b/rising.txt\nnew file mode 100644\n--- /dev/null\n+++ b/rising.txt\n@@ -0,0 +1 @@\n+risen\n".to_owned(),
            ),
        )
        .await
        .expect("capture");

    service
        .apply(&artifact, OwnershipKind::Owned, PermissionMode::Yolo, &policy, None)
        .await
        .expect("first apply creates the file");
    assert_eq!(
        std::fs::read_to_string(root.join("rising.txt")).expect("file created"),
        "risen\n"
    );

    // The operator deletes the file by hand, then re-applies.
    std::fs::remove_file(root.join("rising.txt")).expect("operator deletes");
    service
        .apply(&artifact, OwnershipKind::Owned, PermissionMode::Yolo, &policy, None)
        .await
        .expect("re-apply resurrects the file (git apply exit=0 again)");
    assert_eq!(
        std::fs::read_to_string(root.join("rising.txt")).expect("file resurrected"),
        "risen\n"
    );

    // Honest record: last-write-wins `Applied`, never `NeverAttempted`.
    let room = journal.project_room("host-apply").await.expect("project room");
    assert_eq!(
        room.apply_state().get(&artifact.id).copied(),
        Some(ApplyState::Resolved(ApplyOutcome::Applied)),
        "the resurrected re-apply must record Applied, honestly"
    );
}
