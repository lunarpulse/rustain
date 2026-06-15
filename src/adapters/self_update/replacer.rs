//! Atomic binary replacement with backup and lockfile guard (Story 13.3a).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;

use crate::adapters::self_update::types::UpdateError;
use crate::domain::ports::self_update::BinaryReplacerPort;
use crate::infrastructure::paths;

/// How long before an existing lockfile is considered stale and reclaimable.
/// A normal update completes in seconds; anything older is the residue of a
/// crash / SIGKILL / power-loss that bypassed [`LockGuard`]'s `Drop`.
const LOCK_STALE: Duration = Duration::from_secs(60 * 60);

/// Guard that holds an exclusive lockfile and removes it on drop.
///
/// Uses `create_new` (O_EXCL) as a simple cross-platform mutex. CB-4: a lockfile
/// left by a hard crash (which never ran `Drop`) older than [`LOCK_STALE`] is
/// reclaimed so a crash can't permanently disable updates; a fresh lock yields
/// [`UpdateError::LockConflict`] carrying the path so the user can act on it.
#[derive(Debug)]
struct LockGuard {
    path: PathBuf,
}

impl LockGuard {
    fn acquire(data_dir: &Path) -> Result<Self, UpdateError> {
        let path = data_dir.join("update.lock");
        match Self::try_create(&path) {
            Ok(()) => Ok(Self { path }),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Existing lock — reclaim ONLY if demonstrably stale by mtime.
                if Self::is_stale(&path) {
                    let _ = std::fs::remove_file(&path);
                    match Self::try_create(&path) {
                        Ok(()) => Ok(Self { path }),
                        // Lost a race to another updater, or still blocked.
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                            Err(UpdateError::LockConflict(path.display().to_string()))
                        }
                        Err(e) => Err(UpdateError::Other(format!(
                            "failed to create lockfile: {e}"
                        ))),
                    }
                } else {
                    Err(UpdateError::LockConflict(path.display().to_string()))
                }
            }
            Err(e) => Err(UpdateError::Other(format!(
                "failed to create lockfile: {e}"
            ))),
        }
    }

    /// Create the lockfile exclusively and stamp it with our PID (diagnostics
    /// only — the file's *existence* is the mutex; the handle is dropped here).
    fn try_create(path: &Path) -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        let _ = writeln!(f, "{}", std::process::id());
        Ok(())
    }

    /// A lock is stale if its mtime is older than [`LOCK_STALE`]. An unreadable
    /// mtime or a future mtime (clock skew) is treated as NOT stale — fail safe
    /// toward the conflict rather than reclaiming a possibly-live lock.
    fn is_stale(path: &Path) -> bool {
        let Ok(modified) = std::fs::metadata(path).and_then(|m| m.modified()) else {
            return false;
        };
        Self::stale_by_age(modified, SystemTime::now())
    }

    /// Pure staleness decision — unit-testable without manipulating file mtimes.
    fn stale_by_age(modified: SystemTime, now: SystemTime) -> bool {
        matches!(now.duration_since(modified), Ok(age) if age > LOCK_STALE)
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Default [`BinaryReplacerPort`] implementation backed by [`self_replace`].
#[derive(Debug)]
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
        // CB-3: resolve symlinks so we probe + back up the SAME path `self_replace`
        // actually mutates (e.g. Homebrew /usr/local/bin/rustain -> Cellar/...).
        // `self_replace` canonicalizes internally; an unresolved `current_exe()`
        // would let `check_writable` probe the symlink's dir while the real target
        // dir is what gets replaced. Fall back to the unresolved path if
        // canonicalization fails (e.g. path no longer exists).
        let current_exe = std::fs::canonicalize(&current_exe).unwrap_or(current_exe);
        let data_dir = paths::data_dir()
            .map_err(|e| UpdateError::Other(format!("cannot resolve data dir: {e}")))?;
        let lock = LockGuard::acquire(&data_dir)?;
        Ok(Self {
            current_exe,
            data_dir,
            _lock: lock,
        })
    }

    /// Test-only constructor: inject explicit paths so backup/lock behavior can
    /// be exercised in a tempdir without touching the real binary (CB-2).
    #[cfg(test)]
    fn with_paths(current_exe: PathBuf, data_dir: PathBuf) -> Result<Self, UpdateError> {
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
        if let Err(e) = check_backup_len(src_len, dst_len) {
            let _ = tokio::fs::remove_file(&backup_path).await;
            return Err(e);
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

/// AC4 backup-integrity check: a backup whose length differs from the source is
/// a truncated copy (e.g. disk-full mid-copy) that a later rollback would restore
/// as a corrupt binary — reject it up front.
fn check_backup_len(src_len: u64, dst_len: u64) -> Result<(), UpdateError> {
    if dst_len != src_len {
        return Err(UpdateError::BackupFailed(format!(
            "backup truncated: wrote {dst_len} of {src_len} bytes"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ports::self_update::BinaryReplacerPort;

    // CB-2 / P0 #10: a second updater must refuse cleanly while a lock is held.
    #[test]
    fn lock_conflict_when_already_held() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("rustain");
        std::fs::write(&exe, b"fake exe").unwrap();

        let _first = DefaultBinaryReplacer::with_paths(exe.clone(), dir.path().to_path_buf())
            .expect("first lock acquires");
        let second = DefaultBinaryReplacer::with_paths(exe, dir.path().to_path_buf());
        assert!(
            matches!(second, Err(UpdateError::LockConflict(_))),
            "second concurrent updater must get LockConflict, got {second:?}"
        );
    }

    // CB-4: a lock dropped (normal completion) is released, so the next update proceeds.
    #[test]
    fn lock_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("rustain");
        std::fs::write(&exe, b"fake exe").unwrap();

        {
            let _g = DefaultBinaryReplacer::with_paths(exe.clone(), dir.path().to_path_buf())
                .expect("acquire");
        } // drop releases the lock
        let again = DefaultBinaryReplacer::with_paths(exe, dir.path().to_path_buf());
        assert!(
            again.is_ok(),
            "lock must be reacquirable after drop, got {again:?}"
        );
    }

    // CB-4: the staleness decision (pure, deterministic — no mtime manipulation).
    #[test]
    fn stale_by_age_decision() {
        let now = SystemTime::now();
        // Older than LOCK_STALE → stale (reclaimable crash residue).
        let old = now - (LOCK_STALE + Duration::from_secs(60));
        assert!(LockGuard::stale_by_age(old, now), "old lock must be stale");
        // Recent → not stale (a live updater).
        let recent = now - Duration::from_secs(5);
        assert!(
            !LockGuard::stale_by_age(recent, now),
            "recent lock must NOT be stale"
        );
        // Future mtime (clock skew) → not stale (fail safe to conflict).
        let future = now + Duration::from_secs(120);
        assert!(
            !LockGuard::stale_by_age(future, now),
            "future-dated lock must NOT be reclaimed"
        );
    }

    // CB-4: a FRESH lockfile (recent mtime) is NOT reclaimed — fail safe to conflict.
    #[test]
    fn fresh_foreign_lock_is_not_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("update.lock");
        std::fs::write(&lock, b"99999\n").unwrap(); // just created → recent mtime
        let exe = dir.path().join("rustain");
        std::fs::write(&exe, b"fake exe").unwrap();
        let acquired = DefaultBinaryReplacer::with_paths(exe, dir.path().to_path_buf());
        assert!(
            matches!(acquired, Err(UpdateError::LockConflict(_))),
            "a fresh foreign lock must NOT be reclaimed, got {acquired:?}"
        );
    }

    // CB-2 / AC4 happy path: backup copies the binary and returns an equal-length file.
    #[tokio::test]
    async fn backup_current_copies_and_validates_length() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("rustain");
        let bytes = b"the current running binary bytes";
        std::fs::write(&exe, bytes).unwrap();

        let replacer =
            DefaultBinaryReplacer::with_paths(exe, dir.path().to_path_buf()).expect("acquire");
        let backup = replacer.backup_current().await.expect("backup ok");

        assert!(backup.exists(), "backup file must exist");
        assert_eq!(
            std::fs::read(&backup).unwrap(),
            bytes,
            "backup content must match source byte-for-byte"
        );
    }

    // CB-2 / AC4: the truncation guard rejects a short backup (disk-full mid-copy).
    #[test]
    fn check_backup_len_rejects_truncation() {
        assert!(
            check_backup_len(1000, 1000).is_ok(),
            "equal lengths must pass"
        );
        let err = check_backup_len(1000, 512);
        assert!(
            matches!(err, Err(UpdateError::BackupFailed(_))),
            "a short backup must be rejected as BackupFailed, got {err:?}"
        );
    }
}
