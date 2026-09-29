use std::path::PathBuf;
use std::sync::Arc;

use crate::domain::events::AppEvent;
use crate::domain::models::{
    AgentId, ApplyOutcome, ApplyState, ArtifactId, ArtifactKind, ArtifactRef, CapabilityTokenId,
    EvidenceArtifactDraft, HostBinding, OperatorApplyFinding, OrchestrationRoom, OwnershipKind,
    PermissionMode, ProvenanceTag, ReviewStatus, ReviewVerdict, RoomEvent, UnifiedDiff,
};
use crate::domain::ports::{
    ArtifactError, ArtifactStore, PatchApplier, PatchApplyError, PatchApplyExecutor,
    PatchApplyPortError, PatchApplyResolver, PatchResolvePortError,
};
use crate::domain::services::patch_review::{ApplyDecision, MergeBackPolicy, may_apply_patch};
use crate::infrastructure::apply_lock::{ApplyLock, ApplyLockError};
use crate::infrastructure::runtime::event_bus::EventBus;
use crate::infrastructure::subagent::{JournalError, NodeJournal};

/// Durable capture, review, and serialized application of isolated-child
/// patches into the One-Ring workspace.
///
/// The git mutation mechanism is owned by the injected [`PatchApplier`] port
/// (concrete `GitPatchApplier` composed at the startup root); this service owns
/// the use-case: capture → review → authorize → apply.
pub struct PatchMergeBack {
    workspace: PathBuf,
    store: Arc<dyn ArtifactStore>,
    journal: Arc<NodeJournal>,
    event_bus: Arc<EventBus>,
    applier: Arc<dyn PatchApplier>,
    apply_guard: tokio::sync::Mutex<()>,
    /// Deterministic proof of the exactly-once file-lock property (Rule 4).
    /// A "release before the mutation and re-acquire after" mutant reads 2; a
    /// two-acquisition race cannot be turned RED reliably by timing, so it is
    /// never proven that way.
    #[cfg(any(test, feature = "test-instrumentation"))]
    lock_acquisitions: std::sync::atomic::AtomicUsize,
}

impl PatchMergeBack {
    pub fn new(
        workspace: PathBuf,
        store: Arc<dyn ArtifactStore>,
        journal: Arc<NodeJournal>,
        event_bus: Arc<EventBus>,
        applier: Arc<dyn PatchApplier>,
    ) -> Self {
        Self {
            workspace,
            store,
            journal,
            event_bus,
            applier,
            apply_guard: tokio::sync::Mutex::new(()),
            #[cfg(any(test, feature = "test-instrumentation"))]
            lock_acquisitions: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Promote one captured CoW delta into the content-addressed artifact graph.
    pub async fn capture(
        &self,
        producer: AgentId,
        authority: CapabilityTokenId,
        provenance: Vec<ProvenanceTag>,
        depends_on: Vec<ArtifactId>,
        host: HostBinding,
        diff: &UnifiedDiff,
    ) -> Result<ArtifactRef, MergeBackError> {
        if diff.is_empty() {
            return Err(MergeBackError::EmptyPatch);
        }
        // F10 — detect binary content and fail closed to review. Match only
        // git's binary marker LINES (header-level), never a literal phrase
        // appearing inside a text hunk, so a text patch is never false-refused.
        if diff
            .diff
            .lines()
            .any(|line| line.starts_with("GIT binary patch") || line.starts_with("Binary files "))
        {
            return Err(MergeBackError::BinaryPatch);
        }
        let artifact = self
            .store
            .put(
                EvidenceArtifactDraft {
                    kind: ArtifactKind::Patch,
                    producer: producer.clone(),
                    authority,
                    provenance,
                    depends_on,
                    review: Some(ReviewStatus::Pending),
                    host,
                },
                diff.diff.as_bytes(),
            )
            .await?;
        self.persist(RoomEvent::ArtifactCreated {
            artifact: artifact.clone(),
        })
        .await?;
        self.persist(RoomEvent::PatchCaptured {
            artifact: artifact.id.clone(),
            producer,
        })
        .await?;
        Ok(artifact)
    }

    /// Record an explicit review verdict. Artifact bodies remain immutable; the
    /// canonical room journal owns the review-state transition.
    pub async fn review(
        &self,
        artifact: ArtifactRef,
        reviewer: AgentId,
        verdict: ReviewVerdict,
    ) -> Result<ArtifactRef, MergeBackError> {
        let stored = self.store.head(&artifact.id).await?;
        ensure_same_stored_artifact(&stored, &artifact)?;
        if artifact.kind != ArtifactKind::Patch {
            return Err(MergeBackError::NotPatch);
        }
        self.persist(RoomEvent::PatchReviewed {
            artifact: artifact.id.clone(),
            reviewer: reviewer.clone(),
            verdict,
        })
        .await?;
        let mut reviewed = artifact;
        reviewed.review = Some(ReviewStatus::Reviewed { reviewer, verdict });
        Ok(reviewed)
    }

    /// Apply a reviewed patch under one async critical section, bracketed by a
    /// durable write-ahead record.
    ///
    /// Authorization is resolved from the **canonical journal projection**, not
    /// the caller-supplied artifact handle: the `review` field on a public
    /// `ArtifactRef` is forgeable, so `apply` re-derives the authoritative
    /// review state from the single writable room journal before consulting the
    /// pure `may_apply_patch` gate. The git mutation is delegated to the
    /// [`PatchApplier`] port; conflicts leave the workspace unchanged.
    ///
    /// # The bracket (Story 18.3a-d — `DF-17-3b-1`, FR160(c), NFR70(c))
    ///
    /// [`RoomEvent::PatchApplyStarted`] is appended **and fsynced before** the
    /// port is invoked; [`RoomEvent::PatchApplyResolved`] carries the outcome
    /// after it returns, on the success and the failure path alike. A crash in
    /// between leaves a `Started` with no `Resolved`, which folds to
    /// [`ApplyState::Indeterminate`] — and the guard below **refuses** that
    /// artifact on every later attempt. Those are the two halves `DF-17-3b-1`
    /// names: no durable record *and no idempotent retry semantics*.
    ///
    /// ⚠ The record is a **crash-recovery marker under a trusted-filesystem
    /// assumption**, never an audit trail: the room journal is
    /// Landlock-enforced but unauthenticated
    /// (`DF-18-2-AUTHENTICATED-JOURNAL`), so whoever can write it can forge a
    /// `Resolved{Applied}` and suppress this reconcile.
    ///
    /// # Two locks, and why the projection is read after both
    ///
    /// `apply_guard` serializes applies inside this process; [`ApplyLock`]
    /// serializes them across processes (`DF-17-3b-2`). Both are taken
    /// **before** the authorization gate and the apply-state guard, and the
    /// journal projection is folded **after** they are held: a peer may have
    /// applied while this call was contending, and a gate evaluated against a
    /// stale projection is exactly the double-apply the lock exists to
    /// prevent. ⛔ Never release and re-acquire around the checks.
    pub async fn apply(
        &self,
        artifact: &ArtifactRef,
        ownership: OwnershipKind,
        permission_mode: PermissionMode,
        policy: &MergeBackPolicy,
        applier: Option<AgentId>,
    ) -> Result<(), MergeBackError> {
        let _guard = self.apply_guard.lock().await;
        let _workspace_lock = self.acquire_workspace_lock().await?;
        let stored = self.store.head(&artifact.id).await?;
        ensure_same_stored_artifact(&stored, artifact)?;
        // ONE journal projection serves BOTH gates. `authoritative_artifact`
        // used to fold the room itself; the apply-state guard reuses that fold
        // instead of paying a second journal read.
        let room = self.journal.project_room(&stored.host.host_id).await?;
        // A prior apply of this exact artifact died between the write-ahead
        // record and its resolution, so whether the working tree carries the
        // delta is unknown. Refuse: re-running `git apply` over an unknown
        // preimage is the silent divergence this record exists to prevent, and
        // "unknown" is the honest answer — this projection reports what was
        // recorded and never repairs what was not (`ADR-18-3a-d-01:72`,
        // applying AD-12's "Live state is never reconstructed from journal
        // data", `ARCHITECTURE-SPINE.md:159`), so this call neither probes the
        // tree nor retries.
        //
        // Reachable on the ordinary retry path, not just after operator
        // action: `patch_artifact_id` is
        // SHA256("rustain.patch.v1" ‖ body ‖ producer ‖ authority), so a
        // post-crash rerun of the same delta by the same subagent under the
        // same token mints the SAME `ArtifactId` — now carrying
        // `Indeterminate`.
        if apply_is_refused(
            room.apply_state()
                .get(&stored.id)
                .copied()
                .unwrap_or_default(),
        ) {
            return Err(MergeBackError::ApplyIndeterminate);
        }
        // P1: the authoritative review state comes from the journal projection,
        // never the caller-supplied (forgeable) `review` field.
        let authoritative = authoritative_artifact(&room, &stored)?;
        if may_apply_patch(&authoritative, ownership, permission_mode, policy)
            != ApplyDecision::Apply
        {
            return Err(MergeBackError::ReviewRequired);
        }
        let body = self.store.get(&artifact.id).await?;
        validate_patch_body(&body)?;

        // Write-ahead: durable BEFORE the side effect is observable
        // (`ADR-17-2c-01` §2). `persist` awaits an fsynced `append_room`, so a
        // failed append returns `Err`, emits nothing on the bus and — the point
        // of the whole story — never reaches `git apply`.
        let workspace_revision = self.applier.revision(&self.workspace).await;
        self.persist(RoomEvent::PatchApplyStarted {
            artifact: stored.id.clone(),
            applier,
            workspace_revision,
        })
        .await?;
        let result = self
            .applier
            .apply(&self.workspace, &body)
            .await
            .map_err(map_apply_error);
        // Second leg of the bracket: the resolution is durable-best-effort.
        // If this append fails AFTER `git apply` already returned, the apply
        // did not durably complete — the workspace may carry the delta while
        // the artifact folds to `Indeterminate` and is refused on every later
        // attempt (cut 1 ships the latch with no release). 18.3a-e surfaces
        // this distinctly via `ApplyUnresolved` rather than a plain failure:
        // swallowing the error and returning `result` would leave the room
        // believing `Indeterminate` while the caller believes success — a
        // silent inconsistency strictly worse than a reported failure.
        // (ADR-18-3a-d-01 Decision 1; the operator-facing distinction is
        // 18.3a-e's honesty refinement.)
        let resolution = self
            .persist(RoomEvent::PatchApplyResolved {
                artifact: stored.id.clone(),
                outcome: apply_outcome(&result),
            })
            .await;
        if let Err(resolution_err) = resolution {
            return Err(MergeBackError::ApplyUnresolved(resolution_err.to_string()));
        }
        result
    }

    /// Record what an operator reported after inspecting the working tree
    /// themselves, releasing an indeterminate apply (Story 18.3a-f, FR160(d)).
    ///
    /// # ⛔ It is a separate function, and that is load bearing
    ///
    /// The append does **not** live inside [`Self::apply`]: that method's
    /// bracket is exactly two durable appends and a structural ratchet pins the
    /// count, and its body is sliced by needle-based conformance assertions
    /// that a nested block would truncate. It is also semantically separate —
    /// this call performs **no** workspace write at all.
    ///
    /// # The same two locks, in the same order, and the projection re-read
    /// under them
    ///
    /// Inherited verbatim from `ADR-18-3a-d-01` D4: *"Acquire before the gates,
    /// and re-read the projection after acquiring … ⛔ Never release and
    /// re-acquire around the checks."* A peer may have resolved this artifact
    /// while this call was blocked on the lock, and a gate evaluated against a
    /// stale projection is the double-write the lock exists to prevent.
    ///
    /// # 🔴 Only what is actually wedged may be resolved
    ///
    /// The fold is last-write-wins. Without this gate an operator could write a
    /// finding over `Resolved(Applied)` and their **report would override a
    /// real machine outcome** — fail-open through the side door, and the single
    /// most likely way this capability ships a defect (ruling A5). Every
    /// non-`Indeterminate` state is refused by name, `NeverAttempted` included.
    ///
    /// ⛔ **No probe.** The decision reads the projection and the operator's
    /// stated finding. It does not call `git status`, `git diff`,
    /// `git rev-parse`, or stat a file (AD-12, `ARCHITECTURE-SPINE.md:159`;
    /// `ADR-18-3a-d-01:72`). ⛔ It does not build on `workspace_revision`
    /// either — that field is best effort and explicitly not load bearing.
    ///
    /// ⛔ **No chained apply.** Resolution unwedges; the operator then runs
    /// `/artifact apply` themselves if they want it.
    pub async fn record_inspection(
        &self,
        artifact: &ArtifactRef,
        finding: OperatorApplyFinding,
        inspector: AgentId,
    ) -> Result<(), MergeBackError> {
        let _guard = self.apply_guard.lock().await;
        let _workspace_lock = self.acquire_workspace_lock().await?;
        let stored = self.store.head(&artifact.id).await?;
        ensure_same_stored_artifact(&stored, artifact)?;
        let room = self.journal.project_room(&stored.host.host_id).await?;
        // Read totally, exactly as the guard above does — an artifact absent
        // from the map is `NeverAttempted`, which is refused here.
        let state = room
            .apply_state()
            .get(&stored.id)
            .copied()
            .unwrap_or_default();
        if state != ApplyState::Indeterminate {
            return Err(MergeBackError::ApplyNotWedged(format!("{state:?}")));
        }
        // 🔴 A report the operator cannot file must never be minted (ruling A4 /
        // DF-18-3a-f-UNREADABLE-VALUE-LATCH). `Unknown` is deserialize-only
        // forward-compat: the parser never produces it, so this fires only for a
        // caller that reaches the seam directly with an unreadable value.
        // Refusing here keeps the verb from creating an `OperatorResolved(Unknown)`
        // this build would then refuse to release.
        if matches!(finding, OperatorApplyFinding::Unknown) {
            return Err(MergeBackError::UnreadableFinding);
        }
        // Durable-first, bus-second through the same shell every other record
        // uses: a failed append returns `Err` and emits nothing
        // (`ADR-18-3a-d-01:41`). ⛔ Never swallow this error and return `Ok` —
        // that tells the operator "resolved" while the room still folds
        // `Indeterminate`, which is the defect cut 2's review found and fixed
        // with `ApplyUnresolved`.
        self.persist(RoomEvent::PatchApplyInspected {
            artifact: stored.id.clone(),
            finding,
            inspector,
        })
        .await
    }

    /// Layer 2 of the apply serialization: an OS advisory lock keyed by the
    /// **canonicalized** workspace (`DF-17-3b-2`, ruling A6).
    ///
    /// ⚑ The sole [`ApplyLock::try_acquire`] call site in this file, so the
    /// cfg-gated counter cannot be bypassed by a second acquisition.
    async fn acquire_workspace_lock(&self) -> Result<ApplyLock, MergeBackError> {
        #[cfg(any(test, feature = "test-instrumentation"))]
        self.lock_acquisitions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        ApplyLock::try_acquire(&self.workspace)
            .await
            .map_err(|error| match error {
                ApplyLockError::Busy => MergeBackError::WorkspaceBusy,
                ApplyLockError::Io(source) => MergeBackError::ApplyLockIo(source.to_string()),
            })
    }

    /// How many times this service has reached for the workspace apply lock.
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub fn lock_acquisitions(&self) -> usize {
        self.lock_acquisitions
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Owner-authorized review-and-apply use case (D2): record the verdict on
    /// the canonical journal, then apply through the journal-authoritative gate.
    /// This is the single end-to-end entry point for merging an isolated child's
    /// patch into the One-Ring workspace — the composition-root wiring target
    /// for a future Owner-facing command (the CLI/TUI render is a follow-up).
    pub async fn review_and_apply(
        &self,
        artifact: ArtifactRef,
        reviewer: AgentId,
        verdict: ReviewVerdict,
        ownership: OwnershipKind,
        permission_mode: PermissionMode,
        policy: &MergeBackPolicy,
    ) -> Result<(), MergeBackError> {
        let applier = reviewer.clone();
        let reviewed = self.review(artifact, reviewer, verdict).await?;
        self.apply(&reviewed, ownership, permission_mode, policy, Some(applier))
            .await
    }

    async fn persist(&self, event: RoomEvent) -> Result<(), MergeBackError> {
        self.journal.append_room(event.clone()).await?;
        let _ = self
            .event_bus
            .emit_domain(AppEvent::DomainEvent(event.into()));
        Ok(())
    }
}

#[async_trait::async_trait]
impl PatchApplyExecutor for PatchMergeBack {
    async fn apply_patch(
        &self,
        artifact: ArtifactRef,
        ownership: OwnershipKind,
        permission_mode: PermissionMode,
        policy: MergeBackPolicy,
        applier: Option<AgentId>,
    ) -> Result<(), PatchApplyPortError> {
        self.apply(&artifact, ownership, permission_mode, &policy, applier)
            .await
            .map_err(|error| match error {
                MergeBackError::WorkspaceBusy => PatchApplyPortError::WorkspaceBusy,
                MergeBackError::ApplyIndeterminate => PatchApplyPortError::ApplyIndeterminate,
                MergeBackError::ApplyUnresolved(message) => {
                    PatchApplyPortError::ApplyUnresolved(message)
                }
                MergeBackError::Conflict(message) => PatchApplyPortError::Conflict(message),
                other => PatchApplyPortError::Failed(other.to_string()),
            })
    }
}

/// ⛔ A **sibling** implementation, never a widening of [`PatchApplyExecutor`]
/// (`ADR-11-3` rule 3, `architecture.md:1220`). One service owns both use
/// cases because both need the same two locks and the same journal; the two
/// *seams* stay distinct because one mutates the workspace and the other
/// provably does not.
#[async_trait::async_trait]
impl PatchApplyResolver for PatchMergeBack {
    async fn record_operator_inspection(
        &self,
        artifact: ArtifactRef,
        finding: OperatorApplyFinding,
        inspector: AgentId,
    ) -> Result<(), PatchResolvePortError> {
        self.record_inspection(&artifact, finding, inspector)
            .await
            .map_err(|error| match error {
                MergeBackError::WorkspaceBusy => PatchResolvePortError::WorkspaceBusy,
                MergeBackError::UnreadableFinding => PatchResolvePortError::UnreadableFinding,
                other => PatchResolvePortError::RecordFailed(other.to_string()),
            })
    }
}

/// Minimal git-diff signature gate. Accepts mode/rename/submodule patches
/// (which carry a `diff --git` header but may have no `@@` hunk). `git apply`
/// (via the `PatchApplier` port) is the authoritative malformed-vs-conflict
/// classifier (F12) after this cheap pre-check.
fn validate_patch_body(body: &[u8]) -> Result<(), MergeBackError> {
    let text = std::str::from_utf8(body).map_err(|_| MergeBackError::MalformedPatch)?;
    if text.lines().any(|line| line.starts_with("diff --git ")) {
        Ok(())
    } else {
        Err(MergeBackError::MalformedPatch)
    }
}

fn map_apply_error(error: PatchApplyError) -> MergeBackError {
    match error {
        PatchApplyError::Malformed => MergeBackError::MalformedPatch,
        PatchApplyError::Conflict(diagnostic) => MergeBackError::Conflict(diagnostic),
        PatchApplyError::Io(message) => MergeBackError::GitIo(message),
    }
}

/// Resolve the canonical, journal-derived view of a stored patch artifact.
/// The projected `review` state reflects `PatchReviewed` events actually
/// appended to the room journal — the only legitimate review transition.
///
/// Takes the already-folded room so `apply` reads the journal exactly once.
fn authoritative_artifact(
    room: &OrchestrationRoom,
    stored: &ArtifactRef,
) -> Result<ArtifactRef, MergeBackError> {
    room.artifacts()
        .get(&stored.id)
        .cloned()
        .ok_or(MergeBackError::ReviewRequired)
}

/// Does the projected apply state of an artifact forbid another attempt?
///
/// 🔴 **Fail closed on anything that means "the workspace state is unknown".**
/// Three shapes qualify and only the first is obvious:
///
/// * [`ApplyState::Indeterminate`] — the process died between the write-ahead
///   record and its resolution.
/// * `Resolved(`[`ApplyOutcome::Unknown`]`)` — a newer build recorded an
///   outcome value this one cannot read, so *what it did* is unreadable. ⛔ An
///   unrecognized outcome is NEVER read as success, and never as a safe
///   failure either.
/// * `OperatorResolved(`[`OperatorApplyFinding::Unknown`]`)` — the same shape
///   one level down: a newer build recorded a *finding* this one cannot read.
///
/// A *readable* machine resolution — `Applied`, `Conflict`, `Failed` — permits
/// a retry: the outcome is on record, so the next attempt has something honest
/// to classify against. ⛔ Do **not** justify this with *"`git apply` is atomic
/// across hunks, so a re-apply is bounded"* — that inference is false and was
/// demonstrated false on 2026-08-08: a new-file hunk re-applies with `exit=0`
/// onto a tree where the file was deleted by hand. (The narrow claim on
/// [`ApplyOutcome::Conflict`] — atomic across hunks, `--reject` never requested
/// — is true and is a different statement.)
///
/// # Why both unreadable values stay refused, and it is not "protecting a record"
///
/// The journal is append-only: releasing them would overwrite nothing, and
/// `ADR-18-3a-c-01` D6 already blessed the identical shape for verdicts
/// (re-review is permitted, the latest governs, both survive in the log). The
/// reason that actually holds is **consent**. For `Indeterminate` the
/// confirmation card can honestly say *"no outcome was recorded"*; for an
/// unreadable value it would have to say *"an outcome was recorded and this
/// build cannot read it"* — and then ask the operator to supersede a fact it
/// cannot name. ⛔ Do not write "it would overwrite a real machine record":
/// that rationale is false and was struck at 18.3a-f preflight.
///
/// ⚑ This is a dead end **from this build**, not a dead end: the door is *a
/// build that can read the value* (`DF-18-3a-f-UNREADABLE-VALUE-LATCH`, which
/// is deliberately routed to no story, because no story can fix it).
///
/// # Why a readable operator report permits a retry
///
/// `OperatorResolved(Present)` and `OperatorResolved(Absent)` both permit,
/// symmetric with `Resolved(Applied)`, which already permits. 🔴 **A report is
/// about a moment, and the tree moves.** A "completion" state that refused
/// after `Present` was proposed and rejected: the operator reports `present` at
/// 14:02, checks out a branch at 14:40, and a refusing state now blocks forever
/// on a report that is **stale, not wrong** — `NeverAttempted`-refuses rebuilt
/// from the other side. The honesty belongs in the row and the card, ⛔ never
/// in this gate. ⛔ No third door.
///
/// ⚑ The latch cut 1 shipped closed has a release: `/artifact resolve <id>
/// present|absent` appends [`RoomEvent::PatchApplyInspected`]
/// (`DF-18-3a-d-INDETERMINATE-CLEARING`, closed by `18-3a-f`). ⛔ "Make it fail
/// open" is still forbidden as the fix — the release is a new durable fact.
fn apply_is_refused(state: ApplyState) -> bool {
    matches!(
        state,
        ApplyState::Indeterminate
            | ApplyState::Resolved(ApplyOutcome::Unknown)
            | ApplyState::OperatorResolved(OperatorApplyFinding::Unknown)
    )
}

/// Classify one apply attempt for the durable resolution record.
///
/// ⛔ Never collapses `Conflict` into `Failed`: the distinction is what
/// `18-3a-e`'s outcome vocabulary renders, and a conflict left the working tree
/// untouched while a failure may not have.
fn apply_outcome(result: &Result<(), MergeBackError>) -> ApplyOutcome {
    match result {
        Ok(()) => ApplyOutcome::Applied,
        Err(MergeBackError::Conflict(_)) => ApplyOutcome::Conflict,
        Err(_) => ApplyOutcome::Failed,
    }
}

fn ensure_same_stored_artifact(
    stored: &ArtifactRef,
    supplied: &ArtifactRef,
) -> Result<(), MergeBackError> {
    // `review` is intentionally excluded: it is projected from the journal, not
    // stored in immutable content-addressed metadata (see `authoritative_artifact`).
    if stored.id != supplied.id
        || stored.content_hash != supplied.content_hash
        || stored.kind != supplied.kind
        || stored.producer != supplied.producer
        || stored.authority != supplied.authority
        || stored.provenance != supplied.provenance
        || stored.depends_on != supplied.depends_on
        || stored.host != supplied.host
    {
        return Err(MergeBackError::ArtifactMetadataMismatch);
    }
    Ok(())
}

#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum MergeBackError {
    #[error("captured patch is empty")]
    EmptyPatch,
    #[error("artifact is not a patch")]
    NotPatch,
    #[error("patch has not satisfied the review gate")]
    ReviewRequired,
    #[error("supplied artifact metadata does not match durable metadata")]
    ArtifactMetadataMismatch,
    #[error("binary patch requires manual application")]
    BinaryPatch,
    #[error("artifact body is not a well-formed unified diff")]
    MalformedPatch,
    #[error("artifact operation failed: {0}")]
    Artifact(#[from] ArtifactError),
    #[error("room journal operation failed: {0}")]
    Journal(#[from] JournalError),
    #[error("git apply could not start or complete: {0}")]
    GitIo(String),
    #[error("git apply rejected the patch without mutating the workspace: {0}")]
    Conflict(String),
    #[error(
        "a prior apply of this patch did not record an outcome; the workspace state is \
         indeterminate and it will not be re-applied"
    )]
    ApplyIndeterminate,
    #[error(
        "git apply returned but the apply-outcome record could not be persisted; \
         the workspace may carry the delta and the artifact is now indeterminate: {0}"
    )]
    ApplyUnresolved(String),
    /// 🔴 The wedged-only gate (Story 18.3a-f, ruling A5). Carries the state it
    /// actually found so the refusal can name it instead of guessing.
    #[error("this patch is not awaiting an operator report; its recorded apply state is {0}")]
    ApplyNotWedged(String),
    /// 🔴 The verb never mints an unreadable finding (Story 18.3a-f). `Unknown`
    /// is deserialize-only forward-compat; an operator's report is `present` or
    /// `absent`. Refused so the verb cannot create an `OperatorResolved(Unknown)`
    /// this build would then refuse to release.
    #[error(
        "an operator report must be 'present' or 'absent'; 'unknown' is a forward-compat value this build cannot record"
    )]
    UnreadableFinding,
    #[error("another process is applying a patch to this workspace")]
    WorkspaceBusy,
    #[error("the workspace apply lock could not be taken: {0}")]
    ApplyLockIo(String),
}
