//! Atomic binary replacement with backup and lockfile guard (Story 13.3a).

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::adapters::self_update::types::UpdateError;
use crate::domain::ports::self_update::BinaryReplacerPort;
use crate::infrastructure::paths;

/// Guard that holds an exclusive lockfile open and removes it on drop.
///
/// Uses `create_new` (O_EXCL) for a simple cross-platform mutex: if the file
/// already exists another update is in progress → [`UpdateError::LockConflict`].
struct LockGuard {
    path: PathBuf,
}

impl LockGuard {
    fn acquire(data_dir: &std::path::Path) -> Result<Self, UpdateError> {
        let path = data_dir.join("update.lock");
        // `create_new` fails with AlreadyExists if the file is present.
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    UpdateError::LockConflict
                } else {
                    UpdateError::Other(format!("failed to create lockfile: {e}"))
                }
            })?;
        // We immediately drop the file handle — the lockfile's *existence* is
        // the mutex. The guard removes it on drop.
        Ok(Self { path })
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Default [`BinaryReplacerPort`] implementation backed by [`self_replace`].
pub struct DefaultBinaryReplacer {
    current_exe: PathBuf,
    data_dir: PathBuf,
    /// Held for the lifetime of the replacer to prevent concurrent updates.
    _lock: LockGuard,
}

impl DefaultBinaryReplacer {
    /// Resolve paths and acquire the exclusive update lock.
    ///
    /// # Errors
    /// - [`UpdateError::LockConflict`] if another update is already in progress.
    /// - [`UpdateError::Other`] if `current_exe()` or `data_dir()` cannot be resolved.
    pub fn new() -> Result<Self, UpdateError> {
        let current_exe = std::env::current_exe()
            .map_err(|e| UpdateError::Other(format!("cannot resolve current exe: {e}")))?;
        let data_dir = paths::data_dir()
            .map_err(|e| UpdateError::Other(format!("cannot resolve data dir: {e}")))?;
        let lock = LockGuard::acquire(&data_dir)?;
        Ok(Self {
            current_exe,
            data_dir,
            _lock: lock,
        })
    }
}

#[async_trait]
impl BinaryReplacerPort for DefaultBinaryReplacer {
    async fn backup_current(&self) -> Result<PathBuf, UpdateError> {
        let backup_dir = self.data_dir.join("backups");
        tokio::fs::create_dir_all(&backup_dir)
            .await
            .map_err(|e| UpdateError::BackupFailed(format!("create backups dir: {e}")))?;

        let backup_path = backup_dir.join(format!("rustain-v{}", env!("CARGO_PKG_VERSION")));
        tokio::fs::copy(&self.current_exe, &backup_path)
            .await
            .map_err(|e| UpdateError::BackupFailed(format!("copy exe to backup: {e}")))?;

        // AC4: verify backup integrity. A partial copy (e.g. disk-full mid-copy)
        // would leave a truncated backup that a later rollback would restore as a
        // corrupt binary — reject it up front.
        let src_len = tokio::fs::metadata(&self.current_exe)
            .await
            .map_err(|e| UpdateError::BackupFailed(format!("stat current exe: {e}")))?
            .len();
        let dst_len = tokio::fs::metadata(&backup_path)
            .await
            .map_err(|e| UpdateError::BackupFailed(format!("stat backup: {e}")))?
            .len();
        if dst_len != src_len {
            let _ = tokio::fs::remove_file(&backup_path).await;
            return Err(UpdateError::BackupFailed(format!(
                "backup truncated: wrote {dst_len} of {src_len} bytes"
            )));
        }

        Ok(backup_path)
    }

    async fn atomic_replace(&self, new_bytes: &[u8]) -> Result<(), UpdateError> {
        // Write new bytes to a temp file on the SAME filesystem as current_exe
        // (required for the atomic rename that `self_replace` performs internally).
        let exe_dir = self
            .current_exe
            .parent()
            .ok_or_else(|| UpdateError::ReplaceFailed("exe has no parent dir".into()))?;

        let temp_path = exe_dir.join(".rustain-update.tmp");
        tokio::fs::write(&temp_path, new_bytes)
            .await
            .map_err(|e| UpdateError::ReplaceFailed(format!("write temp binary: {e}")))?;

        // `self_replace::self_replace` is a blocking syscall (rename/swap) — run
        // on the blocking pool so we don't stall the async runtime.
        let tp = temp_path.clone();
        let result = tokio::task::spawn_blocking(move || self_replace::self_replace(&tp))
            .await
            .map_err(|e| UpdateError::ReplaceFailed(format!("spawn_blocking: {e}")))?;

        // Clean up temp regardless of outcome.
        let _ = tokio::fs::remove_file(&temp_path).await;

        result.map_err(|e| UpdateError::ReplaceFailed(format!("self_replace: {e}")))
    }

    async fn restore(&self, backup: &Path) -> Result<(), UpdateError> {
        // Restore by self-replacing with the backup binary.
        let bp = backup.to_path_buf();
        let result = tokio::task::spawn_blocking(move || self_replace::self_replace(&bp))
            .await
            .map_err(|e| UpdateError::RestoreFailed(format!("spawn_blocking: {e}")))?;

        result.map_err(|e| UpdateError::RestoreFailed(format!("self_replace restore: {e}")))
    }
}
