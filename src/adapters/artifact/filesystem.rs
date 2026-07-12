use std::path::{Path, PathBuf};

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::domain::models::{ArtifactId, ContentHash, EvidenceArtifact, EvidenceArtifactDraft};
use crate::domain::ports::{ArtifactError, ArtifactStore};

pub const MAX_ARTIFACT_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Workspace-local content-addressed artifact store.
pub struct FileSystemArtifactStore {
    root: PathBuf,
    max_body_bytes: usize,
    write_lock: tokio::sync::Mutex<()>,
}

impl FileSystemArtifactStore {
    pub fn new(workspace: &Path) -> Self {
        Self {
            root: workspace.join(".rustain").join("artifacts"),
            max_body_bytes: MAX_ARTIFACT_BODY_BYTES,
            write_lock: tokio::sync::Mutex::new(()),
        }
    }

    #[must_use]
    pub fn with_max_body_bytes(mut self, max_body_bytes: usize) -> Self {
        self.max_body_bytes = max_body_bytes;
        self
    }

    fn artifact_dir(&self, id: &ArtifactId) -> PathBuf {
        self.root.join(id.as_str())
    }

    async fn verify_body(
        &self,
        id: &ArtifactId,
        expected: ContentHash,
    ) -> Result<Vec<u8>, ArtifactError> {
        let path = self.artifact_dir(id).join("body");
        let size = tokio::fs::metadata(&path)
            .await
            .map_err(|error| map_read_error(id, "read body", error))?
            .len();
        if size > self.max_body_bytes as u64 {
            return Err(ArtifactError::BodyTooLarge {
                size: usize::try_from(size).unwrap_or(usize::MAX),
                max: self.max_body_bytes,
            });
        }
        let body = tokio::fs::read(&path)
            .await
            .map_err(|error| map_read_error(id, "read body", error))?;
        let actual = content_hash(&body);
        if actual != expected {
            return Err(ArtifactError::IntegrityMismatch { expected, actual });
        }
        Ok(body)
    }
}

#[async_trait]
impl ArtifactStore for FileSystemArtifactStore {
    async fn put(
        &self,
        meta: EvidenceArtifactDraft,
        body: &[u8],
    ) -> Result<EvidenceArtifact, ArtifactError> {
        if body.len() > self.max_body_bytes {
            return Err(ArtifactError::BodyTooLarge {
                size: body.len(),
                max: self.max_body_bytes,
            });
        }
        let content_hash = content_hash(body);
        let id = ArtifactId::from(content_hash);
        let artifact = EvidenceArtifact {
            id: id.clone(),
            kind: meta.kind,
            producer: meta.producer,
            content_hash,
            authority: meta.authority,
            provenance: meta.provenance,
            depends_on: meta.depends_on,
            review: meta.review,
            host: meta.host,
        };
        let _guard = self.write_lock.lock().await;
        let directory = self.artifact_dir(&id);
        let meta_path = directory.join("meta.json");

        if tokio::fs::try_exists(&meta_path)
            .await
            .map_err(|error| io_error("inspect metadata", error))?
        {
            let existing = self.head(&id).await?;
            if existing != artifact {
                return Err(ArtifactError::MetadataConflict(id));
            }
            self.verify_body(&existing.id, existing.content_hash)
                .await?;
            return Ok(existing);
        }

        tokio::fs::create_dir_all(&directory)
            .await
            .map_err(|error| io_error("create artifact directory", error))?;
        set_directory_permissions(&directory).await?;
        let body_path = directory.join("body");
        if tokio::fs::try_exists(&body_path)
            .await
            .map_err(|error| io_error("inspect body", error))?
        {
            self.verify_body(&id, content_hash).await?;
        } else {
            atomic_write(&body_path, body).await?;
        }

        let encoded = serde_json::to_vec(&artifact)
            .map_err(|error| ArtifactError::Serialization(error.to_string()))?;
        // Metadata is the commit marker: readers never observe a handle before
        // its fully synced body is available.
        match create_new_metadata(&meta_path, &encoded).await {
            Ok(()) => Ok(artifact),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing = self.head(&id).await?;
                if existing != artifact {
                    return Err(ArtifactError::MetadataConflict(id));
                }
                self.verify_body(&existing.id, existing.content_hash)
                    .await?;
                Ok(existing)
            }
            Err(error) => Err(io_error("publish artifact metadata", error)),
        }
    }

    async fn get(&self, id: &ArtifactId) -> Result<Vec<u8>, ArtifactError> {
        let artifact = self.head(id).await?;
        self.verify_body(id, artifact.content_hash).await
    }

    async fn head(&self, id: &ArtifactId) -> Result<EvidenceArtifact, ArtifactError> {
        let path = self.artifact_dir(id).join("meta.json");
        let encoded = tokio::fs::read(&path)
            .await
            .map_err(|error| map_read_error(id, "read metadata", error))?;
        let artifact = serde_json::from_slice::<EvidenceArtifact>(&encoded)
            .map_err(|error| ArtifactError::Serialization(error.to_string()))?;
        let expected = id.content_hash();
        if artifact.id != *id || artifact.content_hash != expected {
            return Err(ArtifactError::IntegrityMismatch {
                expected,
                actual: artifact.content_hash,
            });
        }
        Ok(artifact)
    }
}

fn content_hash(body: &[u8]) -> ContentHash {
    let digest: [u8; 32] = Sha256::digest(body).into();
    ContentHash::from_bytes(digest)
}

async fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), ArtifactError> {
    let parent = path.parent().ok_or_else(|| ArtifactError::Io {
        operation: "resolve artifact parent",
        message: "artifact path has no parent".into(),
    })?;
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        nanoid::nanoid!(8)
    ));
    let mut options = tokio::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = async {
        let mut file = options
            .open(&temp)
            .await
            .map_err(|error| io_error("create temporary artifact", error))?;
        file.write_all(bytes)
            .await
            .map_err(|error| io_error("write temporary artifact", error))?;
        file.sync_data()
            .await
            .map_err(|error| io_error("sync temporary artifact", error))?;
        tokio::fs::rename(&temp, path)
            .await
            .map_err(|error| io_error("publish artifact", error))?;
        sync_directory(parent).await
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temp).await;
    }
    result
}

async fn create_new_metadata(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "artifact metadata path has no parent",
        )
    })?;
    let mut options = tokio::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).await?;
    file.write_all(bytes).await?;
    file.sync_data().await?;
    let directory = tokio::fs::File::open(parent).await?;
    directory.sync_data().await
}

async fn sync_directory(path: &Path) -> Result<(), ArtifactError> {
    let directory = tokio::fs::File::open(path)
        .await
        .map_err(|error| io_error("open artifact directory", error))?;
    directory
        .sync_data()
        .await
        .map_err(|error| io_error("sync artifact directory", error))
}

#[cfg(unix)]
async fn set_directory_permissions(path: &Path) -> Result<(), ArtifactError> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .await
        .map_err(|error| io_error("set artifact directory permissions", error))
}

#[cfg(not(unix))]
async fn set_directory_permissions(_path: &Path) -> Result<(), ArtifactError> {
    Ok(())
}

fn map_read_error(
    id: &ArtifactId,
    operation: &'static str,
    error: std::io::Error,
) -> ArtifactError {
    if error.kind() == std::io::ErrorKind::NotFound {
        ArtifactError::NotFound(id.clone())
    } else {
        io_error(operation, error)
    }
}

fn io_error(operation: &'static str, error: std::io::Error) -> ArtifactError {
    ArtifactError::Io {
        operation,
        message: error.to_string(),
    }
}
