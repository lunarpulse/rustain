use std::path::PathBuf;
use std::sync::Arc;

use crate::domain::events::AppEvent;
use crate::domain::models::{
    AgentId, ArtifactId, ArtifactKind, ArtifactRef, CapabilityTokenId, EvidenceArtifactDraft,
    HostBinding, OwnershipKind, PermissionMode, ProvenanceTag, ReviewStatus, ReviewVerdict,
    RoomEvent, UnifiedDiff,
};
use crate::domain::ports::{ArtifactError, ArtifactStore, PatchApplier, PatchApplyError};
use crate::domain::services::patch_review::{ApplyDecision, MergeBackPolicy, may_apply_patch};
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

    /// Apply a reviewed patch under one async critical section.
    ///
    /// Authorization is resolved from the **canonical journal projection**, not
    /// the caller-supplied artifact handle: the `review` field on a public
    /// `ArtifactRef` is forgeable, so `apply` re-derives the authoritative
    /// review state from the single writable room journal before consulting the
    /// pure `may_apply_patch` gate. The git mutation is delegated to the
    /// [`PatchApplier`] port; conflicts leave the workspace unchanged.
    pub async fn apply(
        &self,
        artifact: &ArtifactRef,
        ownership: OwnershipKind,
        permission_mode: PermissionMode,
        policy: &MergeBackPolicy,
    ) -> Result<(), MergeBackError> {
        let _guard = self.apply_guard.lock().await;
        let stored = self.store.head(&artifact.id).await?;
        ensure_same_stored_artifact(&stored, artifact)?;
        // P1: the authoritative review state comes from the journal projection,
        // never the caller-supplied (forgeable) `review` field.
        let authoritative = self.authoritative_artifact(&stored).await?;
        if may_apply_patch(&authoritative, ownership, permission_mode, policy)
            != ApplyDecision::Apply
        {
            return Err(MergeBackError::ReviewRequired);
        }
        let body = self.store.get(&artifact.id).await?;
        validate_patch_body(&body)?;
        self.applier
            .apply(&self.workspace, &body)
            .await
            .map_err(map_apply_error)
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
        let reviewed = self.review(artifact, reviewer, verdict).await?;
        self.apply(&reviewed, ownership, permission_mode, policy)
            .await
    }

    /// Resolve the canonical, journal-derived view of a stored patch artifact.
    /// The projected `review` state reflects `PatchReviewed` events actually
    /// appended to the room journal — the only legitimate review transition.
    async fn authoritative_artifact(
        &self,
        stored: &ArtifactRef,
    ) -> Result<ArtifactRef, MergeBackError> {
        let room = self.journal.project_room(&stored.host.host_id).await?;
        room.artifacts()
            .get(&stored.id)
            .cloned()
            .ok_or(MergeBackError::ReviewRequired)
    }

    async fn persist(&self, event: RoomEvent) -> Result<(), MergeBackError> {
        self.journal.append_room(event.clone()).await?;
        let _ = self
            .event_bus
            .emit_domain(AppEvent::DomainEvent(event.into()));
        Ok(())
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
}
