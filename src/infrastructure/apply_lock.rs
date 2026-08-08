//! Cross-process advisory lock serializing patch application against one
//! workspace (Story 18.3a-d, `DF-17-3b-2`).
//!
//! # Two layers, both load bearing
//!
//! `PatchMergeBack` already holds a `tokio::sync::Mutex` that serializes
//! applies *inside* one process. That mutex says nothing about a second OS
//! process — the supported daemon + CLI configuration — reaching the same
//! working tree. This module is layer 2, mirroring the two-layer pattern the
//! room journal already ships (`NodeJournal`'s `append_guard` + its `FileLock`
//! sidecar).
//!
//! # Why `flock`, non-blocking, and OS-released
//!
//! * **`libc::flock`, not `fcntl`.** `flock` locks are held on the *open file
//!   description*, so two independent `open()` calls contend even inside one
//!   process. POSIX record locks (`fcntl`) are held per **process** and would
//!   silently let a second in-process acquirer through. ⛔ Do not substitute
//!   `fcntl`, `fs2` or `fd-lock`.
//! * **`LOCK_NB`, not a bounded blocking acquire.** `ADR-19-1-03` sanctions
//!   holding a flock across a bounded await for the daemonless refresh
//!   election, where blocking is correct because the caller wants the value.
//!   Here a blocked apply would stall its caller behind an arbitrarily long
//!   `git apply`; an immediate typed refusal is the honest answer and is what
//!   `18-3a-e` renders.
//! * **OS-released, never `Drop`-released.** `[profile.release] panic =
//!   "abort"` is project-wide (`DF-CR-14-5-2`), so `Drop` never runs on panic
//!   and never runs under `kill -9`. The kernel releases a `flock` when the
//!   descriptor closes with the process, which is the only release mechanism
//!   that survives those paths. The `Drop` impl below is a courtesy for the
//!   clean path.
//!
//! # Why the path is derived from the CANONICALIZED workspace
//!
//! `PatchMergeBack.workspace` is stored verbatim — `startup.rs` never
//! canonicalizes — so two processes can reach one repository under two
//! spellings. Keying the lock on the path *you were handed* hands them two
//! different locks and both applies proceed. [`workspace_hash`] is the tree's
//! single `std::fs::canonicalize` caller and already keys the room id, so
//! embedding it in the lock filename makes the canonical identity the thing
//! being locked, exactly as `{room-id}.lock` already does for the journal.
//!
//! [`workspace_hash`]: crate::infrastructure::paths::workspace_hash

use std::path::{Path, PathBuf};

/// Exclusive cross-process claim on one workspace's apply path.
///
/// Held for the duration of one `PatchMergeBack::apply` critical section:
/// acquired before the authorization gate, released when this value drops (or
/// when the process dies, whichever comes first).
#[derive(Debug)]
pub struct ApplyLock {
    /// Owns the descriptor the kernel associates the `flock` with. Dropping
    /// this releases the lock even if the explicit `LOCK_UN` below is skipped.
    file: std::fs::File,
    path: PathBuf,
}

/// Location of the apply lock for `workspace`.
///
/// A `.lock` sidecar under `{workspace}/.rustain/`, beside `daemon.lock`, whose
/// **name carries the canonical workspace hash**. Both halves matter: the
/// directory makes the lock intrinsically shared by everyone addressing this
/// tree, and the hash makes a non-canonical spelling produce a *visibly
/// different* file rather than a silently-shared one.
#[must_use]
pub fn apply_lock_path(workspace: &Path) -> PathBuf {
    workspace.join(".rustain").join(format!(
        "apply-{}.lock",
        crate::infrastructure::paths::workspace_hash(workspace)
    ))
}

impl ApplyLock {
    /// Try to claim the workspace's apply path without blocking.
    ///
    /// Returns [`ApplyLockError::Busy`] the moment another open file
    /// description already holds the lock.
    pub async fn try_acquire(workspace: &Path) -> Result<Self, ApplyLockError> {
        let path = apply_lock_path(workspace);
        tokio::task::spawn_blocking(move || acquire(path))
            .await
            .map_err(|error| {
                ApplyLockError::Io(std::io::Error::other(format!(
                    "apply lock task failed: {error}"
                )))
            })?
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(unix)]
fn acquire(path: PathBuf) -> Result<ApplyLock, ApplyLockError> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // `truncate(false)`: the sidecar is a lock, not a channel — never destroy a
    // peer's file contents just to take a lock on it.
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&path)?;
    // SAFETY: `file` owns a valid descriptor for the lifetime of the guard.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return Err(ApplyLockError::Busy);
        }
        return Err(ApplyLockError::Io(error));
    }
    Ok(ApplyLock { file, path })
}

/// Non-unix fallback: exclusive creation stands in for the advisory lock.
///
/// ⚠ Copied deliberately from `DaemonSingletonLock`, **not** from
/// `NodeJournal::FileLock` — the latter has no non-unix arm at all, so on
/// Windows it is a silent no-op. A lock that silently does nothing is worse
/// than no lock, because callers believe they are serialized.
///
/// ⚠ **Not crash-OS-released.** Unlike the unix `flock` arm — released by the
/// kernel when the descriptor closes with the process — this arm relies on
/// `Drop::remove_file`, which does not run under `[profile.release] panic =
/// "abort"` (`DF-CR-14-5-2`) or `kill`. A hard crash can therefore leave a
/// stale `apply-<hash>.lock` that permanently returns `WorkspaceBusy` until
/// the file is removed by hand. Non-unix apply is outside the currently
/// supported (Linux) target set; if it is ever supported this needs a real
/// OS-released primitive or an explicit stale-lock recovery step.
#[cfg(not(unix))]
fn acquire(path: PathBuf) -> Result<ApplyLock, ApplyLockError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                ApplyLockError::Busy
            } else {
                ApplyLockError::Io(error)
            }
        })?;
    Ok(ApplyLock { file, path })
}

impl Drop for ApplyLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: the descriptor remains valid until this drop completes.
            unsafe {
                libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
            }
        }
        #[cfg(not(unix))]
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ApplyLockError {
    /// Another open file description — in this process or another — already
    /// holds the workspace's apply lock.
    #[error("another process is applying a patch to this workspace")]
    Busy,
    #[error("apply lock could not be taken: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lock_path_is_keyed_by_the_canonical_workspace_not_the_spelling() {
        let root = tempfile::tempdir().expect("tempdir");
        let real = root.path().join("repo");
        std::fs::create_dir_all(&real).expect("mkdir");
        let link = root.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        let direct = apply_lock_path(&real);
        let via_symlink = apply_lock_path(&link);
        let via_dot_dot = apply_lock_path(&real.join("..").join("repo"));

        assert_eq!(
            direct.file_name(),
            via_symlink.file_name(),
            "a symlinked spelling must take the same lock"
        );
        assert_eq!(
            direct.file_name(),
            via_dot_dot.file_name(),
            "a `..`-relative spelling must take the same lock"
        );
    }

    #[tokio::test]
    async fn a_second_open_file_description_is_refused_without_blocking() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let held = ApplyLock::try_acquire(workspace.path())
            .await
            .expect("first acquire");
        let refused = ApplyLock::try_acquire(workspace.path()).await;
        assert!(
            matches!(refused, Err(ApplyLockError::Busy)),
            "flock is held per open file description, so the second acquire must be refused"
        );
        drop(held);
        ApplyLock::try_acquire(workspace.path())
            .await
            .expect("released locks are re-acquirable");
    }
}
