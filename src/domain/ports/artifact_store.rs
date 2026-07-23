//! Content-addressed evidence persistence boundary.

use async_trait::async_trait;

use crate::domain::models::{ArtifactId, ContentHash, EvidenceArtifact, EvidenceArtifactDraft};

#[async_trait]
pub trait ArtifactStore: Send + Sync {
    async fn put(
        &self,
        meta: EvidenceArtifactDraft,
        body: &[u8],
    ) -> Result<EvidenceArtifact, ArtifactError>;

    async fn get(&self, id: &ArtifactId) -> Result<Vec<u8>, ArtifactError>;

    async fn head(&self, id: &ArtifactId) -> Result<EvidenceArtifact, ArtifactError>;
}

#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ArtifactError {
    #[error("artifact not found: {0}")]
    NotFound(ArtifactId),
    #[error("artifact integrity mismatch: expected {expected}, got {actual}")]
    IntegrityMismatch {
        expected: ContentHash,
        actual: ContentHash,
    },
    #[error("artifact metadata conflicts with existing content-addressed handle: {0}")]
    MetadataConflict(ArtifactId),
    #[error("artifact body is too large: {size} bytes exceeds {max} byte cap")]
    BodyTooLarge { size: usize, max: usize },
    #[error("artifact I/O failed during {operation}: {message}")]
    Io {
        operation: &'static str,
        message: String,
    },
    #[error("artifact metadata serialization failed: {0}")]
    Serialization(String),
}
