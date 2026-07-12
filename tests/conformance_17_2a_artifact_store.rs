use rustain::adapters::artifact::FileSystemArtifactStore;
use rustain::domain::models::{
    AgentId, ArtifactKind, CapabilityTokenId, EvidenceArtifactDraft, HostBinding, ProvenanceTag,
};
use rustain::domain::ports::{ArtifactError, ArtifactStore};

fn draft() -> EvidenceArtifactDraft {
    EvidenceArtifactDraft {
        kind: ArtifactKind::Evidence,
        producer: AgentId::parse("artifact-producer").expect("valid fixture agent id"),
        authority: CapabilityTokenId::root(),
        provenance: vec![ProvenanceTag::SelfOriginated],
        depends_on: Vec::new(),
        review: None,
        host: HostBinding::new("host-a", "workspace-a"),
    }
}

async fn artifact_store_conformance(store: &impl ArtifactStore) {
    let body = b"durable evidence body";
    let first = store.put(draft(), body).await.expect("put artifact");
    let second = store
        .put(draft(), body)
        .await
        .expect("idempotent duplicate put");

    assert_eq!(first, second, "same body and metadata must deduplicate");
    assert_eq!(first.id.content_hash(), first.content_hash);
    assert_eq!(store.head(&first.id).await.expect("head artifact"), first);
    assert_eq!(store.get(&first.id).await.expect("get artifact"), body);
}

#[tokio::test]
async fn filesystem_adapter_satisfies_artifact_store_contract() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let store = FileSystemArtifactStore::new(workspace.path());
    artifact_store_conformance(&store).await;
}

#[tokio::test]
async fn get_rejects_tampered_body_hash() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let store = FileSystemArtifactStore::new(workspace.path());
    let artifact = store.put(draft(), b"original").await.expect("put artifact");
    let body_path = workspace
        .path()
        .join(".rustain")
        .join("artifacts")
        .join(artifact.id.as_str())
        .join("body");
    tokio::fs::write(body_path, b"tampered")
        .await
        .expect("inject body corruption");

    let error = store
        .get(&artifact.id)
        .await
        .expect_err("hash mismatch must fail closed");
    assert!(matches!(error, ArtifactError::IntegrityMismatch { .. }));
}

#[tokio::test]
async fn put_rejects_oversized_body_before_writing() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let store = FileSystemArtifactStore::new(workspace.path()).with_max_body_bytes(4);
    let error = store
        .put(draft(), b"12345")
        .await
        .expect_err("oversized artifact must be rejected");
    assert!(matches!(
        error,
        ArtifactError::BodyTooLarge { size: 5, max: 4 }
    ));
    assert!(
        !workspace.path().join(".rustain").exists(),
        "rejected body must not create partial artifact state"
    );
}

#[tokio::test]
async fn put_rejects_identical_body_with_conflicting_metadata() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let store = FileSystemArtifactStore::new(workspace.path());
    let first = store
        .put(draft(), b"shared evidence body")
        .await
        .expect("put initial artifact");
    let mut conflicting = draft();
    conflicting.producer =
        AgentId::parse("other-artifact-producer").expect("valid conflicting fixture agent id");

    let error = store
        .put(conflicting, b"shared evidence body")
        .await
        .expect_err("different metadata for identical bytes must fail");

    assert!(matches!(error, ArtifactError::MetadataConflict(_)));
    assert_eq!(
        store
            .head(&first.id)
            .await
            .expect("original metadata remains"),
        first
    );
}

#[tokio::test]
async fn put_deduplicates_identical_body_and_metadata() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let store = FileSystemArtifactStore::new(workspace.path());

    let first = store
        .put(draft(), b"identical evidence body")
        .await
        .expect("put initial artifact");
    let second = store
        .put(draft(), b"identical evidence body")
        .await
        .expect("deduplicate identical artifact");

    assert_eq!(first, second);
}

#[tokio::test]
async fn get_rejects_oversized_tampered_body_before_hashing() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let store = FileSystemArtifactStore::new(workspace.path()).with_max_body_bytes(4);
    let artifact = store
        .put(draft(), b"1234")
        .await
        .expect("put artifact at cap");
    let body_path = workspace
        .path()
        .join(".rustain")
        .join("artifacts")
        .join(artifact.id.as_str())
        .join("body");
    tokio::fs::write(body_path, b"12345")
        .await
        .expect("plant oversized tampered body");

    let error = store
        .get(&artifact.id)
        .await
        .expect_err("oversized body must fail before hashing");

    assert!(matches!(
        error,
        ArtifactError::BodyTooLarge { size: 5, max: 4 }
    ));
}
