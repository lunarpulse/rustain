use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::process::Command;

use crate::domain::clock::{Clock, SystemClock};
use crate::domain::models::{IsolationError, IsolationHandle, ProvisioningTier, UnifiedDiff};
use crate::domain::ports::IsolationProvider;

#[derive(Clone)]
pub struct CowIsolationProvider {
    clock: Arc<dyn Clock>,
}

impl Default for CowIsolationProvider {
    fn default() -> Self {
        Self::new(Arc::new(SystemClock::default()))
    }
}

impl CowIsolationProvider {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self { clock }
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

        // §3.7 #8 (orphan-cleanup): R1 ships NO reaper — a crashed/killed run
        // can leak `rustain-isolation-*` dirs. Orphan hygiene (reap stale dirs
        // by nonce/age on `start`, with a sibling NON-owned dir surviving the
        // sweep) is deferred to R2; `TempDir`'s own `Drop` covers the normal +
        // panic-terminal teardown paths. (DD4 #8 — accepted R1 residual.)
        let temp_dir = tempfile::Builder::new()
            .prefix("rustain-isolation-")
            .tempdir()
            .map_err(|source| IsolationError::FailClosed {
                reason: format!("failed to create scratch dir: {source}"),
            })?;

        // Copy on a blocking thread — `copy_dir_recursive` is fully recursive
        // sync I/O and must not stall a tokio worker on a real workspace
        // (large `target/`, vendored deps). (P14)
        let lower_owned = lower.to_path_buf();
        let dest_owned = temp_dir.path().to_path_buf();
        tokio::task::spawn_blocking(move || copy_tree_without_git(&lower_owned, &dest_owned))
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
        // R1 fidelity bar is low (the delta is inert — never applied). We keep
        // a `String` body for serde round-trip; invalid UTF-8 (binary diffs,
        // raw-byte filenames with `core.quotePath` off) is lossy-converted and
        // WARNED so it is never SILENTLY corrupted. R2 should move `UnifiedDiff`
        // to `Vec<u8>` for true byte-fidelity. (P13)
        let stdout = output.stdout;
        if std::str::from_utf8(&stdout).is_err() {
            tracing::warn!(
                "isolation diff contained non-UTF-8 bytes; lossy-converted (R1 inert delta; R2 -> Vec<u8>)"
            );
        }
        Ok(UnifiedDiff::new(
            h.backend(),
            String::from_utf8_lossy(&stdout).into_owned(),
        ))
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

/// Directories never copied into the scratch clone (build output, heavy vendor
/// trees, editor cruft). Keeps the clone proportional to what the child edits
/// and bounds copy cost (P14).
const SKIP_DIR_NAMES: &[&str] = &["target", "node_modules", ".next", ".nuxt", "dist", "build"];

fn copy_tree_without_git(from: &Path, to: &Path) -> Result<(), IsolationError> {
    let from = from.to_path_buf();
    let to = to.to_path_buf();
    copy_dir_recursive(&from, &to, &from)
}

fn copy_dir_recursive(src: &Path, dst: &Path, root: &Path) -> Result<(), IsolationError> {
    std::fs::create_dir_all(dst).map_err(|source| IsolationError::CopyFailed {
        from: src.to_path_buf(),
        to: dst.to_path_buf(),
        source,
    })?;

    for entry in std::fs::read_dir(src).map_err(|source| IsolationError::CopyFailed {
        from: src.to_path_buf(),
        to: dst.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| IsolationError::CopyFailed {
            from: src.to_path_buf(),
            to: dst.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let name = entry.file_name();
        // Exclude `.git` (the clone gets its own fresh repo) and build-output
        // dirs (P14).
        if name == ".git" || path == root.join(".git") {
            continue;
        }
        if path.is_dir() && SKIP_DIR_NAMES.iter().any(|d| *d == name.to_string_lossy()) {
            continue;
        }
        let target = dst.join(&name);
        let file_type = entry
            .file_type()
            .map_err(|source| IsolationError::CopyFailed {
                from: path.clone(),
                to: target.clone(),
                source,
            })?;
        if file_type.is_dir() {
            copy_dir_recursive(&path, &target, root)?;
        } else if file_type.is_file() {
            std::fs::copy(&path, &target).map_err(|source| IsolationError::CopyFailed {
                from: path.clone(),
                to: target.clone(),
                source,
            })?;
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
}
