//! Port traits for self-update (Story 13.3a).
//!
//! `SelfUpdatePort` abstracts the GitHub releases API so the orchestration
//! logic is hermetically testable with a fake.
//! `BinaryReplacerPort` abstracts the backup/replace/restore filesystem ops.
use async_trait::async_trait;
use std::path::{Path, PathBuf};

/// Metadata about a GitHub release.
#[derive(Debug, Clone)]
pub struct ReleaseInfo {
    /// Semantic version string (without leading `v`).
    pub version: String,
    /// First N lines of release notes (may be empty).
    pub notes: String,
    /// Downloadable assets attached to the release.
    pub assets: Vec<ReleaseAsset>,
}

/// A single downloadable asset from a GitHub release.
#[derive(Debug, Clone)]
pub struct ReleaseAsset {
    /// Filename as published (e.g. `rustain-0.2.0-x86_64-unknown-linux-gnu`).
    pub name: String,
    /// Direct download URL (browser_download_url from the API).
    pub download_url: String,
}

/// Errors specific to the self-update subsystem.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("Offline: {0}")]
    Offline(String),

    #[error("Connection failed: {0}")]
    ConnectionFailed(String),

    #[error("No release asset for platform: {0}")]
    PlatformNotSupported(String),

    #[error("Install path is not user-writable (managed externally): {0}")]
    ManagedInstall(String),

    #[error("Signature verification failed: {0}")]
    VerifyFailed(#[from] VerifyError),

    #[error("Backup failed: {0}")]
    BackupFailed(String),

    #[error("Replace failed: {0}")]
    ReplaceFailed(String),

    #[error("Restore failed: {0}")]
    RestoreFailed(String),

    #[error(
        "Another update is already in progress (lock held at {0}). If no other 'rustain update' is running, remove the stale lockfile."
    )]
    LockConflict(String),

    #[error("Downgrade refused: latest {latest} is older than current {current}")]
    DowngradeRefused { current: String, latest: String },

    #[error("Channel-pin violation: redirect to untrusted host {0}")]
    UntrustedHost(String),

    #[error("{0}")]
    Other(String),
}

/// Errors from the pure verification path.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VerifyError {
    #[error("Signature missing (no .minisig provided)")]
    SignatureMissing,

    #[error("Bad signature: {0}")]
    BadSignature(String),

    #[error("Untrusted key: signature valid but key not in trust set")]
    UntrustedKey,

    #[error("Checksum mismatch for {artifact}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        artifact: String,
        expected: String,
        actual: String,
    },

    #[error("Artifact not found in manifest: {0}")]
    ArtifactNotInManifest(String),

    #[error("Malformed signature: {0}")]
    MalformedSignature(String),

    #[error("Malformed manifest: {0}")]
    MalformedManifest(String),
}

/// Network-side port: query releases + download assets.
#[async_trait]
pub trait SelfUpdatePort: Send + Sync {
    /// Fetch metadata for the latest release.
    async fn latest_release(&self) -> Result<ReleaseInfo, UpdateError>;

    /// Download a release asset. Returns the raw bytes.
    async fn download_asset(&self, asset: &ReleaseAsset) -> Result<Vec<u8>, UpdateError>;

    /// Download a text asset (SHA256SUMS, .minisig). Returns the raw string.
    async fn download_text_asset(&self, asset: &ReleaseAsset) -> Result<String, UpdateError>;
}

/// Filesystem-side port: backup + atomic binary replace + restore.
#[async_trait]
pub trait BinaryReplacerPort: Send + Sync {
    /// Back up the current executable. Returns the backup path.
    async fn backup_current(&self) -> Result<PathBuf, UpdateError>;

    /// Atomically replace the current executable with `new_bytes`.
    async fn atomic_replace(&self, new_bytes: &[u8]) -> Result<(), UpdateError>;

    /// Restore from a backup path (rollback on failure).
    async fn restore(&self, backup: &Path) -> Result<(), UpdateError>;
}
