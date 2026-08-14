//! Story 18.3a-f — the operator resolution verb for an indeterminate apply.
//!
//! Cut 1 (`18.3a-d`) bracketed every `git apply` in a durable write-ahead
//! record and shipped a latch: a `PatchApplyStarted` with no matching
//! `PatchApplyResolved` folds to `ApplyState::Indeterminate`, and
//! `PatchMergeBack::apply` refuses it forever. Cut 2 (`18.3a-e`) built the
//! operator door that can reach that latch. This cut ships the **release** —
//! not by weakening the refusal, but by letting the operator put a new durable
//! fact into the journal: *I inspected the working tree, and here is what I
//! found.*
//!
//! ⛔ **No `a2a` and no `transparency` in this filename.**
//! `every_a2a_integration_test_is_wired_into_the_ci_a2a_lane`
//! (`src/domain/ports/capability_provider.rs`) fails the **default** lane for
//! an unlisted `tests/*a2a*.rs`.
//!
//! Class C throughout: a real git workspace, a real `NodeJournal`, a real
//! `WorkspaceJournalReader`, a real `flock`, real `ratatui` buffers, and a
//! `PatchApplier` double that **decorates** the production `GitPatchApplier`
//! rather than replacing it.

#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use rustain::adapters::artifact::FileSystemArtifactStore;
use rustain::adapters::merge_back::GitPatchApplier;
use rustain::domain::models::node_journal::JournalRecord;
use rustain::domain::models::{
    AgentId, ApplyOutcome, ApplyState, ArtifactId, ArtifactRef, CapabilityTokenId, ContentHash,
    HostBinding, OperatorApplyFinding, OrchestrationRoom, OrchestrationRoomId, OwnershipKind,
    PermissionMode, ProvenanceTag, ProvisioningTier, RoomEvent, UnifiedDiff,
};
use rustain::domain::ports::{ArtifactStore, PatchApplier, PatchApplyError, RoomJournalReader};
use rustain::domain::services::patch_review::MergeBackPolicy;
use rustain::infrastructure::orchestrator::{MergeBackError, PatchMergeBack};
use rustain::infrastructure::runtime::event_bus::EventBus;
use rustain::infrastructure::subagent::{NodeJournal, WorkspaceJournalReader};
use tokio::sync::Notify;

const HOST: &str = "host-resolve";

fn source(relative: &str) -> String {
    std::fs::read_to_string(format!("{}/{relative}", env!("CARGO_MANIFEST_DIR")))
        .unwrap_or_else(|error| panic!("read {relative}: {error}"))
}

fn hash(byte: char) -> ContentHash {
    ContentHash::parse_hex(&byte.to_string().repeat(64)).expect("hex")
}

/// A patch handle for painted-card assertions. ⚠ Only the id, producer and
/// kind reach the widget; nothing here is journal-derived.
fn card_artifact() -> ArtifactRef {
    rustain::domain::models::EvidenceArtifact {
        id: ArtifactId::from(hash('e')),
        kind: rustain::domain::models::ArtifactKind::Patch,
        producer: AgentId::parse("spoke-1").expect("agent"),
        content_hash: hash('0'),
        authority: CapabilityTokenId::default(),
        provenance: vec![ProvenanceTag::UserOriginated],
        depends_on: Vec::new(),
        review: Some(rustain::domain::models::ReviewStatus::Pending),
        host: HostBinding::new("host-A", "workspace"),
    }
}

fn run_git(path: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?} failed");
}

/// A real git repository with one committed `file.txt` containing `old\n`.
fn init_repo(workspace: &Path) {
    run_git(workspace, &["init", "-q"]);
    std::fs::write(workspace.join("file.txt"), "old\n").expect("seed file");
    run_git(workspace, &["add", "file.txt"]);
    run_git(
        workspace,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "baseline",
        ],
    );
}

fn patch(old: &str, new: &str) -> UnifiedDiff {
    UnifiedDiff::new(
        ProvisioningTier::ScratchCopy,
        format!(
            "diff --git a/file.txt b/file.txt\n--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-{old}\n+{new}\n"
        ),
    )
}

/// The shipped `/fanout` merge-back configuration (`startup.rs` hardcodes
/// `auto_approve_user_originated: true`).
fn shipped_policy() -> MergeBackPolicy {
    MergeBackPolicy {
        auto_approve_user_originated: true,
    }
}

fn bus() -> Arc<EventBus> {
    let (bus, rx) = EventBus::new(64);
    std::mem::forget(rx);
    Arc::new(bus)
}

/// Cut 1's decorator, carried forward: mutate the tree for real, announce it,
/// then never return. The driving test aborts the task, so `PatchApplyResolved`
/// is never appended — deterministically, with no timing window and no
/// `kill -9`. ⛔ Never a stub: a stub returning `Ok(())` would kill the positive
/// control that the working tree actually changed.
struct CrashingApplier {
    inner: GitPatchApplier,
    calls: Arc<AtomicUsize>,
    hang_after_mutating: bool,
    entered: Arc<Notify>,
}

impl CrashingApplier {
    fn new(hang_after_mutating: bool) -> Self {
        Self {
            inner: GitPatchApplier,
            calls: Arc::new(AtomicUsize::new(0)),
            hang_after_mutating,
            entered: Arc::new(Notify::new()),
        }
    }
}

#[async_trait]
impl PatchApplier for CrashingApplier {
    async fn apply(&self, workspace: &Path, body: &[u8]) -> Result<(), PatchApplyError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let outcome = self.inner.apply(workspace, body).await;
        if self.hang_after_mutating {
            self.entered.notify_one();
            std::future::pending::<()>().await;
        }
        outcome
    }

    async fn revision(&self, workspace: &Path) -> Option<String> {
        self.inner.revision(workspace).await
    }
}

struct Harness {
    workspace: tempfile::TempDir,
    journal: Arc<NodeJournal>,
    store: Arc<dyn ArtifactStore>,
    producer: AgentId,
}

impl Harness {
    async fn new() -> Self {
        let workspace = tempfile::tempdir().expect("tempdir");
        init_repo(workspace.path());
        let journal = Arc::new(
            NodeJournal::open_workspace(workspace.path())
                .await
                .expect("journal opens"),
        );
        let store: Arc<dyn ArtifactStore> =
            Arc::new(FileSystemArtifactStore::new(workspace.path()));
        Self {
            workspace,
            journal,
            store,
            producer: AgentId::new(),
        }
    }

    fn path(&self) -> &Path {
        self.workspace.path()
    }

    fn service(&self, applier: Arc<dyn PatchApplier>) -> PatchMergeBack {
        PatchMergeBack::new(
            self.path().to_path_buf(),
            self.store.clone(),
            self.journal.clone(),
            bus(),
            applier,
        )
    }

    async fn capture(&self, service: &PatchMergeBack, old: &str, new: &str) -> ArtifactRef {
        service
            .capture(
                self.producer.clone(),
                CapabilityTokenId::root(),
                vec![ProvenanceTag::UserOriginated],
                vec![],
                HostBinding::new(HOST, "ws"),
                &patch(old, new),
            )
            .await
            .expect("capture")
    }

    fn tree(&self) -> String {
        std::fs::read_to_string(self.path().join("file.txt")).expect("read tree")
    }

    async fn state(&self, artifact: &ArtifactRef) -> ApplyState {
        self.journal
            .project_room(HOST)
            .await
            .expect("project room")
            .apply_state()
            .get(&artifact.id)
            .copied()
            .unwrap_or_default()
    }

    async fn journal_lines(&self) -> usize {
        self.journal.load().await.expect("journal loads").len()
    }

    /// Drive a REAL apply to the point where the process dies inside the apply
    /// window, leaving a genuine `Indeterminate` — never a hand-built one.
    async fn wedge(&self, artifact: &ArtifactRef) {
        let applier = Arc::new(CrashingApplier::new(true));
        let entered = applier.entered.clone();
        let service = Arc::new(self.service(applier));
        let held = {
            let service = service.clone();
            let artifact = artifact.clone();
            tokio::spawn(async move {
                service
                    .apply(
                        &artifact,
                        OwnershipKind::Owned,
                        PermissionMode::Yolo,
                        &shipped_policy(),
                        None,
                    )
                    .await
            })
        };
        entered.notified().await;
        held.abort();
        let _ = held.await;
        assert_eq!(
            self.state(artifact).await,
            ApplyState::Indeterminate,
            "the harness must produce a REAL wedge, not a fabricated state"
        );
    }
}

// ═══════════════════════════════ AC1 ═══════════════════════════════

/// **AC1 keystone (a) — behavioural, and AC1's positive control in one test.**
///
/// Front door: `OrchestrationRoom::project` → `room.apply_state()` →
/// `apply_is_refused`, reached through the production `PatchMergeBack::apply`.
///
/// ⛔ **Forbidden bypass:** constructing `ApplyState::OperatorResolved` by hand.
/// The state here is produced only by folding the event that produces it, off a
/// real journal file on disk.
///
/// **Positive control** (without it, a build where everything still refuses
/// passes the first and third mutants vacuously): after the resolution the
/// artifact really applies and the working tree really changes.
#[tokio::test]
async fn an_operator_resolution_releases_the_latch_and_the_next_apply_really_runs() {
    let harness = Harness::new().await;
    let applier = Arc::new(CrashingApplier::new(false));
    let service = harness.service(applier.clone());
    let artifact = harness.capture(&service, "old", "new").await;

    harness.wedge(&artifact).await;
    // The tree DID change — that is why the honest answer is "unknown", and why
    // the operator has something real to inspect.
    assert_eq!(harness.tree(), "new\n");

    // Third mutant: make `OperatorResolved(Present)` refuse and the artifact is
    // still wedged here.
    let refused = service
        .apply(
            &artifact,
            OwnershipKind::Owned,
            PermissionMode::Yolo,
            &shipped_policy(),
            None,
        )
        .await
        .expect_err("a wedged artifact is refused before the release");
    assert!(matches!(refused, MergeBackError::ApplyIndeterminate));

    // The operator inspects the tree themselves and reports what they found.
    // Reset it first so the follow-up apply is a real mutation, not a no-op.
    std::fs::write(harness.path().join("file.txt"), "old\n").expect("operator reverts by hand");
    service
        .record_inspection(
            &artifact,
            OperatorApplyFinding::Absent,
            AgentId::local_operator(),
        )
        .await
        .expect("a genuinely wedged artifact resolves");

    assert_eq!(
        harness.state(&artifact).await,
        ApplyState::OperatorResolved(OperatorApplyFinding::Absent),
        "the fold must produce the operator lattice state, never Resolved(_)"
    );

    service
        .apply(
            &artifact,
            OwnershipKind::Owned,
            PermissionMode::Yolo,
            &shipped_policy(),
            None,
        )
        .await
        .expect("the released artifact applies");
    assert_eq!(
        harness.tree(),
        "new\n",
        "positive control: the release is worthless unless the retry really writes"
    );
    assert_eq!(
        harness.state(&artifact).await,
        ApplyState::Resolved(ApplyOutcome::Applied),
        "a real machine outcome supersedes the human report, as last-write-wins"
    );
}

/// **AC1 first mutant — delete the `OperatorResolved(Unknown)` arm from
/// `apply_is_refused` and this shows the door PERMITTING the apply.**
///
/// A newer build recorded a finding value this one cannot read. ⛔ Never read as
/// resolved (`DF-18-3a-f-UNREADABLE-VALUE-LATCH`).
#[tokio::test]
async fn an_unreadable_operator_finding_from_a_newer_build_stays_refused_at_the_door() {
    let harness = Harness::new().await;
    let applier = Arc::new(CrashingApplier::new(false));
    let service = harness.service(applier);
    let artifact = harness.capture(&service, "old", "new").await;
    harness.wedge(&artifact).await;

    // A record written by a newer build: the finding string is unknown here, so
    // serde degrades it to `Unknown`. Appended as raw JSON through the real
    // journal writer so the degradation happens on the real decode path.
    let line = format!(
        r#"{{"event":"patch_apply_inspected","artifact":"{}","finding":"present_but_reverted","inspector":"operator"}}"#,
        artifact.id.as_str()
    );
    let event: RoomEvent = serde_json::from_str(&line).expect("a newer finding still decodes");
    assert!(
        matches!(
            event,
            RoomEvent::PatchApplyInspected {
                finding: OperatorApplyFinding::Unknown,
                ..
            }
        ),
        "an unknown finding must degrade, not fail: {event:?}"
    );
    harness
        .journal
        .append_room(event)
        .await
        .expect("append the newer build's record");

    assert_eq!(
        harness.state(&artifact).await,
        ApplyState::OperatorResolved(OperatorApplyFinding::Unknown)
    );
    let refused = service
        .apply(
            &artifact,
            OwnershipKind::Owned,
            PermissionMode::Yolo,
            &shipped_policy(),
            None,
        )
        .await
        .expect_err("an unreadable finding is not a resolution");
    assert!(
        matches!(refused, MergeBackError::ApplyIndeterminate),
        "fail closed on a value this build cannot read: {refused:?}"
    );

    // ⛔ And the verb does not clear it either: the card cannot name what it
    // would be asking the operator to supersede (ruling A4 as rewritten by P1).
    let not_wedged = service
        .record_inspection(
            &artifact,
            OperatorApplyFinding::Absent,
            AgentId::local_operator(),
        )
        .await
        .expect_err("only Indeterminate is releasable");
    assert!(matches!(not_wedged, MergeBackError::ApplyNotWedged { .. }));
}

/// **Review finding F2 — the verb refuses an `Unknown` finding even on a wedged
/// artifact (ruling A4 / DF-18-3a-f-UNREADABLE-VALUE-LATCH).**
///
/// `Unknown` is deserialize-only forward-compat; the parser never produces it,
/// so this drives the seam directly. A genuinely wedged (Indeterminate) artifact
/// plus an `Unknown` finding is refused at the door — never persisted — so the
/// verb cannot mint an `OperatorResolved(Unknown)` this build would then refuse
/// to release.
#[tokio::test]
async fn the_release_verb_refuses_an_unknown_finding_even_on_a_wedged_artifact() {
    let harness = Harness::new().await;
    let applier = Arc::new(CrashingApplier::new(false));
    let service = harness.service(applier);
    let artifact = harness.capture(&service, "old", "new").await;
    harness.wedge(&artifact).await;
    assert_eq!(
        harness.state(&artifact).await,
        ApplyState::Indeterminate,
        "precondition: the artifact is genuinely wedged"
    );

    let refused = service
        .record_inspection(
            &artifact,
            OperatorApplyFinding::Unknown,
            AgentId::local_operator(),
        )
        .await
        .expect_err("an Unknown finding is not a report an operator can file");
    assert!(
        matches!(refused, MergeBackError::UnreadableFinding),
        "the verb refuses to mint an unreadable finding: {refused:?}"
    );
    // The refusal persisted nothing — the artifact is still wedged.
    assert_eq!(
        harness.state(&artifact).await,
        ApplyState::Indeterminate,
        "the refusal wrote nothing; the latch is unchanged"
    );
}

/// **AC1 — the third lattice consumer's decision, asserted (ruling A1).**
///
/// `auto_applies_refusal` was a `matches!` equality against
/// `Resolved(Applied)`, so a new variant read silently as *"no completed apply
/// is recorded"*. An operator-reported `Present` is neither that nor
/// *"already ran"*: no machine outcome exists, but a human did look. AC1
/// requires the decision to be **written down** — here it is, behaviourally.
///
/// ⚑ Found by the human smoke: driving `/artifact apply` on a policy
/// auto-apply patch right after resolving it is the one production path that
/// reaches this arm, and nothing was asserting it.
#[tokio::test]
async fn the_auto_apply_door_distinguishes_a_machine_outcome_from_an_operator_report() {
    use rustain::domain::models::{ArtifactKind, EvidenceArtifact, ReviewStatus};
    use rustain::infrastructure::runtime::artifact_bridge::apply_artifact;

    // `AutoApplies`: user-originated under the shipped policy, so the operator
    // door always refuses — the question is only WHAT it says.
    let handle = EvidenceArtifact {
        id: ArtifactId::from(hash('7')),
        kind: ArtifactKind::Patch,
        producer: AgentId::parse("spoke-1").expect("agent"),
        content_hash: hash('0'),
        authority: CapabilityTokenId::default(),
        provenance: vec![ProvenanceTag::UserOriginated],
        depends_on: Vec::new(),
        review: Some(ReviewStatus::Pending),
        host: HostBinding::new("host-A", "workspace"),
    };
    let base = vec![
        RoomEvent::ArtifactCreated {
            artifact: handle.clone(),
        },
        RoomEvent::PatchCaptured {
            artifact: handle.id.clone(),
            producer: handle.producer.clone(),
        },
    ];

    struct NeverCalled;
    #[async_trait]
    impl rustain::domain::ports::PatchApplyExecutor for NeverCalled {
        async fn apply_patch(
            &self,
            _artifact: ArtifactRef,
            _ownership: OwnershipKind,
            _permission_mode: PermissionMode,
            _policy: MergeBackPolicy,
            _applier: Option<AgentId>,
        ) -> Result<(), rustain::domain::ports::PatchApplyPortError> {
            panic!("⛔ an AutoApplies patch must be refused BEFORE the port");
        }
    }

    for (tail, expected) in [
        (
            vec![RoomEvent::PatchApplyResolved {
                artifact: handle.id.clone(),
                outcome: ApplyOutcome::Applied,
            }],
            "already ran at fan-out completion",
        ),
        (
            vec![
                RoomEvent::PatchApplyStarted {
                    artifact: handle.id.clone(),
                    applier: None,
                    workspace_revision: None,
                },
                RoomEvent::PatchApplyInspected {
                    artifact: handle.id.clone(),
                    finding: OperatorApplyFinding::Present,
                    inspector: AgentId::local_operator(),
                },
            ],
            "an operator reported its changes are already in the working tree",
        ),
        (
            vec![
                RoomEvent::PatchApplyStarted {
                    artifact: handle.id.clone(),
                    applier: None,
                    workspace_revision: None,
                },
                RoomEvent::PatchApplyInspected {
                    artifact: handle.id.clone(),
                    finding: OperatorApplyFinding::Absent,
                    inspector: AgentId::local_operator(),
                },
            ],
            "no completed apply is recorded yet",
        ),
        (Vec::new(), "no completed apply is recorded yet"),
    ] {
        let mut events = base.clone();
        events.extend(tail);
        let room = OrchestrationRoom::project(OrchestrationRoomId::default(), events);
        let refusal = apply_artifact(
            &room,
            &NeverCalled,
            &AgentId::local_operator(),
            handle.id.as_str(),
            PermissionMode::Yolo,
            shipped_policy(),
        )
        .await
        .expect_err("an AutoApplies patch is refused at the operator door");
        assert!(refusal.contains(expected), "{refusal}");
        // ⛔ Only a recorded machine outcome may say the apply "already ran".
        if expected != "already ran at fan-out completion" {
            assert!(!refusal.contains("already ran"), "{refusal}");
        }
    }
}

/// **AC1 second mutant — fold `PatchApplyInspected` to `Resolved(Applied)` and
/// a human's report renders as `apply: applied`, indistinguishable from a real
/// `git apply` success (ruling A8).**
#[test]
fn an_operator_report_never_renders_as_a_machine_outcome() {
    use rustain::adapters::tui::widgets::artifacts_panel::apply_state_suffix;

    let artifact = ArtifactId::from(hash('a'));
    for (finding, expected) in [
        (OperatorApplyFinding::Present, "reported present"),
        (OperatorApplyFinding::Absent, "reported absent"),
        (OperatorApplyFinding::Unknown, "report unreadable"),
    ] {
        let room = OrchestrationRoom::project(
            OrchestrationRoomId::default(),
            vec![
                RoomEvent::PatchApplyStarted {
                    artifact: artifact.clone(),
                    applier: None,
                    workspace_revision: None,
                },
                RoomEvent::PatchApplyInspected {
                    artifact: artifact.clone(),
                    finding,
                    inspector: AgentId::local_operator(),
                },
            ],
        );
        let suffix = apply_state_suffix(&room, &artifact);
        assert!(suffix.contains(expected), "{suffix}");
        assert!(
            suffix.contains("resolved by operator"),
            "the row must attribute the report to a human: {suffix}"
        );
        assert_ne!(
            suffix, "apply: applied",
            "two epistemic classes, one phrase"
        );
        // Ruling P2 — a completion, never an invitation. The gate permits a
        // retry; the row must not advertise one.
        let lowered = suffix.to_ascii_lowercase();
        for forbidden in ["retry", "ready to apply", "try again"] {
            assert!(!lowered.contains(forbidden), "{suffix}");
        }
        // Ruling P6 — compaction can silently re-wedge this artifact, so no
        // copy may imply the release survives routine maintenance.
        for forbidden in ["permanent", "final", "durable"] {
            assert!(!lowered.contains(forbidden), "{suffix}");
        }
    }
}

/// **AC1 keystone (b) — structural (Rule 4).**
///
/// The lattice has exactly three production consumers and TWO of them read it
/// through a construct the compiler will not check. This pins the set so a
/// fourth cannot appear un-audited, and pins that the refusal classifier names
/// every state that must fail closed.
///
/// ⚠ Counted per file on `.apply_state()`, ⛔ **never** on `room.apply_state()`
/// and ⛔ never repo-wide on `ApplyState::`. The bare `ApplyState::` token
/// appears 19 times in doc comments, `insert` calls and match arms, so a global
/// count is un-writeable — and a receiver-coupled needle silently reads 0 the
/// moment rustfmt breaks the chain onto its own line, which is exactly how the
/// `self.persist(` ratchet in `conformance_18_3a_d_apply.rs` was made to
/// disagree with `cargo fmt`. The accessor's own definition
/// (`pub fn apply_state(&self)`) carries no leading dot, so the defining module
/// is correctly absent from this map.
#[test]
fn every_lattice_consumer_is_audited_and_no_fourth_one_appeared() {
    let mut sources = Vec::new();
    collect_sources(
        &std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut sources,
    );
    assert!(
        sources.len() > 100,
        "positive control: the walk actually found the source tree"
    );

    let expected: std::collections::BTreeMap<&str, usize> = [
        ("src/adapters/tui/widgets/artifacts_panel.rs", 1usize),
        // 🔴 TWO in the guard file, both audited, both under the two locks: the
        // apply door's refusal read and the release verb's wedged-only read.
        // The AC's suggested "exactly one per file" predates the release verb,
        // which `ADR-18-3a-d-01` D4 REQUIRES to re-read the projection after
        // acquiring. The invariant that matters — no fourth *file* consumes the
        // lattice un-audited — is what this map enforces.
        ("src/infrastructure/orchestrator/merge_back.rs", 2),
        ("src/infrastructure/runtime/artifact_bridge.rs", 1),
    ]
    .into_iter()
    .collect();
    let mut found: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for (path, body) in &sources {
        let count = body.matches(".apply_state()").count();
        if count > 0 {
            found.insert(path.clone(), count);
        }
    }
    let found_refs: std::collections::BTreeMap<&str, usize> = found
        .iter()
        .map(|(path, count)| (path.as_str(), *count))
        .collect();
    assert_eq!(
        found_refs, expected,
        "a fourth consumer of the apply lattice appeared un-audited, or one moved"
    );

    // Each audited consumer makes a deliberate decision about the new state.
    for path in expected.keys() {
        let body = source(path);
        assert!(
            body.contains("OperatorResolved"),
            "{path} reads the lattice but never names the operator state"
        );
    }

    // The refusal classifier names EVERY shape that must fail closed.
    let merge_back = source("src/infrastructure/orchestrator/merge_back.rs");
    let classifier = &merge_back[merge_back
        .find("fn apply_is_refused(")
        .expect("positive control: the classifier exists")..];
    let classifier = &classifier[..classifier.find("\n}\n").expect("it closes")];
    for shape in [
        "ApplyState::Indeterminate",
        "ApplyState::Resolved(ApplyOutcome::Unknown)",
        "ApplyState::OperatorResolved(OperatorApplyFinding::Unknown)",
    ] {
        assert!(
            classifier.contains(shape),
            "the door must fail closed on {shape}:\n{classifier}"
        );
    }
    // ⛔ The false atomicity inference must stay deleted (it was demonstrated
    // false on 2026-08-08 by a new-file hunk re-applying with `exit=0`).
    assert!(
        !merge_back.contains("atomic across hunks, so the working tree"),
        "the falsified `atomicity bounds a re-apply` claim must not return"
    );
}

fn collect_sources(dir: &Path, out: &mut Vec<(String, String)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            let relative = path
                .strip_prefix(env!("CARGO_MANIFEST_DIR"))
                .expect("inside the crate")
                .to_string_lossy()
                .into_owned();
            out.push((
                relative,
                std::fs::read_to_string(&path).expect("read source"),
            ));
        }
    }
}

// ═══════════════════════════════ AC2 ═══════════════════════════════

/// Body of `PatchMergeBack::record_inspection`, bounded at the next method.
///
/// ⚠ Trap 7's slicer shape: this bounds on `"\n    }\n"`, so the function must
/// contain no nested block that closes at four-space indentation. Keep it flat.
fn record_inspection_body(merge_back: &str) -> &str {
    let start = merge_back
        .find("pub async fn record_inspection(")
        .expect("positive control: the resolution seam exists");
    let rest = &merge_back[start..];
    &rest[..rest.find("\n    }\n").expect("the function closes")]
}

/// **AC2 first mutant — drop the wedged-only gate and an operator's report
/// OVERRIDES a real machine outcome (ruling A5).**
///
/// The fold is last-write-wins, so without this refusal a `Resolved(Applied)`
/// becomes `OperatorResolved(Absent)` on one keystroke: fail-open through the
/// side door, and the single most likely way this capability ships a defect.
///
/// Also the AC2 positive control: a genuinely `Indeterminate` artifact resolves
/// and the journal gains **exactly one** new line. Without it a build that
/// refuses everything passes every mutant here.
#[tokio::test]
async fn only_a_genuinely_wedged_artifact_can_be_resolved() {
    let harness = Harness::new().await;
    let applier = Arc::new(CrashingApplier::new(false));
    let service = harness.service(applier);

    // 1. A completed apply is NOT resolvable — its outcome is on record.
    let applied = harness.capture(&service, "old", "new").await;
    service
        .apply(
            &applied,
            OwnershipKind::Owned,
            PermissionMode::Yolo,
            &shipped_policy(),
            None,
        )
        .await
        .expect("apply");
    assert_eq!(
        harness.state(&applied).await,
        ApplyState::Resolved(ApplyOutcome::Applied)
    );
    let before = harness.journal_lines().await;
    let refusal = service
        .record_inspection(
            &applied,
            OperatorApplyFinding::Absent,
            AgentId::local_operator(),
        )
        .await
        .expect_err("a recorded machine outcome must not be overwritable by a report");
    assert!(
        matches!(&refusal, MergeBackError::ApplyNotWedged(state) if state.contains("Applied")),
        "the refusal must name the state it found: {refusal:?}"
    );
    assert_eq!(
        harness.state(&applied).await,
        ApplyState::Resolved(ApplyOutcome::Applied),
        "the machine outcome survives the refused report"
    );
    assert_eq!(
        harness.journal_lines().await,
        before,
        "a refused resolution appends nothing"
    );

    // 2. `NeverAttempted` is refused too — nothing is wedged.
    let untouched = harness.capture(&service, "new", "newer").await;
    let refusal = service
        .record_inspection(
            &untouched,
            OperatorApplyFinding::Present,
            AgentId::local_operator(),
        )
        .await
        .expect_err("nothing to resolve on a patch that was never applied");
    assert!(
        matches!(&refusal, MergeBackError::ApplyNotWedged(state) if state.contains("NeverAttempted")),
        "{refusal:?}"
    );

    // 3. Positive control: the real wedge resolves, and costs exactly one line.
    harness.wedge(&untouched).await;
    let before = harness.journal_lines().await;
    service
        .record_inspection(
            &untouched,
            OperatorApplyFinding::Present,
            AgentId::local_operator(),
        )
        .await
        .expect("a genuinely wedged artifact resolves");
    assert_eq!(
        harness.journal_lines().await,
        before + 1,
        "one report, one durable line"
    );
    assert_eq!(
        harness.state(&untouched).await,
        ApplyState::OperatorResolved(OperatorApplyFinding::Present)
    );
}

/// **AC2 second and third mutants — read the projection BEFORE taking the lock,
/// or release and re-acquire around the check, and two callers both pass the
/// gate.**
///
/// 🔴 **Why a same-process test proves a cross-process claim.** POSIX `flock`
/// locks are held on the **open file description**, not the process: two
/// separate `open()` calls create two descriptions, so the second
/// `LOCK_EX | LOCK_NB` fails with `EWOULDBLOCK` even inside one process — the
/// same outcome a second OS process sees. (The POSIX inference cut 1 recorded;
/// ⛔ it does not hold for `fcntl` record locks, and ⛔ never substitute `fs2`
/// or `fd-lock`.)
///
/// ⛔ **Forbidden bypass:** asserting the lock **file exists**. Existence is not
/// exclusion; the loser must be refused.
#[tokio::test]
async fn a_resolution_contends_for_the_same_workspace_lock_as_an_apply() {
    let harness = Harness::new().await;
    let holder_applier = Arc::new(CrashingApplier::new(true));
    let entered = holder_applier.entered.clone();
    let holder = Arc::new(harness.service(holder_applier));
    let first = harness.capture(&holder, "old", "first").await;
    let second = harness.capture(&holder, "old", "second").await;
    harness.wedge(&second).await;

    let held = {
        let holder = holder.clone();
        let first = first.clone();
        tokio::spawn(async move {
            holder
                .apply(
                    &first,
                    OwnershipKind::Owned,
                    PermissionMode::Yolo,
                    &shipped_policy(),
                    None,
                )
                .await
        })
    };
    entered.notified().await;

    // A second service over the same workspace: a genuinely wedged artifact,
    // so ONLY the lock can refuse it. ⛔ A mutant that reads the projection
    // before acquiring, or releases around the check, lets this through.
    let loser = harness.service(Arc::new(CrashingApplier::new(false)));
    let refusal = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        loser.record_inspection(
            &second,
            OperatorApplyFinding::Present,
            AgentId::local_operator(),
        ),
    )
    .await
    .expect("⛔ a mutant that HANGS is not RED — the timeout is a FAILURE")
    .expect_err("the release verb must contend for the workspace lock");
    assert!(
        matches!(refusal, MergeBackError::WorkspaceBusy),
        "the loser gets the named busy error, never Ok: {refusal:?}"
    );
    assert_eq!(
        harness.state(&second).await,
        ApplyState::Indeterminate,
        "a lock-refused resolution leaves no partial durable record"
    );

    held.abort();
    let _ = held.await;
}

/// **AC2 keystone (b) — structural source-order ratchet (Rule 4).**
///
/// Exactly-once says nothing about *before the gate*. Mirrors
/// `the_workspace_lock_is_acquired_before_every_gate_in_source`
/// (`conformance_18_3a_d_apply.rs`) for the release verb.
#[test]
fn the_release_verb_locks_then_reprojects_then_gates_then_appends() {
    let merge_back = source("src/infrastructure/orchestrator/merge_back.rs");
    let body = record_inspection_body(&merge_back);
    let acquire = body
        .find("self.acquire_workspace_lock()")
        .expect("positive control: the lock acquisition exists");
    let projection = body
        .find("self.journal.project_room(")
        .expect("positive control: the projection read exists");
    let gate = body
        .find("ApplyState::Indeterminate")
        .expect("positive control: the wedged-only gate exists");
    let append = body
        .find("self.persist(")
        .expect("positive control: the durable append exists");

    assert!(
        acquire < projection,
        "the projection must be re-read AFTER the lock is held: a peer may have \
         resolved this artifact while this call was contending"
    );
    assert!(acquire < gate, "the wedged-only gate runs under the lock");
    assert!(gate < append, "nothing is appended before the gate passes");

    // ⛔ Never release and re-acquire around the checks
    // (`ADR-18-3a-d-01` D4), and ⛔ never add a second acquisition site.
    assert_eq!(
        body.matches("self.acquire_workspace_lock()").count(),
        1,
        "the release verb reaches for the lock exactly once"
    );
    assert_eq!(
        merge_back.matches("ApplyLock::try_acquire(").count(),
        1,
        "one acquisition site in the whole file, so the counter cannot be bypassed"
    );

    // 🔴 **Both locks must be HELD across the gate and the append, not merely
    // TAKEN before them** (Rule 4 — a deterministic structural ratchet, ⛔ never
    // a timing window).
    //
    // This closes a real evidence gap the mutation campaign found: the mutant
    // `let _ = self.acquire_workspace_lock().await?;` keeps the acquisition,
    // keeps the source order, and keeps the contention keystone GREEN — because
    // that keystone proves the resolver *contends* for the lock, not that it
    // *holds* it. Rust expresses the difference in the binding name alone:
    // `let _ =` drops the guard at the semicolon, `let _name =` holds it to the
    // end of scope. There is no behavioural test for it — correct code
    // structurally prevents the interleave, and forcing the mutant's race would
    // need an artificial delay, which is evidence the test can be tricked, not
    // evidence the invariant holds.
    for (binding, what) in [
        (
            "let _workspace_lock = self.acquire_workspace_lock().await?;",
            "the cross-process workspace lock",
        ),
        (
            "let _guard = self.apply_guard.lock().await;",
            "the in-process apply guard",
        ),
    ] {
        assert!(
            body.contains(binding),
            "{what} must be bound to a NAMED guard that lives to the end of the \
             function; `let _ = …` drops it at the semicolon:\n{body}"
        );
    }
    assert!(
        !body.contains("let _ = self.acquire_workspace_lock()")
            && !body.contains("let _ = self.apply_guard.lock()"),
        "⛔ a wildcard binding releases the guard immediately:\n{body}"
    );
    // The same invariant on the apply path, so the two verbs cannot drift.
    let apply_body = &merge_back[merge_back
        .find("pub async fn apply(")
        .expect("positive control: the apply seam exists")..];
    let apply_body = &apply_body[..apply_body.find("\n    }\n").expect("it closes")];
    assert!(
        apply_body.contains("let _workspace_lock = self.acquire_workspace_lock().await?;")
            && apply_body.contains("let _guard = self.apply_guard.lock().await;"),
        "the apply path must hold both guards the same way:\n{apply_body}"
    );

    // ⛔ No probe: the decision reads the projection and the operator's stated
    // finding, never the filesystem (AD-12; `ADR-18-3a-d-01:72`).
    for probe in [
        "git status",
        "git diff",
        "rev-parse",
        "workspace_revision",
        "std::fs::",
        "Command::new",
    ] {
        assert!(
            !body.contains(probe),
            "the release verb must not probe the working tree: `{probe}`"
        );
    }
    // ⛔ And no chained apply: resolution unwedges, it does not re-run.
    assert!(
        !body.contains("self.applier"),
        "resolution performs no workspace write"
    );

    // ⛔ The append lives in its OWN function, never inside `apply`.
    let apply_body = &merge_back[merge_back
        .find("pub async fn apply(")
        .expect("positive control: the apply seam exists")..];
    let apply_body = &apply_body[..apply_body.find("\n    }\n").expect("it closes")];
    assert!(
        !apply_body.contains("PatchApplyInspected"),
        "⛔ the resolution append must not be nested inside `apply` (Trap 7)"
    );
}

/// **AC2 fourth mutant — swallow the `persist` error and return `Ok`.**
///
/// ⛔ Not provable behaviourally: `PatchMergeBack.journal` is
/// `Arc<NodeJournal>`, a concrete struct with no injectable seam, so a failing
/// append cannot be produced from a test — the same reason 18.3a-c and 18.3a-d
/// both recorded. ⛔ Do not invent a `RoomJournalWriter` port to route around
/// it. Proven structurally instead: the verb's last expression IS the
/// `persist` result, so there is no place to drop an `Err`.
#[test]
fn the_release_verb_cannot_report_success_on_a_failed_append() {
    let merge_back = source("src/infrastructure/orchestrator/merge_back.rs");
    let body = record_inspection_body(&merge_back);
    for forbidden in ["event_bus", "emit_domain", ".emit(", "let _ =", "Ok(())"] {
        assert!(
            !body.contains(forbidden),
            "`record_inspection` must return the append's own Result — `{forbidden}` \
             is how it would report success on a failure"
        );
    }
    assert!(
        body.trim_end().ends_with(".await"),
        "the durable append is the tail expression:\n{body}"
    );
}

// ═══════════════════════════════ AC3 ═══════════════════════════════

/// **AC3 second mutant — drop `#[serde(other)]` from `OperatorApplyFinding` and
/// the vendored fixture fails the WHOLE journal, not one line.**
///
/// ⚑ **Fixture shape is load bearing.** `parse_entries` silently drops a
/// *trailing* unparseable line (`Err(_) if is_last => break`), so an
/// unknown-finding record placed last would make the mutated and un-mutated
/// builds agree and the mutant would escape. The fixture carries a well-formed
/// record **after** it.
#[tokio::test]
async fn an_unknown_finding_from_a_newer_build_does_not_fail_the_journal() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let room_id = OrchestrationRoomId::parse("room-fixture-f").expect("room id");
    let rooms = workspace.path().join(".rustain").join("rooms");
    std::fs::create_dir_all(&rooms).expect("rooms dir");
    std::fs::copy(
        format!(
            "{}/tests/fixtures/18_3a_f/unknown_finding_apply.jsonl",
            env!("CARGO_MANIFEST_DIR")
        ),
        rooms.join(format!("{}.jsonl", room_id.as_str())),
    )
    .expect("vendored fixture is readable");

    let reader = WorkspaceJournalReader::open(workspace.path(), room_id);
    let entries = reader
        .load_entries()
        .await
        .expect("an unknown finding value must not fail the whole journal load");

    assert_eq!(
        entries.len(),
        3,
        "all three records survive — including the well-formed one that FOLLOWS \
         the unknown-finding line, which is what makes this fixture discriminate"
    );
    assert!(
        matches!(
            &entries[1].record,
            JournalRecord::Room(RoomEvent::PatchApplyInspected {
                finding: OperatorApplyFinding::Unknown,
                ..
            })
        ),
        "the unrecognised finding degrades to Unknown: {:?}",
        entries[1].record
    );
    assert!(
        matches!(
            &entries[2].record,
            JournalRecord::Room(RoomEvent::PatchApplyResolved { .. })
        ),
        "the record after the unknown line must not be swallowed: {:?}",
        entries[2].record
    );
}

/// **AC3 third mutant — add `#[serde(default)]` to `inspector` and a truncated
/// line deserializes into a FABRICATED identity.**
///
/// ⚠ Asserted deterministically: the un-mutated build returns `Err`, the
/// mutated build returns `Ok`. ⛔ Never assert on the fabricated value —
/// `AgentId::default()` is `AgentId::new()`, a fresh random nanoid, so an
/// equality assertion on it is non-deterministic and would flake.
#[test]
fn a_resolution_record_missing_its_inspector_is_rejected_not_defaulted() {
    let complete = format!(
        r#"{{"event":"patch_apply_inspected","artifact":"{}","finding":"present","inspector":"operator"}}"#,
        ArtifactId::from(hash('a')).as_str()
    );
    let decoded: RoomEvent = serde_json::from_str(&complete).expect("positive control: it decodes");
    assert!(matches!(
        decoded,
        RoomEvent::PatchApplyInspected {
            finding: OperatorApplyFinding::Present,
            ..
        }
    ));

    let truncated = format!(
        r#"{{"event":"patch_apply_inspected","artifact":"{}","finding":"present"}}"#,
        ArtifactId::from(hash('a')).as_str()
    );
    assert!(
        serde_json::from_str::<RoomEvent>(&truncated).is_err(),
        "a missing attribution field must fail the record, never mint an identity"
    );
}

/// **AC3 first mutant — omit `seen_apply_record = true` from the new fold arm.**
///
/// 🔴 **The stream shape is load bearing, because the obvious stream does NOT
/// fire this.** In any ordinary stream the `PatchApplyStarted` that created the
/// `Indeterminate` has already set the flag, so omitting it changes nothing.
/// The anomaly the arm exists to keep visible is the discriminating case: a
/// resolution for an artifact this projection never saw, then a fresh capture.
///
/// **AC3 fourth mutant** rides along: wrap the arm in the
/// `artifacts.get_mut` guard and the orphan resolution vanishes entirely.
#[test]
fn a_resolution_for_an_unseen_artifact_stays_visible_and_stamps_the_record_era() {
    let orphan = ArtifactId::from(hash('c'));
    let fresh = ArtifactId::from(hash('d'));
    let room = OrchestrationRoom::project(
        OrchestrationRoomId::default(),
        vec![
            RoomEvent::PatchApplyInspected {
                artifact: orphan.clone(),
                finding: OperatorApplyFinding::Present,
                inspector: AgentId::local_operator(),
            },
            RoomEvent::PatchCaptured {
                artifact: fresh.clone(),
                producer: AgentId::local_operator(),
            },
        ],
    );

    // Fourth mutant: the `artifacts.get_mut` guard would swallow this outright.
    assert_eq!(
        room.apply_state().get(&orphan).copied(),
        Some(ApplyState::OperatorResolved(OperatorApplyFinding::Present)),
        "a resolution for an unknown artifact is an anomaly and must stay visible"
    );
    // First mutant: without `seen_apply_record = true` the capture that follows
    // is stamped pre-record-era and the panel paints a false safety warning on
    // a brand-new patch.
    assert!(
        !room.predates_apply_records().contains(&fresh),
        "a patch captured AFTER a resolution is not from the pre-record era"
    );
}

/// **AC3 — replay determinism (NFR70(d)).**
///
/// ⛔ Not asserted over `project_for_host` alone: that is a pure constructor and
/// the assertion would be vacuous.
#[tokio::test]
async fn replaying_a_resolution_stream_reconstructs_the_identical_room() {
    let harness = Harness::new().await;
    let applier = Arc::new(CrashingApplier::new(false));
    let service = harness.service(applier);
    let artifact = harness.capture(&service, "old", "new").await;
    harness.wedge(&artifact).await;
    service
        .record_inspection(
            &artifact,
            OperatorApplyFinding::Present,
            AgentId::local_operator(),
        )
        .await
        .expect("resolve");

    let events: Vec<RoomEvent> = harness
        .journal
        .load()
        .await
        .expect("journal loads")
        .into_iter()
        .filter_map(|entry| match entry.record {
            JournalRecord::Room(event) => Some(event),
            _ => None,
        })
        .collect();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, RoomEvent::PatchApplyInspected { .. })),
        "positive control: the stream really carries the new variant"
    );
    let first = OrchestrationRoom::project(OrchestrationRoomId::default(), events.clone());
    let second = OrchestrationRoom::project(OrchestrationRoomId::default(), events);
    assert_eq!(
        first, second,
        "replaying the journal reconstructs the identical room"
    );
}

/// **AC3 additivity positive control** — without it, a build that mis-folds
/// everything passes the additivity mutants vacuously.
///
/// A pre-`18-3a-f` journal (no `PatchApplyInspected` at all) must project
/// exactly as it did before this change.
#[tokio::test]
async fn a_pre_resolution_journal_projects_unchanged() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let room_id = OrchestrationRoomId::parse("room-pre-f").expect("room id");
    let rooms = workspace.path().join(".rustain").join("rooms");
    std::fs::create_dir_all(&rooms).expect("rooms dir");
    std::fs::copy(
        format!(
            "{}/tests/fixtures/18_3a_e/pre_applier_apply.jsonl",
            env!("CARGO_MANIFEST_DIR")
        ),
        rooms.join(format!("{}.jsonl", room_id.as_str())),
    )
    .expect("pre-18.3a-f fixture is readable");

    let reader = WorkspaceJournalReader::open(workspace.path(), room_id);
    let events: Vec<RoomEvent> = reader
        .load_entries()
        .await
        .expect("loads")
        .into_iter()
        .filter_map(|entry| match entry.record {
            JournalRecord::Room(event) => Some(event),
            _ => None,
        })
        .collect();
    assert_eq!(events.len(), 2, "positive control: the fixture has content");
    let room = OrchestrationRoom::project(OrchestrationRoomId::default(), events);
    let applied = ArtifactId::from(hash('c'));
    assert_eq!(
        room.apply_state().get(&applied).copied(),
        Some(ApplyState::Resolved(ApplyOutcome::Applied)),
        "the additive variant must not disturb a journal that predates it"
    );
    assert_eq!(
        room.apply_state().len(),
        1,
        "no phantom lattice entries appeared"
    );
}

// ═══════════════════════════════ AC4 ═══════════════════════════════

/// A real `AppState` composed over `workspace`: the real journal read path and
/// the real merge-back service bound to **both** artifact ports. Everything
/// else is inert noop composition.
///
/// Exists for the front-door keystone: it must drive the exact callee the
/// `InputAction::ExecuteCommand` dispatch arm invokes
/// (`artifact_bridge::artifact_command`), which reads the journal and both
/// ports **through `AppState`** — a harness that bypasses it proves the seam,
/// not that the operator can reach it.
fn resolve_app_state(
    workspace: &Path,
    recorder: Arc<dyn rustain::domain::ports::PatchReviewRecorder>,
    executor: Arc<dyn rustain::domain::ports::PatchApplyExecutor>,
    resolver: Arc<dyn rustain::domain::ports::PatchApplyResolver>,
) -> rustain::infrastructure::runtime::app_state::AppState {
    use arc_swap::ArcSwap;
    use clap::Parser;
    use rustain::adapters::noop::{NoOpProvider, NoOpStorage};
    use rustain::domain::ports::StreamingProvider;
    use rustain::domain::services::plan_manager::PlanManager;
    use rustain::domain::services::plan_mode_injector::DefaultPlanInjector;
    use rustain::infrastructure::composition::ComposeContext;
    use rustain::infrastructure::runtime::agent_core::AgentCore;
    use rustain::infrastructure::runtime::app_state::AppState;

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
    app_state.transparency = Some(Arc::new(
        rustain::infrastructure::transparency::TransparencyService::new(
            Arc::new(WorkspaceJournalReader::open_workspace(workspace)),
            workspace.to_path_buf(),
        ),
    ));
    app_state.patch_review = Some(recorder);
    app_state.patch_apply = Some(executor);
    app_state.patch_resolve = Some(resolver);
    app_state
}

/// A wedged artifact reachable through the operator's own addressing, on the
/// **current** host so the bridge's `project_for_host` fold sees it.
struct FrontDoor {
    workspace: tempfile::TempDir,
    merge_back: Arc<PatchMergeBack>,
    recorder: Arc<rustain::infrastructure::orchestrator::JournalPatchReview>,
    artifact: ArtifactRef,
}

impl FrontDoor {
    async fn wedged() -> Self {
        use rustain::infrastructure::orchestrator::JournalPatchReview;

        let workspace = tempfile::tempdir().expect("workspace");
        init_repo(workspace.path());
        let store: Arc<dyn ArtifactStore> =
            Arc::new(FileSystemArtifactStore::new(workspace.path()));
        let journal = Arc::new(
            NodeJournal::open_workspace(workspace.path())
                .await
                .expect("journal"),
        );
        let applier = Arc::new(CrashingApplier::new(true));
        let entered = applier.entered.clone();
        let merge_back = Arc::new(PatchMergeBack::new(
            workspace.path().to_path_buf(),
            store.clone(),
            journal.clone(),
            bus(),
            applier,
        ));
        let artifact = merge_back
            .capture(
                AgentId::new(),
                CapabilityTokenId::root(),
                vec![ProvenanceTag::UserOriginated],
                vec![],
                HostBinding::new(
                    rustain::infrastructure::subagent::current_host_id(workspace.path()),
                    "ws",
                ),
                &patch("old", "new"),
            )
            .await
            .expect("capture");

        // A REAL crash inside the apply window. ⛔ Never a hand-built state.
        let held = {
            let merge_back = merge_back.clone();
            let artifact = artifact.clone();
            tokio::spawn(async move {
                merge_back
                    .apply(
                        &artifact,
                        OwnershipKind::Owned,
                        PermissionMode::Yolo,
                        &shipped_policy(),
                        None,
                    )
                    .await
            })
        };
        entered.notified().await;
        held.abort();
        let _ = held.await;

        let recorder = Arc::new(JournalPatchReview::new(
            merge_back.clone(),
            store,
            shipped_policy(),
        ));
        let door = Self {
            workspace,
            merge_back,
            recorder,
            artifact,
        };
        assert_eq!(
            door.projected().await,
            ApplyState::Indeterminate,
            "the front-door harness must start from a REAL wedge"
        );
        door
    }

    fn app_state(&self) -> rustain::infrastructure::runtime::app_state::AppState {
        resolve_app_state(
            self.workspace.path(),
            self.recorder.clone(),
            self.merge_back.clone(),
            self.merge_back.clone(),
        )
    }

    fn journal_lines(&self) -> Vec<String> {
        let path = self
            .workspace
            .path()
            .join(".rustain")
            .join("rooms")
            .join(format!(
                "room-{}.jsonl",
                rustain::infrastructure::paths::workspace_hash(self.workspace.path())
            ));
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    async fn projected(&self) -> ApplyState {
        let reader = WorkspaceJournalReader::open_workspace(self.workspace.path());
        let entries = reader.load_entries().await.expect("load");
        let events = entries.into_iter().filter_map(|entry| match entry.record {
            JournalRecord::Room(event) => Some(event),
            _ => None,
        });
        OrchestrationRoom::project(OrchestrationRoomId::default(), events)
            .apply_state()
            .get(&self.artifact.id)
            .copied()
            .unwrap_or_default()
    }

    fn prefix(&self) -> String {
        self.artifact.id.as_str()[..6].to_owned()
    }
}

/// **AC4 keystone (a) — behavioural, through the REAL front door.**
///
/// `state.input_buffer` → `submit_message_for_test` →
/// `InputAction::ExecuteCommand` → `artifact_bridge::artifact_command` → the
/// card → `handle_input('y')` → `InputAction::ApplyCardAccept` →
/// `resolve_apply_card` → the real effect arm → the journal file on disk.
///
/// ⛔ **Forbidden bypass (named):** driving `resolve_apply_card` or the port
/// without going through `handle_input` — the exact bypass cut 2's review
/// caught, and the third consecutive cut in which it happened.
///
/// **First mutant:** append when the card is *opened* rather than on `y` — the
/// journal is asserted byte-identical across the preview.
/// **Second mutant:** make `n`/`Esc` fall through to accept — the decline half
/// asserts nothing was written and the state did not move.
#[tokio::test]
async fn the_resolve_command_reaches_the_journal_through_the_real_dispatch_path() {
    use rustain::adapters::tui::app::{InputAction, handle_input, submit_message_for_test};
    use rustain::adapters::tui::handlers::artifact_command::resolve_apply_card;
    use rustain::adapters::tui::state::{ArtifactCardMode, TuiState};
    use rustain::domain::events::DomainInputEvent;

    let door = FrontDoor::wedged().await;
    let app_state = door.app_state();
    let mut state = TuiState::new(120, 40);

    // ── decline first: n writes nothing ──────────────────────────────────
    let before = door.journal_lines();
    state.input_buffer = format!("/artifact resolve {} absent", door.prefix());
    let InputAction::ExecuteCommand { name, args } = submit_message_for_test(&mut state) else {
        panic!("the resolve command must route to ExecuteCommand");
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
    let card = state
        .pending_artifact_card
        .as_ref()
        .expect("the resolve decision card");
    assert_eq!(
        card.mode,
        ArtifactCardMode::Resolve(OperatorApplyFinding::Absent),
        "the finding travels in the command, not in the card's keys"
    );
    assert_eq!(
        door.journal_lines(),
        before,
        "opening the card must append nothing"
    );
    assert_eq!(
        handle_input(&mut state, &DomainInputEvent::KeyPress('n')),
        InputAction::ApplyCardDecline
    );
    assert!(resolve_apply_card(&mut state, false).is_none());
    assert_eq!(door.journal_lines(), before, "decline writes nothing");
    assert_eq!(
        door.projected().await,
        ApplyState::Indeterminate,
        "a declined report leaves the latch closed"
    );

    // ── then accept: y records exactly one line and releases the latch ───
    state.input_buffer = format!("/artifact resolve {} present", door.prefix());
    let InputAction::ExecuteCommand { args, .. } = submit_message_for_test(&mut state) else {
        panic!("ExecuteCommand");
    };
    rustain::infrastructure::runtime::artifact_bridge::artifact_command(
        &mut state,
        "conv",
        args.as_deref(),
        &app_state,
        PermissionMode::Yolo,
    )
    .await;
    assert!(state.pending_artifact_card.is_some());
    assert_eq!(door.journal_lines(), before, "still nothing before `y`");
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

    let after = door.journal_lines();
    assert_eq!(after.len(), before.len() + 1, "exactly one durable line");
    let recorded = &after[before.len()];
    assert!(
        recorded.contains("\"event\":\"patch_apply_inspected\""),
        "{recorded}"
    );
    assert!(recorded.contains("\"finding\":\"present\""), "{recorded}");
    assert!(
        recorded.contains("\"inspector\":\"operator\""),
        "{recorded}"
    );
    assert_eq!(
        door.projected().await,
        ApplyState::OperatorResolved(OperatorApplyFinding::Present),
        "the latch is released through the production path"
    );
}

/// **AC4 third and sixth mutants, and the NFR34/NFR35 layer.**
///
/// Third: drop the epistemic sentence and this painted-buffer needle fires.
/// Sixth: paint the resolve card with the unmodified `APPLY_CARD_BINDINGS` and
/// it shows `[y] Apply` on a verb that applies nothing (ruling A13).
///
/// ⚠ Asserted on a **painted `ratatui` buffer**, never a formatter's return
/// value, and at the spec's 36/60/120 reference widths. Reading symbols out of
/// the buffer discards style entirely, so every claim here is carried by text —
/// NFR34's "never colour alone" holds by construction.
#[test]
fn the_painted_resolve_card_states_its_epistemics_and_never_paints_apply() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use rustain::adapters::tui::state::{ArtifactCardMode, PendingArtifactCard, TuiState};
    use rustain::adapters::tui::widgets::{apply_card, inline_card};

    let state = TuiState::new(80, 24);
    for (finding, expected) in [
        (OperatorApplyFinding::Present, "ARE in the working tree"),
        (OperatorApplyFinding::Absent, "are NOT in the working tree"),
    ] {
        let card = PendingArtifactCard {
            conversation_id: "conversation".to_owned(),
            artifact: card_artifact(),
            files: vec!["src/lib.rs".to_owned()],
            workspace: std::path::PathBuf::from("/workspace"),
            prior_focus: rustain::domain::models::FocusState::Input,
            predates_apply_records: true,
            mode: ArtifactCardMode::Resolve(finding),
        };
        for width in [36u16, 60, 120] {
            let area = Rect::new(0, 0, width, 20);
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
            assert!(!rendered.trim().is_empty(), "width {width} renders nothing");
            // NFR35: nothing may spill past the pane edge at ANY reference
            // width. The card wraps its prose and breaks its action row rather
            // than truncating, so every claim below is asserted at 36 too — a
            // clipped needle would make the negative assertions vacuous, which
            // is the Rule-0 Class A failure this file exists to avoid.
            for line in rendered.lines() {
                assert!(
                    line.chars().count() <= width as usize,
                    "width {width}: a line spilled past the pane edge:\n{rendered}"
                );
            }
            // Prose needles are matched against a chrome-stripped,
            // whitespace-normalised view: the card WRAPS its sentences to the
            // pane, so at 36 columns a claim is painted across two rows with a
            // box border between them — and it is no less painted for it.
            // ⚠ The raw render is what the width and key assertions use.
            let flat = rendered
                .chars()
                .map(|glyph| {
                    if "║╔╗╚╝═".contains(glyph) {
                        ' '
                    } else {
                        glyph
                    }
                })
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");

            // Sixth mutant (A13): ⛔ never `[y] Apply` on a verb that applies
            // nothing, and ⛔ no label that says "Apply".
            assert!(
                !rendered.contains("[y] Apply"),
                "width {width}: the resolve card must not paint an Apply key:\n{rendered}"
            );
            assert!(
                rendered.contains("[y] Record report"),
                "width {width}:\n{rendered}"
            );
            assert!(
                rendered.contains("[n] Cancel (Esc)"),
                "width {width}:\n{rendered}"
            );

            // Third mutant: the epistemic sentence, stated before `y`.
            for needle in [
                expected,
                "what YOU report, not what the system observed",
                "No workspace write is performed",
            ] {
                assert!(
                    flat.contains(needle),
                    "width {width}: missing {needle:?}:\n{rendered}"
                );
            }
            // Ruling A9: `present` — and only `present` — owes the
            // double-apply consequence.
            assert_eq!(
                flat.contains("can succeed silently"),
                matches!(finding, OperatorApplyFinding::Present),
                "width {width}: only `present` owes the double-apply warning:\n{rendered}"
            );

            // The wording ceiling: `ADR-18-3a-d-01:49` plus ruling P6, plus the
            // panel's already-ratcheted integrity vocabulary.
            let lowered = flat.to_ascii_lowercase();
            for forbidden in [
                "audit trail",
                "evidence",
                "authenticated",
                "tamper",
                "verified",
                "proof",
                "cryptograph",
                "provably",
                "permanent",
                "final",
                "durable",
            ] {
                assert!(
                    !lowered.contains(forbidden),
                    "width {width}: forbidden wording {forbidden:?}:\n{rendered}"
                );
            }
        }
    }
}

/// **Review finding F3 — the epistemic consent survives a multi-file overflow
/// (AC4).**
///
/// The resolve card renders through the tail-scrolling decision-card renderer.
/// A multi-file patch can produce more lines than the pane shows, so the lead
/// scrolls off — and the consent sentence must stay visible before `[y]`,
/// because confirming a report the operator was not shown is the exact defect
/// the epistemics exist to prevent. After the re-order the consent sits in the
/// tail region the renderer always keeps.
#[test]
fn the_resolve_card_keeps_the_consent_visible_when_files_overflow() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use rustain::adapters::tui::state::{ArtifactCardMode, PendingArtifactCard, TuiState};
    use rustain::adapters::tui::widgets::{apply_card, inline_card};

    let state = TuiState::new(80, 24);
    // Enough files that the joined list overflows a short pane many times over.
    let many_files: Vec<String> = (0..40)
        .map(|i| format!("crates/module_{i}/src/deeply/nested/path/to/file_{i}.rs"))
        .collect();
    let card = PendingArtifactCard {
        conversation_id: "conversation".to_owned(),
        artifact: card_artifact(),
        files: many_files,
        workspace: std::path::PathBuf::from("/workspace"),
        prior_focus: rustain::domain::models::FocusState::Input,
        predates_apply_records: true,
        mode: ArtifactCardMode::Resolve(OperatorApplyFinding::Absent),
    };
    // A short pane forces the decision-card renderer to tail-scroll.
    let height = 16u16;
    let width = 60u16;
    let area = Rect::new(0, 0, width, height);
    let lines = apply_card::render_apply_card_lines(&card, &state.theme, area.width);
    assert!(
        lines.len() > height as usize,
        "precondition: the card overflows the {}-row pane ({} lines)",
        height,
        lines.len()
    );
    let mut buffer = Buffer::empty(area);
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
    // Strip the decision-card chrome the same way the painted-card test does.
    let flat = rendered
        .chars()
        .map(|glyph| {
            if "║╔╗╚╝═".contains(glyph) {
                ' '
            } else {
                glyph
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        flat.contains("what YOU report, not what the system observed"),
        "the epistemic consent must survive overflow:\n{rendered}"
    );
    assert!(
        rendered.contains("[y] Record report"),
        "the decision actions must remain visible after overflow:\n{rendered}"
    );
}

/// **AC4 fifth mutant — accept a bare `/artifact resolve <id>` with no finding.**
#[test]
fn the_resolve_subverb_parses_exactly_one_id_and_one_finding() {
    use rustain::adapters::tui::handlers::artifact_command::{
        ArtifactCommandArgs, USAGE, parse_artifact_command, parse_finding,
    };

    assert_eq!(
        parse_artifact_command(false, Some("resolve abc123 present")).expect("parses"),
        ArtifactCommandArgs::Resolve {
            id: "abc123".to_owned(),
            finding: OperatorApplyFinding::Present,
        }
    );
    assert_eq!(
        parse_artifact_command(false, Some("resolve abc123 absent")).expect("parses"),
        ArtifactCommandArgs::Resolve {
            id: "abc123".to_owned(),
            finding: OperatorApplyFinding::Absent,
        }
    );
    for (input, fragment) in [
        ("resolve", "needs an artifact id"),
        ("resolve abc123", "needs a finding"),
        ("resolve abc123 present extra", "one id and one finding"),
        ("resolve abc123 applied", "unknown finding `applied`"),
        // ⛔ `unknown` is what a newer build's value degrades to, not something
        // an operator may assert (ruling A8's other half).
        ("resolve abc123 unknown", "unknown finding `unknown`"),
    ] {
        let error = parse_artifact_command(false, Some(input)).expect_err(input);
        assert!(error.contains(fragment), "{input}: {error}");
        assert!(error.contains(USAGE), "{input}: every refusal names USAGE");
    }
    // ⛔ The verb must be discoverable, or it is the 18.3c defect class.
    assert!(USAGE.contains("/artifact resolve <id> present|absent"));
    let palette = rustain::adapters::command_registry::CommandRegistry::new();
    let artifact_entry = palette
        .find("artifact")
        .expect("positive control: the palette advertises /artifact");
    assert!(
        artifact_entry.description.contains("/artifact resolve"),
        "{}",
        artifact_entry.description
    );
    assert!(
        source("src/adapters/tui/help_data.rs").contains("/artifact resolve <id> present|absent"),
        "the help overlay must advertise the verb"
    );
    // Positive control that `parse_finding` is not a constant function.
    assert_eq!(
        parse_finding("present"),
        Some(OperatorApplyFinding::Present)
    );
    assert_eq!(parse_finding("absent"), Some(OperatorApplyFinding::Absent));
    assert_eq!(parse_finding("unknown"), None);
}

/// **AC4 seventh mutant — route the resolution outcome through
/// `render_apply_result` and it claims *"applied to the workspace"* after a
/// call that performed no workspace write (ruling A14).**
#[test]
fn the_resolve_result_never_claims_the_workspace_changed() {
    use rustain::adapters::tui::handlers::artifact_command::render_resolve_result;
    use rustain::domain::ports::PatchResolvePortError;

    let id = ArtifactId::from(hash('d'));
    let success = render_resolve_result(&id, OperatorApplyFinding::Present, Ok(()));
    assert!(
        !success.contains("applied to the workspace"),
        "the one verb that never writes must not claim a write: {success}"
    );
    assert!(success.contains("recorded your report"), "{success}");
    assert!(success.contains("was not written to"), "{success}");
    let lowered = success.to_ascii_lowercase();
    for forbidden in ["permanent", "final", "durable", "evidence", "proof"] {
        assert!(!lowered.contains(forbidden), "{success}");
    }

    for (error, fragment) in [
        (PatchResolvePortError::WorkspaceBusy, "workspace is busy"),
        (
            PatchResolvePortError::NotWedged("Resolved(Applied)".to_owned()),
            "not awaiting an operator report",
        ),
        (
            PatchResolvePortError::RecordFailed("disk full".to_owned()),
            "could not be recorded",
        ),
    ] {
        let rendered = render_resolve_result(&id, OperatorApplyFinding::Absent, Err(error));
        assert!(rendered.contains(fragment), "{rendered}");
        assert!(
            !rendered.contains("recorded your report"),
            "a refusal must not read as a success: {rendered}"
        );
    }
}

/// **AC4 keystone (b) — structural.**
///
/// `event_loop.rs` did not grow, and the resolve verb reaches the bridge rather
/// than an inline body there. ⚑ The whole 0-line result rests on one move: a
/// mode field on the card plus a branch inside `apply_confirmed_card`.
#[test]
fn the_resolve_verb_costs_the_event_loop_nothing() {
    let loop_source = source("src/infrastructure/runtime/event_loop.rs");
    let lines = loop_source.lines().count();
    assert!(
        lines <= 11_321,
        "event_loop.rs has {lines} lines — put logic in artifact_bridge.rs, do not bump the cap"
    );
    // ⚑ RE-BASED 2026-08-14 by Story 18.4b, from 11_285. That story's whole
    // deliverable is a new operator surface, so unlike 18.3a-f it cannot cost
    // the loop zero; it is amended here rather than paralleled by a second pin.
    // Its budget is accounted for exactly:
    //
    //   +2  the `/peer` dispatch arm, before the adapter-override catch-all
    //   +4  the PeerAddConfirm | PeerAddDecline resolution arm
    //   +9  the pending-peer-add render branch
    //   ---
    //   +15 → 11_300
    //
    // Everything else lives in `peer_bridge.rs`, `cli/peer/*` and
    // `handlers/peer_command.rs`.
    assert_eq!(
        lines, 11_300,
        "18.3a-f budgeted ZERO added lines (ruling A7) and 18.4b re-based this to \
         11_300 for exactly 15 accounted lines. Growth beyond that is a design \
         failure, not a budget question: reuse the card slot, the ConfirmationType, \
         the InputActions, the render branch and the single effect call, and put \
         verb logic in a bridge."
    );

    // ⛔ No second card slot, ConfirmationType, InputAction or render branch.
    for forbidden in [
        "pending_resolve_card",
        "ConfirmationType::ArtifactResolve",
        "InputAction::ResolveCard",
        "render_resolve_card_lines",
        "ArtifactCommandArgs::Resolve",
        "record_operator_inspection",
    ] {
        assert!(
            !loop_source.contains(forbidden),
            "the event loop must stay ignorant of the resolve verb: `{forbidden}`"
        );
    }
    // Positive control: the three needles cut 1 pinned into the event loop are
    // still there, so the assertions above are not vacuously true of a file
    // that lost its card wiring entirely.
    for needle in [
        "InputAction::ApplyCardAccept",
        "InputAction::ApplyCardDecline",
        "render_apply_card_lines",
        "pending_artifact_card",
    ] {
        assert!(loop_source.contains(needle), "{needle}");
    }

    // The new dispatch arm reaches the bridge, and the bridge reaches the
    // SIBLING port — ⛔ never the apply port, which would make "no workspace
    // write" false by construction.
    let bridge = source("src/infrastructure/runtime/artifact_bridge.rs");
    assert!(bridge.contains("ArtifactCommandArgs::Resolve"));
    assert!(bridge.contains("pub async fn resolve_artifact_apply("));
    assert_eq!(
        bridge.matches(".record_operator_inspection(").count(),
        1,
        "one resolution port call"
    );
    // ⛔ Cut 1 and cut 2's bridge ratchets stay green beside the new verb.
    assert_eq!(bridge.matches(".apply_patch(").count(), 1);
    assert_eq!(bridge.matches(".record_verdict(").count(), 1);
    assert!(!bridge.contains("append_room") && !bridge.contains("RoomJournal>"));
    assert!(!bridge.contains("review_and_apply"));
    let lowered = bridge.to_ascii_lowercase();
    assert!(lowered.contains("seam, not enforcement"));
    for forbidden in ["capabilitytoken", "authorityprovider", "fingerprint"] {
        assert!(!lowered.contains(forbidden), "`{forbidden}` in the bridge");
    }

    // ⛔ And the handler layer stays free of infrastructure.
    for line in source("src/adapters/tui/handlers/artifact_command.rs").lines() {
        let trimmed = line.trim_start();
        assert!(
            trimmed.starts_with("//") || !trimmed.contains("crate::infrastructure::"),
            "adapters/tui/handlers must not reference infrastructure: {line}"
        );
    }
}

/// **AC5 positive control for the two inverted assertions.**
///
/// ⚑ *A deletion wearing a disguise is the failure mode.* Cut 2 required the
/// literal `"18-3a-f"` in the painted apply card and in
/// `render_apply_result(ApplyIndeterminate)`. Both were inverted rather than
/// deleted — this proves the replacements are reachable strings that a build
/// can fail on, from the other side of the file boundary.
#[test]
fn the_apply_surface_routes_the_operator_to_the_verb_that_now_exists() {
    use rustain::adapters::tui::handlers::artifact_command::render_apply_result;
    use rustain::domain::ports::PatchApplyPortError;

    let id = ArtifactId::from(hash('d'));
    for error in [
        PatchApplyPortError::ApplyIndeterminate,
        PatchApplyPortError::ApplyUnresolved("append failed".to_owned()),
    ] {
        let rendered = render_apply_result(&id, Err(error));
        assert!(rendered.contains("/artifact resolve"), "{rendered}");
        assert!(
            !rendered.contains("no resolution verb exists yet"),
            "{rendered}"
        );
    }
    // The apply card's own recovery line, and the port errors behind it.
    assert!(
        source("src/adapters/tui/widgets/apply_card.rs")
            .contains("/artifact resolve <id> present|absent"),
        "the apply card must name the recovery verb"
    );
    let port = source("src/domain/ports/patch_apply_executor.rs");
    assert!(
        port.contains("/artifact resolve <id> present|absent"),
        "{port}"
    );
    assert!(
        !port.contains("18-3a-f"),
        "the port must not still name an unshipped story"
    );

    // ⛔ No in-tree site may still claim the verb does not exist.
    let mut sources = Vec::new();
    collect_sources(
        &std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut sources,
    );
    let liars: Vec<&str> = sources
        .iter()
        .filter(|(_, body)| body.contains("no resolution verb exists yet"))
        .map(|(path, _)| path.as_str())
        .collect();
    assert!(
        liars.is_empty(),
        "stale dead-end claims survive in {liars:?}"
    );
}

/// **AC5 — the new target actually runs, in BOTH lanes.**
///
/// ⚠ A conformance file does not run unless a line names it. ⛔ And the filename
/// carries neither `a2a` nor `transparency`.
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
        default_lane.contains("cargo test --test conformance_18_3a_f_resolution"),
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
        a2a_lane.contains("--test conformance_18_3a_f_resolution"),
        "the A2A lane must execute this story target"
    );
    assert!(
        !concat!(file!(), "").contains("a2a"),
        "this filename must not contain `a2a`"
    );
    assert!(
        !concat!(file!(), "").contains("transparency"),
        "this filename must not contain `transparency`"
    );
}
