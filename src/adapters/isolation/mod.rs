use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use tokio::process::Command;

use crate::domain::clock::{Clock, SystemClock};
use crate::domain::models::{IsolationError, IsolationHandle, ProvisioningTier, UnifiedDiff};
use crate::domain::ports::IsolationProvider;

#[derive(Clone)]
pub struct CowIsolationProvider {
    clock: Arc<dyn Clock>,
    periodic_reaper_started: Arc<AtomicBool>,
    entry_hook: EntryHook,
}

impl Default for CowIsolationProvider {
    fn default() -> Self {
        Self::new(Arc::new(SystemClock::default()))
    }
}

impl CowIsolationProvider {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            periodic_reaper_started: Arc::new(AtomicBool::new(false)),
            entry_hook: EntryHook::none(),
        }
    }

    /// Test seam: see [`EntryHook`].
    #[cfg(test)]
    fn with_entry_hook(
        mut self,
        hook: impl Fn(&Path) -> Option<std::io::Error> + Send + Sync + 'static,
    ) -> Self {
        self.entry_hook = EntryHook(Some(Arc::new(hook)));
        self
    }

    /// Wall clock read through the injected `Clock`, converted to `u64` ms at
    /// this single boundary (AC6). Returns `FailClosed` (never panics) on a
    /// pre-epoch reading — a panic would be fail-open-by-crash, and under
    /// `panic = "abort"` (release) it would abort the whole process (P12).
    fn now_ms(&self) -> Result<u64, IsolationError> {
        u64::try_from(self.clock.wall_now_ms()).map_err(|_| IsolationError::FailClosed {
            reason: "wall clock reported a pre-epoch time".to_string(),
        })
    }

    /// Single chokepoint for the backend ladder (DD2 "ladder-as-seam, ONE
    /// proven rung"). Today only scratch-dir copy is implemented; R2 drops
    /// reflink/overlayfs in here. `BackendUnavailable` is the documented R2
    /// hook and is intentionally not constructed yet.
    fn select_backend(&self) -> Result<ProvisioningTier, IsolationError> {
        Ok(ProvisioningTier::ScratchCopy)
    }

    fn ensure_periodic_reaper(&self) {
        if self
            .periodic_reaper_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let started = self.periodic_reaper_started.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                if tokio::task::spawn_blocking(reap_dead_owned_scratch)
                    .await
                    .is_err()
                {
                    started.store(false, Ordering::Release);
                    break;
                }
            }
        });
    }
}

#[async_trait]
impl IsolationProvider for CowIsolationProvider {
    async fn start(&self, lower: &Path) -> Result<IsolationHandle, IsolationError> {
        let backend = match self.select_backend() {
            Ok(backend) => backend,
            Err(IsolationError::BackendUnavailable { backend, reason }) => {
                tracing::warn!(backend, reason, "isolation backend unavailable");
                return Err(IsolationError::FailClosed {
                    reason: "no isolation backend available".to_string(),
                });
            }
            Err(err) => return Err(err),
        };

        // Liveness-scoped orphan hygiene: only directories carrying our
        // ownership marker are considered, and a live PID is never age-reaped.
        tokio::task::spawn_blocking(reap_dead_owned_scratch)
            .await
            .map_err(|join_err| IsolationError::FailClosed {
                reason: format!("orphan reaper task failed: {join_err}"),
            })?;
        self.ensure_periodic_reaper();
        let temp_dir = tempfile::Builder::new()
            .prefix("rustain-isolation-")
            .tempdir()
            .map_err(|source| IsolationError::FailClosed {
                reason: format!("failed to create scratch dir: {source}"),
            })?;
        // Publish ownership immediately after allocation, before any expensive
        // copy work: a SIGKILL at any later point leaves a reapable record.
        tokio::fs::write(
            temp_dir.path().join(OWNER_MARKER),
            format!("{}\n{}\n", std::process::id(), PROCESS_NONCE.as_str()),
        )
        .await
        .map_err(|source| IsolationError::FailClosed {
            reason: format!("failed to write scratch ownership marker: {source}"),
        })?;

        // Copy on a blocking thread — `copy_dir_recursive` is fully recursive
        // sync I/O and must not stall a tokio worker on a real workspace
        // (large `target/`, vendored deps). (P14)
        let lower_owned = lower.to_path_buf();
        let dest_owned = temp_dir.path().to_path_buf();
        let hook = self.entry_hook.clone();
        tokio::task::spawn_blocking(move || {
            copy_tree_without_git(&lower_owned, &dest_owned, &hook)
        })
        .await
        .map_err(|join_err| IsolationError::FailClosed {
            reason: format!("scratch copy task failed: {join_err}"),
        })??;

        init_git_baseline(temp_dir.path()).await?;
        tracing::info!(backend = ?backend, path = %temp_dir.path().display(), "isolation active");
        IsolationHandle::new(temp_dir, backend, self.now_ms()?)
    }

    async fn diff(&self, h: &IsolationHandle) -> Result<UnifiedDiff, IsolationError> {
        // Stage everything first (P2): a bare `git diff` compares the working
        // tree to the INDEX and silently misses new/committed files — the
        // common case for a coding subagent. `git add -A` then `git diff
        // --cached` captures new + modified + deleted entries against the
        // baseline commit.
        run_git(h.path(), &["add", "-A"]).await?;
        let output = Command::new("git")
            .arg("diff")
            .arg("--cached")
            .current_dir(h.path())
            .output()
            .await
            .map_err(|source| IsolationError::GitFailed {
                cmd: "git diff --cached",
                cwd: h.path().to_path_buf(),
                stderr: source.to_string(),
            })?;
        if !output.status.success() {
            return Err(IsolationError::GitFailed {
                cmd: "git diff --cached",
                cwd: h.path().to_path_buf(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        // Merge-back is write-capable, so lossy conversion is forbidden:
        // corrupting a path or hunk and then applying it would violate the
        // reviewed artifact. Git quotes non-UTF-8 paths by default; if a caller
        // disables that protection and raw bytes remain, fail closed.
        let diff =
            String::from_utf8(output.stdout).map_err(|error| IsolationError::FailClosed {
                reason: format!("git diff output was not valid UTF-8: {error}"),
            })?;
        Ok(UnifiedDiff::new(h.backend(), diff))
    }

    async fn stop(&self, h: IsolationHandle) -> Result<(), IsolationError> {
        // Defense against the §3.7 #6 teardown-root-resolve family: refuse to
        // tear down if the clone's canonical root changed between `start` and
        // `stop` (e.g. the tempdir was swapped to a symlink to the real tree).
        // The actual `remove_dir_all` is delegated to `TempDir`'s `Drop`, which
        // on Linux uses an openat-based hardened removal (mitigating the
        // classic recursive-swap TOCTOU, §3.7 #5). §3.7 #6 (teardown-root-resolve)
        // is armed by `stop_refuses_when_canonical_root_changed` below; the §3.7
        // #5 check→drop TOCTOU window is an ACCEPTED residual (TempDir's
        // openat-based removal is the mitigation; no portable armed test — R2). (P7)
        let canonical =
            h.path()
                .canonicalize()
                .map_err(|source| IsolationError::CanonicalizeRoot {
                    path: h.path().to_path_buf(),
                    source,
                })?;
        if canonical != h.canonical_root() {
            return Err(IsolationError::TeardownRefused {
                reason: format!(
                    "scratch dir canonical root changed from {} to {}",
                    h.canonical_root().display(),
                    canonical.display()
                ),
            });
        }
        h.mark_stopped();
        Ok(())
    }
}

const SCRATCH_PREFIX: &str = "rustain-isolation-";
const OWNER_MARKER: &str = ".rustain-isolation-owner";
static PROCESS_NONCE: LazyLock<String> = LazyLock::new(|| nanoid::nanoid!(24));

fn reap_dead_owned_scratch() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(SCRATCH_PREFIX) {
            continue;
        }
        let path = entry.path();
        let Ok(marker) = std::fs::read_to_string(path.join(OWNER_MARKER)) else {
            continue;
        };
        let mut lines = marker.lines();
        let Some(pid) = lines.next().and_then(|value| value.parse::<u32>().ok()) else {
            continue;
        };
        let Some(nonce) = lines.next() else {
            continue;
        };
        let same_live_process = pid == std::process::id() && nonce == PROCESS_NONCE.as_str();
        if same_live_process || (pid != std::process::id() && process_is_live(pid)) {
            continue;
        }
        if let Err(error) = std::fs::remove_dir_all(&path) {
            tracing::warn!(path = %path.display(), %error, "failed to reap dead owned isolation scratch");
        } else {
            tracing::info!(path = %path.display(), pid, "reaped dead owned isolation scratch");
        }
    }
}

#[cfg(unix)]
fn process_is_live(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 performs no mutation; it only asks the kernel whether
    // the process exists and whether this process may signal it.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_is_live(_pid: u32) -> bool {
    // Conservative fallback: unsupported platforms never delete another
    // process's owned workspace.
    true
}

/// Directories never copied into the scratch clone (build output, heavy vendor
/// trees, editor cruft). Keeps the clone proportional to what the child edits
/// and bounds copy cost (P14).
const SKIP_DIR_NAMES: &[&str] = &["target", "node_modules", ".next", ".nuxt", "dist", "build"];

/// Test seam (infra-2): called with each listed source entry before it is
/// processed. It may change the file system to stand in for a concurrent
/// writer, or return an `io::Error` that replaces a file entry's copy result.
/// Production builds carry a zero-sized value that does nothing.
#[cfg(test)]
#[derive(Clone)]
struct EntryHook(Option<Arc<dyn Fn(&Path) -> Option<std::io::Error> + Send + Sync>>);

#[cfg(not(test))]
#[derive(Clone)]
struct EntryHook;

impl EntryHook {
    #[cfg(test)]
    fn none() -> Self {
        Self(None)
    }

    #[cfg(not(test))]
    fn none() -> Self {
        Self
    }

    #[cfg(test)]
    fn call(&self, path: &Path) -> Option<std::io::Error> {
        self.0.as_ref().and_then(|hook| hook(path))
    }

    #[cfg(not(test))]
    fn call(&self, _path: &Path) -> Option<std::io::Error> {
        None
    }
}

/// True when `error` is `NotFound`, the SOURCE `path` is itself gone, and the
/// workspace ROOT is still there: a concurrent deletion or rename of one entry
/// (rsync's "file has vanished"). A `NotFound` with the source present (a missing
/// destination, a broken link) is not; neither is any `NotFound` once the root
/// itself is gone (the whole workspace vanished: fail closed).
fn source_vanished(root: &Path, path: &Path, error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound
        && matches!(
            std::fs::symlink_metadata(path),
            Err(probe) if probe.kind() == std::io::ErrorKind::NotFound
        )
        && root_present(root)
}

fn root_present(root: &Path) -> bool {
    std::fs::symlink_metadata(root).is_ok()
}

fn copy_tree_without_git(from: &Path, to: &Path, hook: &EntryHook) -> Result<(), IsolationError> {
    let from = from.to_path_buf();
    let to = to.to_path_buf();
    copy_dir_recursive(&from, &to, &from, hook)?;
    // A root that vanished during the traversal leaves a partial scratch copy of
    // a workspace that no longer exists: fail closed.
    match std::fs::symlink_metadata(&from) {
        Ok(_) => Ok(()),
        Err(source) => Err(IsolationError::CopyFailed { from, to, source }),
    }
}

/// Copies `src` into `dst`. The scratch copy has never been a point-in-time
/// snapshot (a file modified during the copy is copied in whatever state it
/// had); a listed entry whose source disappears before it is copied (an atomic
/// rename over a live workspace file) is skipped instead of failing the launch.
/// Every other error, a destination-side `NotFound` and a missing ROOT included,
/// still fails closed.
fn copy_dir_recursive(
    src: &Path,
    dst: &Path,
    root: &Path,
    hook: &EntryHook,
) -> Result<(), IsolationError> {
    std::fs::create_dir_all(dst).map_err(|source| IsolationError::CopyFailed {
        from: src.to_path_buf(),
        to: dst.to_path_buf(),
        source,
    })?;

    let entries = match std::fs::read_dir(src) {
        Ok(entries) => entries,
        Err(source) if src != root && source_vanished(root, src, &source) => {
            tracing::debug!(dir = %src.display(), "source directory vanished during scratch copy; skipped");
            // Nothing of it was copied yet: leave no partial copy of it behind.
            return match std::fs::remove_dir_all(dst) {
                Err(source) if source.kind() != std::io::ErrorKind::NotFound => {
                    Err(IsolationError::CopyFailed {
                        from: src.to_path_buf(),
                        to: dst.to_path_buf(),
                        source,
                    })
                }
                _ => Ok(()),
            };
        }
        Err(source) => {
            return Err(IsolationError::CopyFailed {
                from: src.to_path_buf(),
                to: dst.to_path_buf(),
                source,
            });
        }
    };
    for entry in entries {
        let entry = entry.map_err(|source| IsolationError::CopyFailed {
            from: src.to_path_buf(),
            to: dst.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let name = entry.file_name();
        let injected = hook.call(&path);
        // Exclude `.git` (the clone gets its own fresh repo; excluded at every
        // depth), the source ROOT's ownership marker only (the destination
        // publishes its own; a nested user file of the same name is preserved),
        // and build-output dirs (P14).
        if name == ".git" || path == root.join(OWNER_MARKER) || path == root.join(".git") {
            continue;
        }
        if path.is_dir() && SKIP_DIR_NAMES.iter().any(|d| *d == name.to_string_lossy()) {
            continue;
        }
        let target = dst.join(&name);
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(source) if source_vanished(root, &path, &source) => {
                tracing::debug!(entry = %path.display(), "source entry vanished during scratch copy; skipped");
                continue;
            }
            Err(source) => {
                return Err(IsolationError::CopyFailed {
                    from: path.clone(),
                    to: target.clone(),
                    source,
                });
            }
        };
        if file_type.is_dir() {
            copy_dir_recursive(&path, &target, root, hook)?;
        } else if file_type.is_file() {
            let copied = match injected {
                Some(error) => Err(error),
                None => std::fs::copy(&path, &target).map(drop),
            };
            match copied {
                Ok(()) => {}
                Err(source) if source_vanished(root, &path, &source) => {
                    tracing::debug!(entry = %path.display(), "source file vanished during scratch copy; skipped");
                }
                Err(source) => {
                    return Err(IsolationError::CopyFailed {
                        from: path.clone(),
                        to: target.clone(),
                        source,
                    });
                }
            }
        } else if file_type.is_symlink() {
            // §3.7 #3 symlink-out: refuse to recreate a symlink whose target
            // resolves outside the clone root — otherwise the clone carries a
            // live escape vector (e.g. `secrets -> /home/user/.ssh`,
            // `../sibling`) that `git`/exec paths can follow out. Skip + warn
            // (P8).
            if symlink_target_escapes(&path, src, root) {
                tracing::warn!(
                    link = %path.display(),
                    "skipping symlink that escapes the clone root (§3.7 #3)"
                );
                continue;
            }
            copy_symlink(&path, &target)?;
        } else {
            // P16: FIFOs / sockets / devices are neither copied nor silently
            // dropped — warn so the fidelity gap is observable.
            tracing::warn!(
                entry = %path.display(),
                "skipping non-regular file (FIFO/socket/device) during scratch copy"
            );
        }
    }
    Ok(())
}

/// True if the symlink at `link_path` points outside `root` (absolute target,
/// or a relative target that resolves above the root). Defends §3.7 family #3.
fn symlink_target_escapes(link_path: &Path, src_dir: &Path, root: &Path) -> bool {
    let target = match std::fs::read_link(link_path) {
        Ok(t) => t,
        Err(_) => return false, // let `copy_symlink` surface the error
    };
    if target.is_absolute() {
        return true;
    }
    let root_canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    if let Ok(canon) = src_dir.join(&target).canonicalize() {
        return !canon.starts_with(&root_canon);
    }
    // Target does not exist yet: resolve lexically from `src_dir`.
    let mut acc = match src_dir.canonicalize() {
        Ok(c) => c,
        Err(_) => src_dir.to_path_buf(),
    };
    for comp in target.components() {
        use std::path::Component;
        match comp {
            Component::ParentDir => {
                acc.pop();
            }
            Component::Normal(c) => acc.push(c),
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => return true,
        }
    }
    !acc.starts_with(&root_canon)
}

#[cfg(unix)]
fn copy_symlink(path: &Path, target: &Path) -> Result<(), IsolationError> {
    let link = std::fs::read_link(path).map_err(|source| IsolationError::CopyFailed {
        from: path.to_path_buf(),
        to: target.to_path_buf(),
        source,
    })?;
    std::os::unix::fs::symlink(link, target).map_err(|source| IsolationError::CopyFailed {
        from: path.to_path_buf(),
        to: target.to_path_buf(),
        source,
    })
}

#[cfg(windows)]
fn copy_symlink(path: &Path, target: &Path) -> Result<(), IsolationError> {
    let link = std::fs::read_link(path).map_err(|source| IsolationError::CopyFailed {
        from: path.to_path_buf(),
        to: target.to_path_buf(),
        source,
    })?;
    if path.is_dir() {
        std::os::windows::fs::symlink_dir(link, target)
    } else {
        std::os::windows::fs::symlink_file(link, target)
    }
    .map_err(|source| IsolationError::CopyFailed {
        from: path.to_path_buf(),
        to: target.to_path_buf(),
        source,
    })
}

async fn init_git_baseline(path: &Path) -> Result<(), IsolationError> {
    run_git(path, &["init", "--quiet"]).await?;
    run_git(
        path,
        &["config", "user.email", "rustain-isolation@example.invalid"],
    )
    .await?;
    run_git(path, &["config", "user.name", "Rustain Isolation"]).await?;
    run_git(path, &["add", "-A"]).await?;
    run_git(
        path,
        &[
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "isolation baseline",
        ],
    )
    .await?;
    Ok(())
}

async fn run_git(path: &Path, args: &[&'static str]) -> Result<(), IsolationError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .await
        .map_err(|source| IsolationError::GitFailed {
            cmd: "git",
            cwd: path.to_path_buf(),
            stderr: source.to_string(),
        })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(IsolationError::GitFailed {
            cmd: "git",
            cwd: path.to_path_buf(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::clock::MockClock;
    use std::time::Duration;

    #[tokio::test]
    async fn scratch_copy_diff_is_non_empty_and_parent_unchanged() {
        let parent = tempfile::TempDir::new().unwrap();
        std::fs::write(parent.path().join("a.txt"), "before\n").unwrap();
        let provider = CowIsolationProvider::new(Arc::new(MockClock::at_wall_ms(42)));
        let handle = provider.start(parent.path()).await.unwrap();
        assert_eq!(handle.created_at_ms(), 42);
        std::fs::write(handle.path().join("a.txt"), "after\n").unwrap();
        let diff = provider.diff(&handle).await.unwrap();
        assert!(!diff.is_empty());
        assert_eq!(
            std::fs::read_to_string(parent.path().join("a.txt")).unwrap(),
            "before\n"
        );
        provider.stop(handle).await.unwrap();
    }

    // P2 proof: a bare `git diff` would miss newly-created (untracked) files —
    // the common case for a coding subagent. After the `git add -A` +
    // `git diff --cached` fix, a NEW file appears in the delta.
    #[tokio::test]
    async fn scratch_copy_diff_captures_new_and_modified_files() {
        let parent = tempfile::TempDir::new().unwrap();
        std::fs::write(parent.path().join("tracked.txt"), "one\n").unwrap();
        let provider = CowIsolationProvider::new(Arc::new(MockClock::at_wall_ms(1)));
        let handle = provider.start(parent.path()).await.unwrap();
        std::fs::write(handle.path().join("tracked.txt"), "two\n").unwrap(); // modify
        std::fs::write(handle.path().join("brand_new.txt"), "new\n").unwrap(); // create
        let diff = provider.diff(&handle).await.unwrap();
        assert!(
            diff.diff.contains("tracked.txt"),
            "modified file missing from delta"
        );
        assert!(
            diff.diff.contains("brand_new.txt"),
            "new file missing from delta (P2 regression)"
        );
        provider.stop(handle).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_capable_diff_fails_closed_on_non_utf8_output() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;

        let parent = tempfile::TempDir::new().unwrap();
        std::fs::write(parent.path().join("tracked.txt"), "one\n").unwrap();
        let provider = CowIsolationProvider::new(Arc::new(MockClock::at_wall_ms(1)));
        let handle = provider.start(parent.path()).await.unwrap();
        let config = Command::new("git")
            .args(["config", "core.quotePath", "false"])
            .current_dir(handle.path())
            .output()
            .await
            .unwrap();
        assert!(config.status.success());
        std::fs::write(
            handle.path().join(OsStr::from_bytes(b"invalid-\xff.txt")),
            "body\n",
        )
        .unwrap();

        let error = provider.diff(&handle).await.unwrap_err();
        assert!(
            matches!(error, IsolationError::FailClosed { ref reason } if reason.contains("not valid UTF-8")),
            "write-capable merge-back must reject, never lossy-convert, an invalid diff: {error}"
        );
        provider.stop(handle).await.unwrap();
    }

    // P8 proof: a symlink pointing outside the clone root is NOT reproduced in
    // the scratch clone (no live escape vector).
    #[tokio::test]
    async fn scratch_copy_skips_symlinks_escaping_the_root() {
        let parent = tempfile::TempDir::new().unwrap();
        std::fs::write(parent.path().join("real.txt"), "x\n").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/hostname", parent.path().join("evil_abs")).unwrap();
            std::os::unix::fs::symlink("../outside", parent.path().join("evil_rel")).unwrap();
        }
        let provider = CowIsolationProvider::new(Arc::new(MockClock::at_wall_ms(1)));
        let handle = provider.start(parent.path()).await.unwrap();
        #[cfg(unix)]
        {
            assert!(
                !handle.path().join("evil_abs").exists(),
                "absolute escape symlink must not be cloned"
            );
            assert!(
                !handle.path().join("evil_rel").exists(),
                "relative escape symlink must not be cloned"
            );
        }
        let _ = provider;
        let _ = handle;
    }

    #[tokio::test]
    async fn unified_diff_round_trips_byte_identically() {
        let diff = UnifiedDiff::new(ProvisioningTier::ScratchCopy, "diff --git a/a b/a\n".into());
        let encoded = serde_json::to_string(&diff).unwrap();
        let decoded: UnifiedDiff = serde_json::from_str(&encoded).unwrap();
        assert_eq!(diff, decoded);
    }

    #[test]
    fn mock_clock_is_advanceable_for_isolation() {
        let clock = MockClock::at_wall_ms(7);
        clock.advance(Duration::from_millis(5));
        assert!(clock.wall_now_ms() >= 7);
    }
    // P10 / AC2 positive control (Murat refinement): a planted "leak" (a clone
    // file copied into the parent) MUST be caught by a parent-tree snapshot
    // diff. This arms the byte-identity detector so a mutant that silently
    // writes the parent would turn this test RED — without it, "parent
    // unchanged" is not a live detector.
    #[tokio::test]
    async fn scratch_clone_leak_into_parent_is_detectable() {
        let parent = tempfile::TempDir::new().unwrap();
        std::fs::write(parent.path().join("seed.txt"), "s\n").unwrap();
        let provider = CowIsolationProvider::new(Arc::new(MockClock::at_wall_ms(1)));
        let handle = provider.start(parent.path()).await.unwrap();
        let snap_before = snapshot_tree(parent.path());
        // In-bounds edit (clone only) — parent must be byte-identical.
        std::fs::write(handle.path().join("seed.txt"), "edited\n").unwrap();
        assert_eq!(
            snap_before,
            snapshot_tree(parent.path()),
            "in-clone edit leaked into the parent tree"
        );
        // PLANTED LEAK — the detector MUST catch this.
        std::fs::copy(
            handle.path().join("seed.txt"),
            parent.path().join("seed.txt"),
        )
        .unwrap();
        assert_ne!(
            snap_before,
            snapshot_tree(parent.path()),
            "planted leak was NOT detected — the byte-identity detector is vacuous"
        );
        provider.stop(handle).await.unwrap();
    }

    /// Recursive file-name → contents snapshot of a directory tree (the AC2
    /// byte-identity detector).
    fn snapshot_tree(root: &Path) -> std::collections::HashMap<String, String> {
        let mut map = std::collections::HashMap::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Ok(contents) = std::fs::read_to_string(&path) {
                    let rel = path.strip_prefix(root).unwrap_or(&path);
                    map.insert(rel.display().to_string(), contents);
                }
            }
        }
        map
    }
    // §3.7 #9: the canonical root is captured ONCE at creation; all later
    // teardown checks compare to it (handles symlinked `$TMPDIR`, e.g. macOS
    // /tmp → /private/tmp — compare against the canonical root, not the join-path).
    #[tokio::test]
    async fn scratch_clone_stores_canonical_root() {
        let parent = tempfile::TempDir::new().unwrap();
        std::fs::write(parent.path().join("x"), "").unwrap();
        let provider = CowIsolationProvider::new(Arc::new(MockClock::at_wall_ms(1)));
        let handle = provider.start(parent.path()).await.unwrap();
        let expected = handle.path().canonicalize().unwrap();
        assert_eq!(handle.canonical_root(), expected);
        provider.stop(handle).await.unwrap();
    }

    // §3.7 #3 expanded: an in-bounds (relative, inside-root) symlink MUST be
    // preserved in the clone; an escaping (absolute) symlink MUST be skipped.
    // Proves both the positive (mechanism can preserve) and the negative (escape
    // is blocked) — not a one-sided grep-style assertion.
    #[cfg(unix)]
    #[tokio::test]
    async fn scratch_clone_preserves_inbounds_symlink_and_skips_escape() {
        use std::os::unix::fs::symlink;
        let parent = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(parent.path().join("sub")).unwrap();
        std::fs::write(parent.path().join("sub").join("inner"), "i\n").unwrap();
        // in-bounds relative symlink (target resolves inside root) — preserved
        symlink("sub", parent.path().join("link_to_sub")).unwrap();
        // escaping absolute symlink — skipped (§3.7 #3)
        symlink("/etc/hostname", parent.path().join("evil")).unwrap();
        let provider = CowIsolationProvider::new(Arc::new(MockClock::at_wall_ms(1)));
        let handle = provider.start(parent.path()).await.unwrap();
        assert!(
            handle.path().join("link_to_sub").exists(),
            "in-bounds symlink must be preserved"
        );
        assert!(
            !handle.path().join("evil").exists(),
            "escape symlink must be skipped (§3.7 #3)"
        );
        provider.stop(handle).await.unwrap();
    }

    // AC3 fail-closed building block: a lower path that cannot be cloned (a file,
    // not a dir) makes `start()` return `Err` — the provider NEVER falls through
    // to "run unisolated". (The runner maps any `IsolationError` → launch `Err`,
    // so the child is refused, not launched against the real workspace.)
    #[tokio::test]
    async fn scratch_start_refuses_on_invalid_lower() {
        let parent = tempfile::TempDir::new().unwrap();
        let not_a_dir = parent.path().join("file");
        std::fs::write(&not_a_dir, "x").unwrap();
        let provider = CowIsolationProvider::new(Arc::new(MockClock::at_wall_ms(1)));
        let result = provider.start(&not_a_dir).await;
        assert!(
            result.is_err(),
            "start() must refuse (fail-closed) on an invalid lower, not fall through"
        );
    }

    // §3.7 #6 (teardown-root-resolve) — ARMED: if the clone's canonical root
    // changed between `start` and `stop` (e.g. the tempdir was swapped to a
    // symlink to the real tree), `stop()` MUST refuse (`TeardownRefused`) — a
    // `remove_dir_all` onto the real tree is a non-undoable blast radius.
    // Kill-criterion: deleting the canonical check in `stop()` → RED. Removal
    // still occurs via `TempDir`'s `Drop`; this arms the *alert* the guard
    // provides. Positive control: a consistent handle stops Ok.
    #[tokio::test]
    async fn stop_refuses_when_canonical_root_changed() {
        let provider = CowIsolationProvider::new(Arc::new(MockClock::at_wall_ms(1)));
        // A handle whose stored canonical_root deliberately mismatches its live path.
        let temp = tempfile::TempDir::new().unwrap();
        let live_canon = temp.path().canonicalize().unwrap();
        let mismatched = IsolationHandle::with_canonical_root_for_test(
            temp,
            live_canon.join("not_the_live_root"),
            0,
        );
        let err = provider.stop(mismatched).await.unwrap_err();
        assert!(
            matches!(err, IsolationError::TeardownRefused { .. }),
            "§3.7 #6: stop must refuse on canonical-root change, got {err:?}"
        );
        // Positive control: a consistent handle (stored root == live path) stops Ok.
        let ok_temp = tempfile::TempDir::new().unwrap();
        let ok_canon = ok_temp.path().canonicalize().unwrap();
        let consistent = IsolationHandle::with_canonical_root_for_test(ok_temp, ok_canon, 0);
        provider
            .stop(consistent)
            .await
            .expect("consistent handle stops Ok");
    }

    // §3.7 #4 (hardlink) — ARMED residual: a hardlink in the source is copied as
    // a MATERIALIZED regular file (distinct inode) — `std::fs::copy` reads bytes
    // + writes a new inode, so the clone's copy cannot mutate the original via a
    // shared inode. Pins the incidental safety against a future `linkat`-based
    // fast-path that would re-create the link. (Unix-only: hardlinks are posix.)
    #[cfg(unix)]
    #[tokio::test]
    async fn scratch_copy_hardlink_is_distinct_inode() {
        use std::os::unix::fs::MetadataExt;
        let parent = tempfile::TempDir::new().unwrap();
        let original = parent.path().join("original.txt");
        let linked = parent.path().join("hardlink.txt");
        std::fs::write(&original, "payload\n").unwrap();
        std::fs::hard_link(&original, &linked).unwrap();
        let src_ino = std::fs::metadata(&original).unwrap().ino();

        let provider = CowIsolationProvider::new(Arc::new(MockClock::at_wall_ms(1)));
        let handle = provider.start(parent.path()).await.unwrap();

        let clone_original = handle.path().join("original.txt");
        let clone_linked = handle.path().join("hardlink.txt");
        let clone_orig_ino = std::fs::metadata(&clone_original).unwrap().ino();
        let clone_link_ino = std::fs::metadata(&clone_linked).unwrap().ino();
        assert_ne!(
            clone_orig_ino, src_ino,
            "§3.7 #4: clone file must NOT share the source's inode (no escape vector)"
        );
        assert_ne!(
            clone_orig_ino, clone_link_ino,
            "§3.7 #4: the two clone files are distinct materialized inodes (not a re-created hardlink)"
        );
        // Writing through the clone's copy does NOT propagate to the source.
        std::fs::write(&clone_original, "mutated\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&original).unwrap(),
            "payload\n",
            "§3.7 #4: a clone write must not mutate the source via a shared inode"
        );
        provider.stop(handle).await.unwrap();
    }

    // §3.7 #1/#2 (traversal at the clone boundary) — the isolated runner hands
    // the child a `SecurityAdapter` rooted at the CLONE path
    // (in_process_runner.rs:207-209). This proves THAT object — the isolation
    // security boundary — rejects `../` (§3.7 #1) and absolute-path-outside
    // (§3.7 #2) escapes at the clone edge. (The clone-rooting itself is pinned
    // by the AC4 capstone; generic traversal coverage lives in security_adapter
    // unit tests.) Positive control: an in-clone path is accepted.
    #[tokio::test]
    async fn clone_rooted_security_rejects_traversal_at_clone_boundary() {
        use crate::adapters::security_adapter::SecurityAdapter;
        use crate::domain::ports::SecurityPort;

        let parent = tempfile::TempDir::new().unwrap();
        std::fs::write(parent.path().join("seed.txt"), "x").unwrap();
        let provider = CowIsolationProvider::new(Arc::new(MockClock::at_wall_ms(1)));
        let handle = provider.start(parent.path()).await.unwrap();
        // The exact object the isolated runner constructs (clone-rooted).
        let sec = SecurityAdapter::new(handle.path().to_path_buf());

        // §3.7 #1: `../` traversal is rejected outright (ParentDir component).
        let trav = sec.check_workspace_access(
            std::path::Path::new("../escapee.txt"),
            crate::domain::models::FileOperation::Write,
        );
        assert!(
            trav.is_err(),
            "§3.7 #1: ../ traversal must be rejected at the clone boundary"
        );

        // §3.7 #2: an absolute path to a file OUTSIDE the clone is rejected for Write.
        let outside = tempfile::TempDir::new().unwrap();
        let outside_file = outside.path().join("escape.txt");
        std::fs::write(&outside_file, "x").unwrap();
        let abs =
            sec.check_workspace_access(&outside_file, crate::domain::models::FileOperation::Write);
        assert!(
            abs.is_err(),
            "§3.7 #2: an absolute path outside the clone must be rejected for Write"
        );

        // Positive control: an in-clone path is accepted (the boundary is
        // functional, not a blanket deny).
        std::fs::write(handle.path().join("inside.txt"), "y").unwrap();
        let inside = sec.check_workspace_access(
            std::path::Path::new("inside.txt"),
            crate::domain::models::FileOperation::Read,
        );
        assert!(
            inside.is_ok(),
            "positive control: an in-clone path must be accepted"
        );

        provider.stop(handle).await.unwrap();
    }

    // ── infra-2: a source entry that vanishes mid-copy (rc5 flake) ──

    fn live_workspace() -> tempfile::TempDir {
        let parent = tempfile::TempDir::new().unwrap();
        std::fs::write(parent.path().join("keep.txt"), "keep\n").unwrap();
        std::fs::write(parent.path().join("vanish.txt"), "gone soon\n").unwrap();
        std::fs::create_dir(parent.path().join("sub")).unwrap();
        std::fs::write(parent.path().join("sub").join("also.txt"), "also\n").unwrap();
        std::fs::create_dir(parent.path().join("gone")).unwrap();
        std::fs::write(parent.path().join("gone").join("inner.txt"), "inner\n").unwrap();
        parent
    }

    fn provider_with(
        hook: impl Fn(&Path) -> Option<std::io::Error> + Send + Sync + 'static,
    ) -> CowIsolationProvider {
        CowIsolationProvider::new(Arc::new(MockClock::at_wall_ms(1))).with_entry_hook(hook)
    }

    /// Removes `name` from the source tree the moment the copier lists it.
    fn remove_when_listed(name: &'static str) -> impl Fn(&Path) -> Option<std::io::Error> {
        move |path: &Path| {
            if path.file_name() == Some(std::ffi::OsStr::new(name)) {
                if path.is_dir() {
                    std::fs::remove_dir_all(path).unwrap();
                } else {
                    std::fs::remove_file(path).unwrap();
                }
            }
            None
        }
    }

    // K-R1 (RED before the tolerance): through `start`, a listed file that is
    // renamed or deleted before its copy no longer fails the launch.
    #[tokio::test]
    async fn scratch_copy_skips_a_file_that_vanishes_mid_copy() {
        let parent = live_workspace();
        let provider = provider_with(remove_when_listed("vanish.txt"));
        let handle = provider.start(parent.path()).await.unwrap();
        assert!(!handle.path().join("vanish.txt").exists());
        assert_eq!(
            std::fs::read_to_string(handle.path().join("keep.txt")).unwrap(),
            "keep\n"
        );
        assert_eq!(
            std::fs::read_to_string(handle.path().join("sub").join("also.txt")).unwrap(),
            "also\n"
        );
        // Every surviving file is byte-identical, the nested one included.
        assert_eq!(
            std::fs::read_to_string(handle.path().join("gone").join("inner.txt")).unwrap(),
            "inner\n"
        );
        provider.stop(handle).await.unwrap();
    }

    // K-R8 (infra-2 review R3): the workspace ROOT vanishing mid-traversal is not
    // "a vanished entry". Once the hook has removed the root, every listed entry's
    // copy fails with NotFound and its source is gone too; `start` must still fail.
    #[tokio::test]
    async fn scratch_copy_fails_closed_when_the_root_vanishes_mid_copy() {
        let parent = live_workspace();
        let root = parent.path().to_path_buf();
        let provider = provider_with(move |path: &Path| {
            if path.file_name() == Some(std::ffi::OsStr::new("vanish.txt")) {
                std::fs::remove_dir_all(&root).unwrap();
            }
            None
        });
        let error = provider.start(parent.path()).await.unwrap_err();
        assert!(
            matches!(&error, IsolationError::CopyFailed { source, .. }
                if source.kind() == std::io::ErrorKind::NotFound),
            "{error:?}"
        );
    }

    // K-R2: a listed subdirectory that vanishes leaves no destination directory.
    #[tokio::test]
    async fn scratch_copy_skips_a_directory_that_vanishes_mid_copy() {
        let parent = live_workspace();
        let provider = provider_with(remove_when_listed("gone"));
        let handle = provider.start(parent.path()).await.unwrap();
        assert!(
            !handle.path().join("gone").exists(),
            "no partial copy of the vanished directory"
        );
        assert!(handle.path().join("keep.txt").is_file());
        provider.stop(handle).await.unwrap();
    }

    // K-R3: any other error for a present file still fails closed, naming it.
    #[tokio::test]
    async fn scratch_copy_still_fails_closed_on_a_non_notfound_error() {
        let parent = live_workspace();
        let provider = provider_with(|path: &Path| {
            (path.file_name() == Some(std::ffi::OsStr::new("keep.txt")))
                .then(|| std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        });
        let error = provider.start(parent.path()).await.unwrap_err();
        assert!(
            matches!(&error, IsolationError::CopyFailed { from, source, .. }
                if from.ends_with("keep.txt") && source.kind() == std::io::ErrorKind::PermissionDenied),
            "{error:?}"
        );
    }

    // K-R4: a missing ROOT is never "vanished": the launch fails.
    #[tokio::test]
    async fn scratch_copy_fails_closed_when_the_root_is_missing() {
        let parent = tempfile::TempDir::new().unwrap();
        let provider = CowIsolationProvider::new(Arc::new(MockClock::at_wall_ms(1)));
        let error = provider
            .start(&parent.path().join("missing"))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, IsolationError::CopyFailed { source, .. }
                if source.kind() == std::io::ErrorKind::NotFound),
            "{error:?}"
        );
    }

    // K-R5 (positive control): an installed but inert hook copies everything the
    // exclusions allow, so K-R1..K-R3 skip only what they remove.
    #[tokio::test]
    async fn scratch_copy_with_an_inert_hook_copies_every_entry() {
        let parent = live_workspace();
        let provider = provider_with(|_: &Path| None);
        let handle = provider.start(parent.path()).await.unwrap();
        for rel in ["keep.txt", "vanish.txt", "sub/also.txt", "gone/inner.txt"] {
            assert!(handle.path().join(rel).is_file(), "{rel} must be copied");
        }
        provider.stop(handle).await.unwrap();
    }

    // K-R7: a destination-side NotFound (the source is still there) fails closed.
    #[test]
    fn scratch_copy_fails_closed_when_the_destination_parent_vanishes() {
        let parent = live_workspace();
        let scratch = tempfile::TempDir::new().unwrap();
        let dst = scratch.path().to_path_buf();
        let hook = EntryHook(Some(Arc::new({
            let dst = dst.clone();
            move |path: &Path| {
                if path.file_name() == Some(std::ffi::OsStr::new("also.txt")) {
                    std::fs::remove_dir_all(dst.join("sub")).unwrap();
                }
                None
            }
        })));
        let error = copy_dir_recursive(parent.path(), &dst, parent.path(), &hook).unwrap_err();
        assert!(
            matches!(&error, IsolationError::CopyFailed { from, source, .. }
                if from.ends_with("also.txt") && source.kind() == std::io::ErrorKind::NotFound),
            "{error:?}"
        );
    }
}
