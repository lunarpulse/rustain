//! Story 18.3a-d — durable apply record, crash recovery and the cross-process
//! apply lock.
//!
//! ⛔ **No `a2a` in this filename.**
//! `every_a2a_integration_test_is_wired_into_the_ci_a2a_lane` fails the
//! **default** lane for an unlisted `tests/*a2a*.rs`.
//!
//! Class C throughout: a real git workspace, a real `NodeJournal`, a real
//! `WorkspaceJournalReader`, a real `flock`, and a `PatchApplier` double that
//! **decorates** the production `GitPatchApplier` rather than replacing it.

#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use rustain::adapters::artifact::FileSystemArtifactStore;
use rustain::adapters::merge_back::GitPatchApplier;
use rustain::domain::models::node_journal::{JournalRecord, NODE_JOURNAL_SCHEMA_VERSION};
use rustain::domain::models::{
    AgentId, ApplyOutcome, ApplyState, ArtifactId, ArtifactRef, CapabilityTokenId, ContentHash,
    HostBinding, OrchestrationRoomId, OwnershipKind, PermissionMode, ProvenanceTag,
    ProvisioningTier, RoomEvent, UnifiedDiff,
};
use rustain::domain::ports::{ArtifactStore, PatchApplier, PatchApplyError, RoomJournalReader};
use rustain::domain::services::patch_review::MergeBackPolicy;
use rustain::infrastructure::orchestrator::{MergeBackError, PatchMergeBack};
use rustain::infrastructure::runtime::event_bus::EventBus;
use rustain::infrastructure::subagent::{NodeJournal, WorkspaceJournalReader};
use tokio::sync::Notify;

const HOST: &str = "host-apply";

fn source(relative: &str) -> String {
    std::fs::read_to_string(format!("{}/{relative}", env!("CARGO_MANIFEST_DIR")))
        .unwrap_or_else(|error| panic!("read {relative}: {error}"))
}

fn merge_back_source() -> String {
    source("src/infrastructure/orchestrator/merge_back.rs")
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

/// The shipped `/fanout` merge-back configuration: `startup.rs` hardcodes
/// `auto_approve_user_originated: true`, which is why `DF-17-3b-1` is reachable
/// in every build without an operator door.
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

/// 🔴 The first `PatchApplier` double in this codebase's history — and a
/// **DECORATOR**, never a stub.
///
/// At the instant `apply` is invoked it reads the room journal *through the
/// production reader* and records how many apply records were **already
/// durably present**. That is the write-ahead assertion, and nothing weaker
/// works: a `PatchApplyStarted` appended *after* `git apply` still carries a
/// lower `seq` than its `Resolved`, so a sequence-ordering assertion passes
/// while the mutant escapes.
///
/// It then **delegates to a real [`GitPatchApplier`]**. A stub returning
/// `Ok(())` would satisfy the witness and silently kill AC1's own positive
/// control — that the working tree content actually changed.
struct JournalWitnessApplier {
    inner: GitPatchApplier,
    reader: WorkspaceJournalReader,
    calls: Arc<AtomicUsize>,
    started_seen: Arc<AtomicUsize>,
    resolved_seen: Arc<AtomicUsize>,
    revision_calls: Arc<AtomicUsize>,
    /// Simulate a process death inside the apply window: mutate the tree for
    /// real, announce it, then never return. The driving test aborts the task,
    /// so `PatchApplyResolved` is never appended — deterministically, with no
    /// timing window and no `kill -9`.
    hang_after_mutating: bool,
    entered: Arc<Notify>,
}

impl JournalWitnessApplier {
    fn new(workspace: &Path, hang_after_mutating: bool) -> Self {
        Self {
            inner: GitPatchApplier,
            reader: WorkspaceJournalReader::open_workspace(workspace),
            calls: Arc::new(AtomicUsize::new(0)),
            started_seen: Arc::new(AtomicUsize::new(0)),
            resolved_seen: Arc::new(AtomicUsize::new(0)),
            revision_calls: Arc::new(AtomicUsize::new(0)),
            hang_after_mutating,
            entered: Arc::new(Notify::new()),
        }
    }
}

#[async_trait]
impl PatchApplier for JournalWitnessApplier {
    async fn apply(&self, workspace: &Path, body: &[u8]) -> Result<(), PatchApplyError> {
        let entries = self
            .reader
            .load_entries()
            .await
            .expect("the room journal is readable from inside the apply");
        let (started, resolved) = count_apply_records(&entries);
        self.started_seen.store(started, Ordering::SeqCst);
        self.resolved_seen.store(resolved, Ordering::SeqCst);
        self.calls.fetch_add(1, Ordering::SeqCst);
        // 🔴 Delegate. Never short-circuit.
        let outcome = self.inner.apply(workspace, body).await;
        if self.hang_after_mutating {
            self.entered.notify_one();
            std::future::pending::<()>().await;
        }
        outcome
    }

    async fn revision(&self, workspace: &Path) -> Option<String> {
        self.revision_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.revision(workspace).await
    }
}

fn count_apply_records(entries: &[rustain::domain::models::JournalEntry]) -> (usize, usize) {
    let mut started = 0;
    let mut resolved = 0;
    for entry in entries {
        match &entry.record {
            JournalRecord::Room(RoomEvent::PatchApplyStarted { .. }) => started += 1,
            JournalRecord::Room(RoomEvent::PatchApplyResolved { .. }) => resolved += 1,
            _ => {}
        }
    }
    (started, resolved)
}

async fn journal_apply_records(journal: &NodeJournal) -> (usize, usize) {
    let entries = journal.load().await.expect("journal loads");
    count_apply_records(&entries)
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

    /// A `PatchMergeBack` addressing the same workspace through a different
    /// path spelling — the two-process configuration `DF-17-3b-2` names,
    /// modulo the POSIX property stated on AC3's keystone below.
    fn service_at(&self, workspace: &Path, applier: Arc<dyn PatchApplier>) -> PatchMergeBack {
        PatchMergeBack::new(
            workspace.to_path_buf(),
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
}

// ═══════════════════════════════ AC1 ═══════════════════════════════

/// **AC1 keystone (a) — behavioural, and the positive control in one test.**
///
/// Front door: `PatchMergeBack::apply`, the sole `git apply` path, entered
/// under the shipped `auto_approve_user_originated: true` policy — the
/// `/fanout` merge-back arm's configuration.
///
/// ⛔ **Forbidden bypass:** constructing the two `RoomEvent`s in the test and
/// appending them directly. That proves the fold, not the bracket. The witness
/// below can only observe a `Started` that the *production path* wrote.
#[tokio::test]
async fn every_apply_is_bracketed_by_a_durable_write_ahead_record() {
    let harness = Harness::new().await;
    let witness = Arc::new(JournalWitnessApplier::new(harness.path(), false));
    let service = harness.service(witness.clone());
    let artifact = harness.capture(&service, "old", "new").await;

    service
        .apply(
            &artifact,
            OwnershipKind::Owned,
            PermissionMode::Yolo,
            &shipped_policy(),
            None,
        )
        .await
        .expect("the shipped policy applies a user-originated patch");

    assert_eq!(
        witness.calls.load(Ordering::SeqCst),
        1,
        "the applier must be invoked exactly once"
    );
    assert_eq!(
        witness.started_seen.load(Ordering::SeqCst),
        1,
        "PatchApplyStarted must ALREADY be durable when the applier is invoked — \
         a seq-ordering assertion would not catch a Started appended afterwards"
    );
    assert_eq!(
        witness.resolved_seen.load(Ordering::SeqCst),
        0,
        "PatchApplyResolved cannot precede the mutation it resolves"
    );
    assert_eq!(
        journal_apply_records(&harness.journal).await,
        (1, 1),
        "exactly one Started and exactly one Resolved"
    );

    let room = harness
        .journal
        .project_room(HOST)
        .await
        .expect("project room");
    assert_eq!(
        room.apply_state().get(&artifact.id).copied(),
        Some(ApplyState::Resolved(ApplyOutcome::Applied)),
        "a completed apply resolves to Applied"
    );
    // Positive control: the bracket surrounds a REAL mutation, not a no-op.
    assert_eq!(
        harness.tree(),
        "new\n",
        "the working tree content must actually have changed"
    );
}

/// **AC1 — the preimage witness is best effort and never load bearing.**
///
/// `git apply` works outside a repository, so a workspace with no git history
/// is a supported case. `revision()` returns `None` and the apply still
/// succeeds; the honest `None` is what lands in the record.
#[tokio::test]
async fn a_workspace_without_git_history_still_applies_and_records_no_revision() {
    let workspace = tempfile::tempdir().expect("tempdir");
    std::fs::write(workspace.path().join("file.txt"), "old\n").expect("seed");
    let journal = Arc::new(
        NodeJournal::open_workspace(workspace.path())
            .await
            .expect("journal"),
    );
    let store: Arc<dyn ArtifactStore> = Arc::new(FileSystemArtifactStore::new(workspace.path()));
    let witness = Arc::new(JournalWitnessApplier::new(workspace.path(), false));
    let service = PatchMergeBack::new(
        workspace.path().to_path_buf(),
        store,
        journal.clone(),
        bus(),
        witness.clone(),
    );
    let artifact = service
        .capture(
            AgentId::new(),
            CapabilityTokenId::root(),
            vec![ProvenanceTag::UserOriginated],
            vec![],
            HostBinding::new(HOST, "ws"),
            &patch("old", "new"),
        )
        .await
        .expect("capture");

    service
        .apply(
            &artifact,
            OwnershipKind::Owned,
            PermissionMode::Yolo,
            &shipped_policy(),
            None,
        )
        .await
        .expect("a non-git workspace is a supported apply target");

    assert_eq!(witness.revision_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("file.txt")).expect("tree"),
        "new\n"
    );
    let revisions: Vec<Option<String>> = journal
        .load()
        .await
        .expect("journal loads")
        .into_iter()
        .filter_map(|entry| match entry.record {
            JournalRecord::Room(RoomEvent::PatchApplyStarted {
                workspace_revision, ..
            }) => Some(workspace_revision),
            _ => None,
        })
        .collect();
    assert_eq!(
        revisions,
        vec![None],
        "`None` is a first-class honest value, recorded as such"
    );
}

/// **AC1 third mutant — the resolution is appended on the FAILURE path too.**
///
/// Skip it there and a merely-conflicting apply projects as `Indeterminate`,
/// which AC2's guard then latches forever: strictly worse than the hole this
/// story closes. Doubles as the positive control that `Conflict` is preserved
/// rather than collapsed into `Failed`, and that a *readable* resolution is
/// retryable — the latch is for unknown state, not for failure.
#[tokio::test]
async fn a_conflicting_apply_records_a_resolved_conflict_and_stays_retryable() {
    let harness = Harness::new().await;
    let witness = Arc::new(JournalWitnessApplier::new(harness.path(), false));
    let service = harness.service(witness.clone());
    // Well formed, but its preimage is not what the tree holds: `git apply`
    // rejects it without mutating anything.
    let conflicting = harness.capture(&service, "stale", "new").await;

    let error = service
        .apply(
            &conflicting,
            OwnershipKind::Owned,
            PermissionMode::Yolo,
            &shipped_policy(),
            None,
        )
        .await
        .expect_err("a non-applying patch is a conflict");
    assert!(matches!(error, MergeBackError::Conflict(_)), "{error:?}");
    assert_eq!(harness.tree(), "old\n", "a conflict mutates nothing");
    assert_eq!(
        journal_apply_records(&harness.journal).await,
        (1, 1),
        "the failure path is bracketed exactly like the success path"
    );

    let room = harness
        .journal
        .project_room(HOST)
        .await
        .expect("project room");
    assert_eq!(
        room.apply_state().get(&conflicting.id).copied(),
        Some(ApplyState::Resolved(ApplyOutcome::Conflict)),
        "a resolved failure is NOT indeterminate, and Conflict is not Failed"
    );

    // Retryable: a readable resolution never latches, so the guard is refusing
    // a specific state rather than every artifact it has ever seen.
    let retry = service
        .apply(
            &conflicting,
            OwnershipKind::Owned,
            PermissionMode::Yolo,
            &shipped_policy(),
            None,
        )
        .await
        .expect_err("still a conflict");
    assert!(matches!(retry, MergeBackError::Conflict(_)), "{retry:?}");
    assert_eq!(
        witness.calls.load(Ordering::SeqCst),
        2,
        "the retry reached git"
    );
    assert_eq!(journal_apply_records(&harness.journal).await, (2, 2));
}

/// **AC1 keystone (b) — structural (Rule 4).**
///
/// The write-ahead ordering is a source property of one function; a behavioural
/// test proves the witness saw it, and this proves nobody reordered the source
/// out from under a future witness.
///
/// ⚠ The needle is **`.apply(&self.workspace`**, not `applier.apply(`: the
/// latter does not exist as a literal, because rustfmt splits the receiver onto
/// its own line.
#[test]
fn the_write_ahead_append_precedes_the_applier_call_in_source() {
    let merge_back = merge_back_source();
    let body = apply_fn_body(&merge_back);

    let applier_call = body
        .find(".apply(&self.workspace")
        .expect("positive control: the applier call site exists");
    let started_append = body
        .find("RoomEvent::PatchApplyStarted")
        .expect("positive control: the write-ahead append exists");
    let resolved_append = body
        .find("RoomEvent::PatchApplyResolved")
        .expect("positive control: the resolution append exists");

    assert!(
        started_append < applier_call,
        "the PatchApplyStarted append must precede the git mutation"
    );
    assert!(
        applier_call < resolved_append,
        "the PatchApplyResolved append must follow the git mutation"
    );
}

/// **AC1 fourth mutant — durable-first, bus-second, proven STRUCTURALLY.**
///
/// ⛔ Not provable behaviourally: `PatchMergeBack.journal` is
/// `Arc<NodeJournal>`, a concrete struct with no injectable seam, so a failing
/// append cannot be produced from a test. Story 18.3a-c recorded the same
/// reason at `conformance_18_3a_c_artifacts.rs:1627-1631`. ⛔ Do not invent a
/// `RoomJournalWriter` port to route around it.
#[test]
fn the_bracketed_region_reaches_the_bus_only_through_the_durable_persist_shell() {
    let merge_back = merge_back_source();
    let body = apply_fn_body(&merge_back);
    for forbidden in ["event_bus", "emit_domain", ".emit("] {
        assert!(
            !body.contains(forbidden),
            "`apply` must not touch the bus directly: the ONE emission lives in \
             `persist`, strictly after `append_room` returns Ok — `{forbidden}`"
        );
    }
    // Positive control: `persist` is what `apply` uses, and it is durable-first.
    //
    // ⚠ 18.3a-f: counted as `.persist(`, not `self.persist(`. rustfmt breaks a
    // long receiver onto its own line (`let resolution = self\n.persist(…)`),
    // so the old needle silently read 1 — meaning `cargo fmt --check` green and
    // this ratchet green were mutually exclusive at `02d6c46`, and cut 2 shipped
    // with fmt red. ⛔ Do not restore the receiver-coupled needle.
    assert_eq!(
        body.matches(".persist(").count(),
        2,
        "the bracket is exactly two durable appends"
    );
    let persist = section(&merge_back, "async fn persist(");
    let append = persist.find(".append_room(").expect("the journal append");
    let emit = persist.find(".emit_domain(").expect("the bus emit");
    assert!(append < emit, "durable-first, bus-second");
}

/// **AC1 second mutant — `#[serde(other)]` on `ApplyOutcome` is mandatory.**
///
/// ⚑ **Fixture shape is load bearing (Trap 5).** `parse_entries` silently drops
/// a *trailing* unparseable line (`Err(_) if is_last => break`), so an
/// unknown-outcome record placed last would make the mutated and un-mutated
/// builds agree. The fixture therefore carries a well-formed record **after**
/// it, and this asserts on the loaded entries — never on `ApplyState`, which
/// would read `Indeterminate` either way.
#[tokio::test]
async fn an_unknown_apply_outcome_from_a_newer_build_does_not_fail_the_journal() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let room_id = OrchestrationRoomId::parse("room-fixture").expect("room id");
    let rooms = workspace.path().join(".rustain").join("rooms");
    std::fs::create_dir_all(&rooms).expect("rooms dir");
    std::fs::copy(
        format!(
            "{}/tests/fixtures/18_3a_d/forward_compat_apply.jsonl",
            env!("CARGO_MANIFEST_DIR")
        ),
        rooms.join(format!("{}.jsonl", room_id.as_str())),
    )
    .expect("vendored fixture is readable");

    let reader = WorkspaceJournalReader::open(workspace.path(), room_id);
    let entries = reader
        .load_entries()
        .await
        .expect("an unknown outcome value must not fail the whole journal load");

    assert_eq!(
        entries.len(),
        3,
        "all three records survive — including the well-formed one that FOLLOWS \
         the unknown-outcome line, which is what makes this fixture discriminate"
    );
    assert!(
        matches!(
            &entries[1].record,
            JournalRecord::Room(RoomEvent::PatchApplyResolved {
                outcome: ApplyOutcome::Unknown,
                ..
            })
        ),
        "the unrecognised outcome degrades to Unknown: {:?}",
        entries[1].record
    );
    assert!(
        matches!(
            &entries[2].record,
            JournalRecord::Room(RoomEvent::PatchCaptured { .. })
        ),
        "the record after the unknown line must not be swallowed: {:?}",
        entries[2].record
    );
}

/// **AC1 — the durable schema did not move.** Two other ratchets pin this; the
/// story that adds two variants owes the reader the number it spent.
#[test]
fn the_room_journal_schema_version_did_not_move() {
    assert_eq!(
        NODE_JOURNAL_SCHEMA_VERSION, 1,
        "additive variants before `Unrecognized` never bump the schema"
    );
}

// ═══════════════════════════════ AC2 ═══════════════════════════════

/// **AC2 keystone (a) — and it reaches the GUARD, not merely the fold.**
///
/// A test that only folds a hand-built journal to `Indeterminate` proves the
/// lattice and *not* the guard — and the guard is why AC2 exists.
///
/// The crash is deterministic and needs no `kill -9`: the decorator mutates the
/// tree for real, signals, then never returns; the driver aborts the task at
/// that await point, so `PatchApplyResolved` is never appended.
///
/// ⛔ **Forbidden bypass:** asserting on a hand-constructed `OrchestrationRoom`
/// instead of one folded from journal entries.
#[tokio::test]
async fn a_crashed_apply_folds_to_indeterminate_and_refuses_the_next_attempt() {
    let harness = Harness::new().await;
    let crashing = Arc::new(JournalWitnessApplier::new(harness.path(), true));
    let entered = crashing.entered.clone();
    let first = Arc::new(harness.service(crashing.clone()));
    let artifact = harness.capture(&first, "old", "new").await;

    let crash = {
        let first = first.clone();
        let artifact = artifact.clone();
        tokio::spawn(async move {
            first
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
    crash.abort();
    assert!(
        crash
            .await
            .expect_err("the crash must not complete")
            .is_cancelled(),
        "the simulated crash must leave the apply unfinished — a completed apply \
         would prove nothing about the window"
    );

    assert_eq!(
        journal_apply_records(&harness.journal).await,
        (1, 0),
        "the crash window is a durable Started with no Resolved"
    );
    assert_eq!(
        harness.tree(),
        "new\n",
        "the crash happened AFTER the tree was really mutated — that is what makes \
         the state indeterminate rather than merely unattempted"
    );
    let room = harness
        .journal
        .project_room(HOST)
        .await
        .expect("project room");
    assert_eq!(
        room.apply_state().get(&artifact.id).copied(),
        Some(ApplyState::Indeterminate),
        "a Started with no Resolved folds to Indeterminate, never to success"
    );

    // ── The guard: a second attempt on the same artifact id. Reachable in
    // production because `patch_artifact_id` is content-addressed over
    // (body ‖ producer ‖ authority), so a post-crash rerun of the same delta by
    // the same subagent under the same token mints the SAME `ArtifactId`.
    let retry_witness = Arc::new(JournalWitnessApplier::new(harness.path(), false));
    let second = harness.service(retry_witness.clone());
    let refusal = second
        .apply(
            &artifact,
            OwnershipKind::Owned,
            PermissionMode::Yolo,
            &shipped_policy(),
            None,
        )
        .await
        .expect_err("an indeterminate artifact must be refused");
    assert!(
        matches!(refusal, MergeBackError::ApplyIndeterminate),
        "the refusal must be the named error: {refusal:?}"
    );
    assert_eq!(
        retry_witness.calls.load(Ordering::SeqCst),
        0,
        "no `git apply` may run — this counter is the mutant detector for \
         deleting the Indeterminate refusal"
    );
    assert_eq!(
        journal_apply_records(&harness.journal).await,
        (1, 0),
        "a refused apply appends nothing"
    );
    assert_eq!(harness.tree(), "new\n", "the working tree is untouched");
}

/// **AC2 positive control — the guard refuses a STATE, not everything.**
///
/// A fresh artifact projects as `NeverAttempted` and applies successfully in
/// the very same workspace whose journal already carries a wedged artifact, so
/// `Indeterminate` is a distinguishable state rather than the only outcome the
/// fold can produce.
#[tokio::test]
async fn a_never_attempted_artifact_still_applies_beside_a_wedged_one() {
    let harness = Harness::new().await;
    let witness = Arc::new(JournalWitnessApplier::new(harness.path(), false));
    let service = harness.service(witness.clone());
    let wedged = harness.capture(&service, "old", "wedged").await;

    // Wedge it durably through the production write-ahead path: the applier
    // fails, then we hand-write nothing — instead we drive a real crash.
    let crashing = Arc::new(JournalWitnessApplier::new(harness.path(), true));
    let entered = crashing.entered.clone();
    let crash_service = Arc::new(harness.service(crashing));
    let crash = {
        let crash_service = crash_service.clone();
        let wedged = wedged.clone();
        tokio::spawn(async move {
            crash_service
                .apply(
                    &wedged,
                    OwnershipKind::Owned,
                    PermissionMode::Yolo,
                    &shipped_policy(),
                    None,
                )
                .await
        })
    };
    entered.notified().await;
    crash.abort();
    let _ = crash.await;

    let fresh = harness.capture(&service, "wedged", "fresh").await;
    service
        .apply(
            &fresh,
            OwnershipKind::Owned,
            PermissionMode::Yolo,
            &shipped_policy(),
            None,
        )
        .await
        .expect("a NeverAttempted artifact must still apply");
    assert_eq!(harness.tree(), "fresh\n");

    let room = harness
        .journal
        .project_room(HOST)
        .await
        .expect("project room");
    assert_eq!(
        room.apply_state().get(&wedged.id).copied(),
        Some(ApplyState::Indeterminate)
    );
    assert_eq!(
        room.apply_state().get(&fresh.id).copied(),
        Some(ApplyState::Resolved(ApplyOutcome::Applied))
    );
}

/// **AC2 third mutant — fail closed on an outcome this build cannot read.**
///
/// `Resolved(Unknown)` means the apply finished but *what it did is unreadable*
/// — the workspace state is exactly as unknown as after a crash. Mapping
/// `Unknown` to `Applied` at the guard lets the retry through and turns this
/// RED.
#[tokio::test]
async fn an_unreadable_outcome_is_never_read_as_success_by_the_guard() {
    let harness = Harness::new().await;
    let witness = Arc::new(JournalWitnessApplier::new(harness.path(), false));
    let service = harness.service(witness.clone());
    let artifact = harness.capture(&service, "old", "new").await;

    // A newer build's resolution: written through the journal, folded through
    // the projection. (The bracket cannot produce this value; a forward-version
    // peer can, and the fixture above proves such a line survives the load.)
    harness
        .journal
        .append_room(RoomEvent::PatchApplyStarted {
            artifact: artifact.id.clone(),
            applier: None,
            workspace_revision: None,
        })
        .await
        .expect("append");
    harness
        .journal
        .append_room(unknown_outcome_resolution(&artifact.id))
        .await
        .expect("append");

    let room = harness
        .journal
        .project_room(HOST)
        .await
        .expect("project room");
    assert_eq!(
        room.apply_state().get(&artifact.id).copied(),
        Some(ApplyState::Resolved(ApplyOutcome::Unknown)),
        "the lattice preserves the unreadable outcome instead of upgrading it"
    );

    let refusal = service
        .apply(
            &artifact,
            OwnershipKind::Owned,
            PermissionMode::Yolo,
            &shipped_policy(),
            None,
        )
        .await
        .expect_err("an unreadable outcome must fail closed");
    assert!(matches!(refusal, MergeBackError::ApplyIndeterminate));
    assert_eq!(witness.calls.load(Ordering::SeqCst), 0);
    assert_eq!(harness.tree(), "old\n");
}

/// Round-trip an `ApplyOutcome` value this build does not know, exactly as a
/// newer peer would have written it.
fn unknown_outcome_resolution(artifact: &ArtifactId) -> RoomEvent {
    let json = format!(
        r#"{{"event":"patch_apply_resolved","artifact":"{artifact}","outcome":"applied_with_rerere"}}"#
    );
    serde_json::from_str(&json).expect("an unknown outcome degrades rather than failing")
}

/// **AC2 — the lattice, folded from a real journal, and last-write-wins.**
///
/// ⚑ *"Folding the same stream twice yields the same map"* is vacuous over a
/// pure constructor. The real property is that a journal carrying a **repeated**
/// Started/Resolved pair for one artifact folds to a single state (fifth
/// mutant: accumulate rather than replace).
#[tokio::test]
async fn the_apply_lattice_preserves_the_outcome_and_replays_last_write_wins() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let journal = NodeJournal::open_workspace(workspace.path())
        .await
        .expect("journal");
    let never = artifact_id(0xa0);
    let indeterminate = artifact_id(0xb0);
    let conflicted = artifact_id(0xc0);
    let repeated = artifact_id(0xd0);

    for event in [
        RoomEvent::PatchApplyStarted {
            artifact: indeterminate.clone(),
            applier: None,
            workspace_revision: Some("cafebabe".to_owned()),
        },
        RoomEvent::PatchApplyStarted {
            artifact: conflicted.clone(),
            applier: None,
            workspace_revision: None,
        },
        RoomEvent::PatchApplyResolved {
            artifact: conflicted.clone(),
            outcome: ApplyOutcome::Conflict,
        },
        RoomEvent::PatchApplyStarted {
            artifact: repeated.clone(),
            applier: None,
            workspace_revision: None,
        },
        RoomEvent::PatchApplyResolved {
            artifact: repeated.clone(),
            outcome: ApplyOutcome::Failed,
        },
        RoomEvent::PatchApplyStarted {
            artifact: repeated.clone(),
            applier: None,
            workspace_revision: None,
        },
        RoomEvent::PatchApplyResolved {
            artifact: repeated.clone(),
            outcome: ApplyOutcome::Applied,
        },
    ] {
        journal.append_room(event).await.expect("append");
    }

    let room = journal.project_room(HOST).await.expect("project room");
    let state = |id: &ArtifactId| room.apply_state().get(id).copied().unwrap_or_default();

    assert_eq!(state(&never), ApplyState::NeverAttempted);
    assert_eq!(state(&indeterminate), ApplyState::Indeterminate);
    assert_eq!(
        state(&conflicted),
        ApplyState::Resolved(ApplyOutcome::Conflict),
        "Conflict must not collapse into Failed — 18-3a-e's row vocabulary needs it"
    );
    assert_eq!(
        state(&repeated),
        ApplyState::Resolved(ApplyOutcome::Applied),
        "a repeated pair folds last-write-wins to ONE state, never accumulating"
    );
    assert_eq!(
        room.apply_state().len(),
        3,
        "one entry per artifact that has an apply record"
    );
}

/// **AC2 fourth mutant (behavioural half) — the fold is unconditional.**
///
/// An apply record for an artifact this projection has never seen must stay
/// visible. Nest the arms inside `artifacts.get_mut` and the state vanishes.
#[tokio::test]
async fn an_apply_record_for_an_unknown_artifact_stays_visible() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let journal = NodeJournal::open_workspace(workspace.path())
        .await
        .expect("journal");
    let orphan = artifact_id(0xe0);
    journal
        .append_room(RoomEvent::PatchApplyStarted {
            artifact: orphan.clone(),
            applier: None,
            workspace_revision: None,
        })
        .await
        .expect("append");

    let room = journal.project_room(HOST).await.expect("project room");
    assert!(
        room.artifacts().get(&orphan).is_none(),
        "precondition: the artifact is absent from the artifact fold"
    );
    assert_eq!(
        room.apply_state().get(&orphan).copied(),
        Some(ApplyState::Indeterminate),
        "an apply record for an unknown artifact is an anomaly and must stay \
         visible, not be silently swallowed the way PatchReviewed's guard does"
    );
}

/// **AC2 keystone (b) — structural (Rule 4), two assertions.**
#[test]
fn the_apply_fold_arms_are_unnested_and_the_guard_precedes_the_mutation() {
    let room = source("src/domain/models/orchestration_room.rs");
    // ⚠ 18.3a-f: this list is HARDCODED — a new apply fold arm ships unpoliced
    // unless it is added here. The `rfind` below needs exactly 12 spaces of
    // indentation before `RoomEvent::`.
    for variant in [
        "RoomEvent::PatchApplyStarted",
        "RoomEvent::PatchApplyResolved",
        "RoomEvent::PatchApplyInspected",
    ] {
        let arm_start = room
            .rfind(&format!("            {variant} {{"))
            .unwrap_or_else(|| panic!("positive control: the {variant} fold arm exists"));
        let arm = &room[arm_start..];
        // ⚠ 18.3a-f: bound at the first 12-space `}` that is ALONE on its line.
        // A three-field destructure makes rustfmt wrap the pattern, whose
        // closer is `            } => {` — the old `"\n            }"` needle
        // matched that and truncated the slice before the arm body, failing the
        // positive control below on a perfectly correct arm.
        let arm = &arm[..arm.find("\n            }\n").expect("the arm closes")];
        assert!(
            !arm.contains("artifacts.get_mut"),
            "{variant} must fold unconditionally into its own map"
        );
        assert!(
            arm.contains("self.apply_state") && arm.contains(".insert("),
            "positive control: {variant} writes the apply map"
        );
    }

    let merge_back = merge_back_source();
    let body = apply_fn_body(&merge_back);
    let guard = body
        .find("room.apply_state()")
        .expect("positive control: the guard reads the projection");
    let applier_call = body
        .find(".apply(&self.workspace")
        .expect("positive control: the applier call site exists");
    assert!(
        guard < applier_call,
        "the apply-state guard must precede the git mutation"
    );
}

fn artifact_id(byte: u8) -> ArtifactId {
    ArtifactId::from(ContentHash::parse_hex(&format!("{byte:02x}").repeat(32)).expect("hex"))
}

// ═══════════════════════════════ AC3 ═══════════════════════════════

/// **AC3 keystone (a) — behavioural, two independently constructed services.**
///
/// 🔴 **Why a same-process test proves a cross-process claim.** POSIX `flock`
/// locks are held on the **open file description**, not the process: two
/// separate `open()` calls create two descriptions, so the second
/// `LOCK_EX | LOCK_NB` fails with `EWOULDBLOCK` even inside one process —
/// the same outcome a second OS process sees.
///
/// ⛔ This does **not** hold for `fcntl`/POSIX record locks, which are
/// per-*process*: under `fcntl` the second acquire succeeds, both applies
/// proceed, and this keystone correctly goes RED. The test therefore
/// discriminates *for `flock`* and would catch an `fcntl` swap. ⛔ Never
/// substitute `fcntl`, `fs2` or `fd-lock`.
///
/// ⚑ Stated as an inference from POSIX semantics, not as a proof of
/// cross-process behaviour. Deliberately preferred over spawning a real
/// subprocess: a subprocess test here is CI-flaky, and a flaky gate gets
/// `#[ignore]`d within an epic — which is how the invariant dies quietly.
///
/// ⛔ **Forbidden bypass:** asserting the lock **file exists**. Existence is
/// not exclusion; the loser must be refused.
#[tokio::test]
async fn a_second_service_over_one_workspace_is_refused_while_the_first_applies() {
    let harness = Harness::new().await;
    let holder_applier = Arc::new(JournalWitnessApplier::new(harness.path(), true));
    let entered = holder_applier.entered.clone();
    let holder = Arc::new(harness.service(holder_applier));
    let first = harness.capture(&holder, "old", "first").await;
    let second = harness.capture(&holder, "old", "second").await;

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

    let loser_applier = Arc::new(JournalWitnessApplier::new(harness.path(), false));
    let loser = harness.service(loser_applier.clone());
    let refusal = loser
        .apply(
            &second,
            OwnershipKind::Owned,
            PermissionMode::Yolo,
            &shipped_policy(),
            None,
        )
        .await
        .expect_err("the second holder of the workspace must be refused");
    assert!(
        matches!(refusal, MergeBackError::WorkspaceBusy),
        "the loser must get the named busy error, never Ok: {refusal:?}"
    );
    assert_eq!(
        loser_applier.calls.load(Ordering::SeqCst),
        0,
        "the loser must not reach `git apply`"
    );
    assert_eq!(
        journal_apply_records(&harness.journal).await,
        (1, 0),
        "the loser leaves no partial durable record: exactly one Started, the winner's"
    );

    held.abort();
    let _ = held.await;
}

/// **AC3 fifth mutant — the lock is keyed by the CANONICAL workspace.**
///
/// Two spellings of one repository must take the *same* lock. Key it on the raw
/// path you were handed and both applies proceed. Without this, "canonical" is
/// decoration.
#[tokio::test]
async fn two_spellings_of_one_workspace_contend_for_the_same_lock() {
    let harness = Harness::new().await;
    let root = tempfile::tempdir().expect("tempdir");
    let alias = root.path().join("alias");
    std::os::unix::fs::symlink(harness.path(), &alias).expect("symlink");

    let holder_applier = Arc::new(JournalWitnessApplier::new(harness.path(), true));
    let entered = holder_applier.entered.clone();
    let holder = Arc::new(harness.service(holder_applier));
    let first = harness.capture(&holder, "old", "first").await;
    let second = harness.capture(&holder, "old", "second").await;

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

    let aliased_applier = Arc::new(JournalWitnessApplier::new(harness.path(), false));
    let aliased = harness.service_at(&alias, aliased_applier.clone());
    let refusal = aliased
        .apply(
            &second,
            OwnershipKind::Owned,
            PermissionMode::Yolo,
            &shipped_policy(),
            None,
        )
        .await
        .expect_err("a symlinked spelling addresses the same tree and must contend");
    assert!(matches!(refusal, MergeBackError::WorkspaceBusy));
    assert_eq!(aliased_applier.calls.load(Ordering::SeqCst), 0);

    held.abort();
    let _ = held.await;
}

/// **AC3 — the acquisition-ORDER ratchet, plus its positive control.**
///
/// Exactly-once says nothing about *before the gate*. A gate evaluated on a
/// projection read before the lock is held is the double-apply the lock exists
/// to prevent.
#[test]
fn the_workspace_lock_is_acquired_before_every_gate_in_source() {
    let merge_back = merge_back_source();
    let body = apply_fn_body(&merge_back);
    let acquire = body
        .find("self.acquire_workspace_lock()")
        .expect("positive control: the lock acquisition exists");
    let projection = body
        .find("self.journal.project_room(")
        .expect("positive control: the projection read exists");
    let guard = body
        .find("room.apply_state()")
        .expect("positive control: the apply-state guard exists");
    let gate = body
        .find("may_apply_patch(")
        .expect("positive control: the review gate exists");

    assert!(
        acquire < projection,
        "the projection must be re-read AFTER the lock is held: a peer may have \
         applied while this call was contending"
    );
    assert!(acquire < guard, "the apply-state guard runs under the lock");
    assert!(acquire < gate, "the review gate runs under the lock");
}

/// **AC3 third mutant — exactly-one acquisition per apply (Rule 4).**
///
/// ⛔ A two-acquisition mutant ("release before the mutation, re-acquire
/// after") is not reliably raceable, so it is proven with a deterministic
/// counter, never a timing window. The source half runs in both lanes; the
/// runtime counter needs `test-instrumentation`, which
/// `the_ci_a2a_lane_runs_this_target_with_instrumentation` keeps honest.
#[test]
fn exactly_one_file_lock_acquisition_site_exists_in_the_apply_path() {
    let merge_back = merge_back_source();
    assert_eq!(
        merge_back.matches("ApplyLock::try_acquire(").count(),
        1,
        "one acquisition site, so the instrumented counter cannot be bypassed"
    );
    assert_eq!(
        apply_fn_body(&merge_back)
            .matches("self.acquire_workspace_lock()")
            .count(),
        1,
        "`apply` reaches for the lock exactly once"
    );
}

#[cfg(feature = "test-instrumentation")]
#[tokio::test]
async fn one_apply_takes_the_workspace_lock_exactly_once() {
    let harness = Harness::new().await;
    let witness = Arc::new(JournalWitnessApplier::new(harness.path(), false));
    let service = harness.service(witness.clone());
    let artifact = harness.capture(&service, "old", "new").await;
    assert_eq!(service.lock_acquisitions(), 0);
    service
        .apply(
            &artifact,
            OwnershipKind::Owned,
            PermissionMode::Yolo,
            &shipped_policy(),
            None,
        )
        .await
        .expect("apply");
    assert_eq!(
        service.lock_acquisitions(),
        1,
        "a release-and-reacquire mutant reads 2"
    );
}

// ═══════════════════════════════ AC4 ═══════════════════════════════

/// **AC4 — the new target actually runs, in BOTH lanes.**
///
/// ⚠ A conformance file does not run unless a line names it.
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
        default_lane.contains("cargo test --test conformance_18_3a_d_apply"),
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
        a2a_lane.contains("--test conformance_18_3a_d_apply"),
        "the A2A lane must execute this story target"
    );
    assert!(
        !concat!(file!(), "").contains("a2a"),
        "this filename must not contain `a2a`"
    );
}

/// **AC4 — `conformance_cow_mergeback` is this story's positive control, so it
/// must actually run in CI.**
///
/// 🔴 It ran in **no** workflow before this story: both lanes use explicit
/// `--test <name>` lists and neither named it. Relying on an unenforced file as
/// a gate is exactly the false-green shape this epic keeps catching. It is
/// `#![cfg(unix)]`, so the Linux a2a lane is the right home.
#[test]
fn ci_executes_the_cow_mergeback_positive_control() {
    let ci = source(".github/workflows/ci.yml");
    let a2a_lane = ci
        .split("\n  a2a:\n")
        .nth(1)
        .expect("A2A lane exists")
        .split("\n  mcp:\n")
        .next()
        .expect("A2A lane is bounded");
    assert!(
        a2a_lane.contains("--test conformance_cow_mergeback"),
        "the merge-back positive control must execute in CI, not just locally"
    );
}

/// **AC4 — the instrumented half of AC3 is not silently absent.**
#[test]
fn the_ci_a2a_lane_runs_this_target_with_instrumentation() {
    let ci = source(".github/workflows/ci.yml");
    let a2a_lane = ci
        .split("\n  a2a:\n")
        .nth(1)
        .expect("A2A lane exists")
        .split("\n  mcp:\n")
        .next()
        .expect("A2A lane is bounded");
    assert!(
        a2a_lane.contains("--features a2a,test-instrumentation"),
        "without `test-instrumentation` the exactly-once lock counter degrades to \
         its source half in every lane"
    );
}

/// **18.3a-e amendment:** the operator surface reaches the existing durable
/// latch through one port and does not mint a second apply implementation.
#[test]
fn operator_surface_routes_to_the_existing_apply_latch() {
    let bridge = source("src/infrastructure/runtime/artifact_bridge.rs");
    let apply = &bridge[bridge
        .find("pub async fn apply_artifact(")
        .expect("apply seam")..];
    let gate = apply
        .find("room_edit_decision(local_room_role(acting), RoomEditKind::DurableContent)")
        .expect("room edit gate");
    let port = apply.find(".apply_patch(").expect("single apply port");
    assert!(gate < port, "the gate precedes the workspace-write port");
    assert_eq!(bridge.matches(".apply_patch(").count(), 1);
    assert!(
        !bridge.contains("RoomEvent::PatchApplyStarted"),
        "the bridge delegates to the existing latch; it does not mint apply events"
    );

    let handler = source("src/adapters/tui/handlers/artifact_command.rs");
    assert!(handler.contains("Some(\"apply\")"), "parser branch");
    let loop_source = source("src/infrastructure/runtime/event_loop.rs");
    for needle in [
        "InputAction::ApplyCardAccept",
        "InputAction::ApplyCardDecline",
        "render_apply_card_lines",
    ] {
        assert!(loop_source.contains(needle), "{needle}");
    }
    assert!(loop_source.lines().count() <= 11_321);

    let merge_back = source("src/infrastructure/orchestrator/merge_back.rs");
    assert_eq!(
        merge_back.matches("pub async fn apply(").count(),
        1,
        "no second merge-back latch"
    );
    let a2a = source("src/adapters/a2a/projection.rs");
    assert!(
        !a2a.contains("PatchApplyExecutor") && !a2a.contains("ArtifactCommandArgs::Apply"),
        "18.4 peer projection remains deferred"
    );
}

/// Body of `PatchMergeBack::apply`, bounded at the next method.
fn apply_fn_body(merge_back: &str) -> &str {
    section(merge_back, "pub async fn apply(")
}

fn section<'a>(source: &'a str, opener: &str) -> &'a str {
    let start = source
        .find(opener)
        .unwrap_or_else(|| panic!("positive control: `{opener}` exists"));
    let rest = &source[start..];
    &rest[..rest.find("\n    }\n").expect("the function closes")]
}
