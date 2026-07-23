#![cfg(unix)]

use std::sync::Arc;

use rustain::adapters::artifact::FileSystemArtifactStore;
use rustain::adapters::isolation::CowIsolationProvider;
use rustain::domain::models::{
    AgentId, ArtifactKind, CapabilityTokenId, HostBinding, OwnershipKind, PermissionMode,
    ProvenanceTag, ProvisioningTier, ReviewStatus, ReviewVerdict, RoomEvent, UnifiedDiff,
};
use rustain::domain::ports::IsolationProvider;
use rustain::domain::services::patch_review::{ApplyDecision, MergeBackPolicy, may_apply_patch};
use rustain::infrastructure::orchestrator::{MergeBackError, PatchMergeBack};
use rustain::infrastructure::runtime::event_bus::EventBus;
use rustain::infrastructure::subagent::NodeJournal;

fn run_git(path: &std::path::Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .expect("git must execute");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
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

async fn service(
    workspace: &std::path::Path,
) -> (PatchMergeBack, Arc<NodeJournal>, AgentId, AgentId) {
    run_git(workspace, &["init", "-q"]);
    std::fs::write(workspace.join("file.txt"), "old\n").unwrap();
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

    let store = Arc::new(FileSystemArtifactStore::new(workspace));
    let journal = Arc::new(NodeJournal::open_workspace(workspace).await.unwrap());
    let (event_bus, _events) = EventBus::new(32);
    let producer = AgentId::new();
    let reviewer = AgentId::new();
    (
        PatchMergeBack::new(
            workspace.to_path_buf(),
            store,
            journal.clone(),
            Arc::new(event_bus),
            Arc::new(rustain::adapters::merge_back::GitPatchApplier),
        ),
        journal,
        producer,
        reviewer,
    )
}

#[test]
fn same_policy_applies_user_originated_but_refuses_tainted_patch() {
    let producer = AgentId::new();
    let other = AgentId::new();
    let mut artifact = rustain::domain::models::EvidenceArtifact {
        id: rustain::domain::models::ArtifactId::from(
            rustain::domain::models::ContentHash::from_bytes([7; 32]),
        ),
        kind: ArtifactKind::Patch,
        producer: producer.clone(),
        content_hash: rustain::domain::models::ContentHash::from_bytes([7; 32]),
        authority: CapabilityTokenId::root(),
        provenance: vec![ProvenanceTag::UserOriginated],
        depends_on: vec![],
        review: Some(ReviewStatus::Pending),
        host: HostBinding::new("host", "workspace"),
    };
    let none = MergeBackPolicy::default();
    let auto_user = MergeBackPolicy {
        auto_approve_user_originated: true,
    };

    assert_eq!(
        may_apply_patch(&artifact, OwnershipKind::Owned, PermissionMode::Yolo, &none),
        ApplyDecision::Refuse,
        "headless without review or policy must fail closed"
    );
    assert_eq!(
        may_apply_patch(
            &artifact,
            OwnershipKind::Owned,
            PermissionMode::Yolo,
            &auto_user,
        ),
        ApplyDecision::Apply,
        "the configured user-originated policy arm must be reachable"
    );

    artifact.provenance = vec![ProvenanceTag::SelfOriginated];
    assert_eq!(
        may_apply_patch(
            &artifact,
            OwnershipKind::Owned,
            PermissionMode::Yolo,
            &auto_user,
        ),
        ApplyDecision::Refuse,
        "the same policy must refuse agent-derived/tainted content"
    );
    artifact.review = Some(ReviewStatus::Reviewed {
        reviewer: producer.clone(),
        verdict: ReviewVerdict::Approved,
    });
    assert_eq!(
        may_apply_patch(&artifact, OwnershipKind::Owned, PermissionMode::Yolo, &none),
        ApplyDecision::Refuse,
        "agent-derived patch cannot self-review"
    );
    artifact.review = Some(ReviewStatus::Reviewed {
        reviewer: other,
        verdict: ReviewVerdict::Approved,
    });
    assert_eq!(
        may_apply_patch(&artifact, OwnershipKind::Owned, PermissionMode::Yolo, &none),
        ApplyDecision::Apply
    );
    assert_eq!(
        may_apply_patch(&artifact, OwnershipKind::Peer, PermissionMode::Yolo, &none),
        ApplyDecision::Refuse
    );
}

#[tokio::test]
async fn plan_mode_refuses_merge_back_even_when_reviewed() {
    let workspace = tempfile::tempdir().unwrap();
    let (service, _journal, producer, reviewer) = service(workspace.path()).await;
    let artifact = service
        .capture(
            producer,
            CapabilityTokenId::root(),
            vec![ProvenanceTag::UserOriginated],
            vec![],
            HostBinding::new("host", "workspace"),
            &patch("old", "plan-blocked"),
        )
        .await
        .unwrap();
    let reviewed = service
        .review(artifact, reviewer, ReviewVerdict::Approved)
        .await
        .unwrap();
    // Plan mode is read-only: a reviewed+approved patch must NOT mutate the
    // One-Ring. (P16 keystone — a mutant that ignores PermissionMode fails RED.)
    let error = service
        .apply(
            &reviewed,
            OwnershipKind::Owned,
            PermissionMode::Plan,
            &MergeBackPolicy::default(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, MergeBackError::ReviewRequired),
        "plan mode must refuse merge-back: {error}"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("file.txt")).unwrap(),
        "old\n",
        "the One-Ring must be untouched under plan mode"
    );
    // Positive control: the same approved patch applies under a write mode.
    service
        .apply(
            &reviewed,
            OwnershipKind::Owned,
            PermissionMode::Yolo,
            &MergeBackPolicy::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("file.txt")).unwrap(),
        "plan-blocked\n"
    );
}

#[tokio::test]
async fn forged_review_field_cannot_bypass_the_journal_gate() {
    let workspace = tempfile::tempdir().unwrap();
    let (service, _journal, producer, _reviewer) = service(workspace.path()).await;
    let artifact = service
        .capture(
            producer.clone(),
            CapabilityTokenId::root(),
            vec![ProvenanceTag::UserOriginated],
            vec![],
            HostBinding::new("host", "workspace"),
            &patch("old", "forged"),
        )
        .await
        .unwrap();
    // P1 regression: a caller mutates the public `review` field to forge an
    // approval WITHOUT journaling PatchReviewed. apply must refuse because it
    // resolves authoritative review from the journal projection (Pending), not
    // the caller-supplied (forgeable) field. A mutant that authorizes from the
    // supplied artifact fails RED.
    let mut forged = artifact.clone();
    forged.review = Some(ReviewStatus::Reviewed {
        reviewer: producer,
        verdict: ReviewVerdict::Approved,
    });
    let error = service
        .apply(
            &forged,
            OwnershipKind::Owned,
            PermissionMode::Yolo,
            &MergeBackPolicy::default(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, MergeBackError::ReviewRequired),
        "a forged in-memory review must not reach git apply: {error}"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("file.txt")).unwrap(),
        "old\n",
        "the One-Ring must be untouched by a forged review"
    );
}

#[tokio::test]
async fn patch_is_durable_reviewed_then_applied_and_journaled() {
    let workspace = tempfile::tempdir().unwrap();
    let (service, journal, producer, reviewer) = service(workspace.path()).await;
    let artifact = service
        .capture(
            producer,
            CapabilityTokenId::root(),
            vec![ProvenanceTag::SelfOriginated],
            vec![],
            HostBinding::new("host", "workspace"),
            &patch("old", "new"),
        )
        .await
        .unwrap();

    assert_eq!(artifact.kind, ArtifactKind::Patch);
    assert_eq!(artifact.review, Some(ReviewStatus::Pending));
    assert!(
        service
            .apply(
                &artifact,
                OwnershipKind::Owned,
                PermissionMode::Yolo,
                &MergeBackPolicy::default(),
            )
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("file.txt")).unwrap(),
        "old\n"
    );

    let reviewed = service
        .review(artifact, reviewer, ReviewVerdict::Approved)
        .await
        .unwrap();
    service
        .apply(
            &reviewed,
            OwnershipKind::Owned,
            PermissionMode::Yolo,
            &MergeBackPolicy::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("file.txt")).unwrap(),
        "new\n"
    );

    let room = journal.project_room("host").await.unwrap();
    let projected = room.artifacts().get(&reviewed.id).unwrap();
    assert!(matches!(
        projected.review,
        Some(ReviewStatus::Reviewed { .. })
    ));
    let mut drifted = serde_json::to_value(RoomEvent::PatchReviewed {
        artifact: reviewed.id,
        reviewer: AgentId::new(),
        verdict: ReviewVerdict::Approved,
    })
    .unwrap();
    drifted.as_object_mut().unwrap().remove("verdict");
    assert!(
        serde_json::from_value::<RoomEvent>(drifted).is_err(),
        "a drifted review event must fail deserialization"
    );
}

#[tokio::test]
async fn configured_user_originated_policy_can_apply_without_attending_reviewer() {
    let workspace = tempfile::tempdir().unwrap();
    let (service, _journal, producer, _reviewer) = service(workspace.path()).await;
    let artifact = service
        .capture(
            producer,
            CapabilityTokenId::root(),
            vec![ProvenanceTag::UserOriginated],
            vec![],
            HostBinding::new("host", "workspace"),
            &patch("old", "policy"),
        )
        .await
        .unwrap();
    service
        .apply(
            &artifact,
            OwnershipKind::Owned,
            PermissionMode::Yolo,
            &MergeBackPolicy {
                auto_approve_user_originated: true,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("file.txt")).unwrap(),
        "policy\n"
    );
}

#[tokio::test]
async fn binary_and_malformed_patches_are_hard_errors_not_conflicts() {
    let workspace = tempfile::tempdir().unwrap();
    let (service, _journal, producer, reviewer) = service(workspace.path()).await;
    let binary = UnifiedDiff::new(
        ProvisioningTier::ScratchCopy,
        "diff --git a/file.bin b/file.bin\nGIT binary patch\nliteral 0\nHcmV?d00001\n".into(),
    );
    let binary_error = service
        .capture(
            producer.clone(),
            CapabilityTokenId::root(),
            vec![ProvenanceTag::SelfOriginated],
            vec![],
            HostBinding::new("host", "workspace"),
            &binary,
        )
        .await
        .unwrap_err();
    assert!(matches!(binary_error, MergeBackError::BinaryPatch));

    let malformed = UnifiedDiff::new(ProvisioningTier::ScratchCopy, "not a patch\n".into());
    let malformed = service
        .capture(
            producer,
            CapabilityTokenId::root(),
            vec![ProvenanceTag::SelfOriginated],
            vec![],
            HostBinding::new("host", "workspace"),
            &malformed,
        )
        .await
        .unwrap();
    let malformed = service
        .review(malformed, reviewer, ReviewVerdict::Approved)
        .await
        .unwrap();
    let error = service
        .apply(
            &malformed,
            OwnershipKind::Owned,
            PermissionMode::Yolo,
            &MergeBackPolicy::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, MergeBackError::MalformedPatch));
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("file.txt")).unwrap(),
        "old\n"
    );
}

#[tokio::test]
async fn concurrent_conflicting_mergeback_serializes_and_one_fails_closed() {
    let workspace = tempfile::tempdir().unwrap();
    let (service, _journal, producer, reviewer) = service(workspace.path()).await;
    let service = Arc::new(service);

    let first = service
        .capture(
            producer.clone(),
            CapabilityTokenId::root(),
            vec![ProvenanceTag::SelfOriginated],
            vec![],
            HostBinding::new("host", "workspace"),
            &patch("old", "first"),
        )
        .await
        .unwrap();
    let second = service
        .capture(
            producer,
            CapabilityTokenId::root(),
            vec![ProvenanceTag::SelfOriginated],
            vec![],
            HostBinding::new("host", "workspace"),
            &patch("old", "second"),
        )
        .await
        .unwrap();
    let first = service
        .review(first, reviewer.clone(), ReviewVerdict::Approved)
        .await
        .unwrap();
    let second = service
        .review(second, reviewer, ReviewVerdict::Approved)
        .await
        .unwrap();

    let a = {
        let service = service.clone();
        tokio::spawn(async move {
            service
                .apply(
                    &first,
                    OwnershipKind::Owned,
                    PermissionMode::Yolo,
                    &MergeBackPolicy::default(),
                )
                .await
        })
    };
    let b = {
        let service = service.clone();
        tokio::spawn(async move {
            service
                .apply(
                    &second,
                    OwnershipKind::Owned,
                    PermissionMode::Yolo,
                    &MergeBackPolicy::default(),
                )
                .await
        })
    };
    let results = [a.await.unwrap(), b.await.unwrap()];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(MergeBackError::Conflict(_))))
            .count(),
        1,
        "a well-formed non-applying second patch must be classified as conflict"
    );
    let body = std::fs::read_to_string(workspace.path().join("file.txt")).unwrap();
    assert!(body == "first\n" || body == "second\n");
}

#[tokio::test]
async fn killed_process_orphan_is_reaped_but_live_and_unowned_siblings_survive() {
    const CHILD_MODE: &str = "RUSTAIN_COW_ORPHAN_CHILD";
    const LIVE_CHILD_MODE: &str = "RUSTAIN_COW_LIVE_CHILD";
    if std::env::var_os(CHILD_MODE).is_some() {
        let workspace = std::path::PathBuf::from(std::env::var("RUSTAIN_COW_WORKSPACE").unwrap());
        let report = std::path::PathBuf::from(std::env::var("RUSTAIN_COW_REPORT").unwrap());
        let handle = CowIsolationProvider::default()
            .start(&workspace)
            .await
            .unwrap();
        std::fs::write(&report, handle.path().as_os_str().as_encoded_bytes()).unwrap();
        std::mem::forget(handle);
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        return;
    }
    if std::env::var_os(LIVE_CHILD_MODE).is_some() {
        // P12: a SECOND live process holds an owned scratch with a distinct
        // PID, so the reaper's cross-process `process_is_live(pid)` branch is
        // exercised — not only the same-process `same_live_process` branch.
        let workspace = std::path::PathBuf::from(std::env::var("RUSTAIN_COW_WORKSPACE").unwrap());
        let report = std::path::PathBuf::from(std::env::var("RUSTAIN_COW_REPORT").unwrap());
        let handle = CowIsolationProvider::default()
            .start(&workspace)
            .await
            .unwrap();
        std::fs::write(&report, handle.path().as_os_str().as_encoded_bytes()).unwrap();
        // Keep the handle alive so the marker PID (this child's) stays live.
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        return;
    }

    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("seed.txt"), "seed\n").unwrap();
    let report_dir = tempfile::tempdir().unwrap();

    // P12: spawn a LIVE sibling process holding its own owned scratch.
    let live_child_report = report_dir.path().join("live-child-path");
    let mut live_child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "killed_process_orphan_is_reaped_but_live_and_unowned_siblings_survive",
            "--nocapture",
        ])
        .env(LIVE_CHILD_MODE, "1")
        .env("RUSTAIN_COW_WORKSPACE", workspace.path())
        .env("RUSTAIN_COW_REPORT", &live_child_report)
        .spawn()
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !live_child_report.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("live child must report its owned scratch path");
    let live_child_owned = std::path::PathBuf::from(
        String::from_utf8(std::fs::read(&live_child_report).unwrap()).unwrap(),
    );

    // Dead orphan: spawn, let it create+leak a scratch, then SIGKILL it.
    let report = report_dir.path().join("child-path");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "killed_process_orphan_is_reaped_but_live_and_unowned_siblings_survive",
            "--nocapture",
        ])
        .env(CHILD_MODE, "1")
        .env("RUSTAIN_COW_WORKSPACE", workspace.path())
        .env("RUSTAIN_COW_REPORT", &report)
        .spawn()
        .unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !report.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child must report its owned scratch path");
    let dead_owned =
        std::path::PathBuf::from(String::from_utf8(std::fs::read(&report).unwrap()).unwrap());
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(
        dead_owned.exists(),
        "SIGKILL must leave the crash orphan behind"
    );

    let provider = CowIsolationProvider::default();
    let live = provider.start(workspace.path()).await.unwrap();
    let live_path = live.path().to_path_buf();
    let unrelated = tempfile::Builder::new()
        .prefix("rustain-isolation-unowned-")
        .tempdir()
        .unwrap();
    let unrelated_path = unrelated.path().to_path_buf();

    let trigger = provider.start(workspace.path()).await.unwrap();
    assert!(
        !dead_owned.exists(),
        "dead process-owned scratch must be reaped"
    );
    assert!(
        live_path.exists(),
        "older-but-live owned sibling (same process) must survive"
    );
    assert!(
        live_child_owned.exists(),
        "live sibling owned by a DIFFERENT process must survive (process_is_live cross-process branch)"
    );
    assert!(
        unrelated_path.exists(),
        "prefix collision without ownership marker must survive"
    );

    provider.stop(trigger).await.unwrap();
    provider.stop(live).await.unwrap();
    // Reap the live child so it does not outlive the test.
    let _ = live_child.kill();
    let _ = live_child.wait();
}
