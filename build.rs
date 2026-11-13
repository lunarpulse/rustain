//! Build metadata for `rustain --version`: `GIT_HASH`, `BUILD_DATE`, `TARGET`.
//!
//! * `GIT_HASH` is `git rev-parse --short HEAD`, or `unknown`.
//! * `BUILD_DATE` is `SOURCE_DATE_EPOCH` when set, else HEAD's committer time, else
//!   `unknown`. It is never the wall clock: two builds of one tree must embed the same
//!   value, or every build would relink `rustain` and every test binary.
//! * The script re-runs only when an input it reads changes. Every git path is
//!   resolved by git (`--git-path`), so linked worktrees (where `.git` is a file) work,
//!   and only existing files are watched: Cargo treats a missing `rerun-if-changed`
//!   path as always stale. Watched: HEAD, the per-worktree HEAD reflog (every commit,
//!   checkout and reset appends to it, even when the branch is packed), the loose file
//!   of the current branch and `packed-refs`. No directory is watched, so a commit in a
//!   sibling worktree does not rebuild this one. Without a `.git` or when any git call
//!   fails, only `build.rs` is watched.
//! * Git is consulted only for a repository rooted at the package, so a source tree
//!   unpacked inside an unrelated repository reports `unknown`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// `9999-12-31T23:59:59Z`: the last instant the fixed 20-byte format can hold.
const MAX_EPOCH: u64 = 253_402_300_799;

fn main() {
    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo sets CARGO_MANIFEST_DIR"),
    );

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    for path in git_watch_paths(&manifest_dir) {
        println!("cargo:rerun-if-changed={}", path.display());
    }

    let hash = git_short_hash(&manifest_dir).unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=GIT_HASH={hash}");

    let epoch = std::env::var_os("SOURCE_DATE_EPOCH").map(|v| v.to_string_lossy().into_owned());
    let date = build_date(epoch.as_deref(), git_commit_time(&manifest_dir))
        .unwrap_or_else(|message| panic!("{message}"));
    println!("cargo:rustc-env=BUILD_DATE={date}");

    // Target triple (set by Cargo during cross-compilation)
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=TARGET={target}");
}

/// Run `git` in `dir`; `Some(output)` if it could be spawned (any exit status).
fn git_output(dir: &Path, args: &[&str]) -> Option<Output> {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()
}

/// Run `git` in `dir`; `Some(trimmed stdout)` only on exit 0 with UTF-8 output.
pub(crate) fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = git_output(dir, args)?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8(out.stdout).ok()?.trim().to_string())
}

fn has_git(dir: &Path) -> bool {
    dir.join(".git").exists()
}

/// `git rev-parse --short HEAD` for a repository rooted at `dir`.
pub(crate) fn git_short_hash(dir: &Path) -> Option<String> {
    if !has_git(dir) {
        return None;
    }
    git(dir, &["rev-parse", "--short", "HEAD"]).filter(|h| !h.is_empty())
}

/// HEAD's committer time (Unix seconds) for a repository rooted at `dir`.
pub(crate) fn git_commit_time(dir: &Path) -> Option<u64> {
    if !has_git(dir) {
        return None;
    }
    git(dir, &["log", "-1", "--format=%ct", "HEAD"])?
        .parse::<u64>()
        .ok()
        .filter(|t| *t <= MAX_EPOCH)
}

/// Existing per-worktree files whose change can alter `GIT_HASH` or the commit time:
/// HEAD, the HEAD reflog (appended by every commit, checkout and reset made here, even
/// when the branch is packed), the loose ref file of the current branch and
/// `packed-refs`. Never a directory (a commit in a sibling worktree must not rebuild
/// this one) and never a missing path. Empty without a package-root `.git` and on any
/// git failure, never a partial set.
pub(crate) fn git_watch_paths(dir: &Path) -> Vec<PathBuf> {
    if !has_git(dir) {
        return Vec::new();
    }
    watch_set(dir).unwrap_or_default()
}

fn watch_set(dir: &Path) -> Option<Vec<PathBuf>> {
    // Relative output (main checkout) is relative to `dir`; absolute output
    // (worktrees) replaces it in `join`.
    let listed = git(
        dir,
        &[
            "rev-parse",
            "--git-path",
            "HEAD",
            "--git-path",
            "logs/HEAD",
            "--git-path",
            "packed-refs",
        ],
    )?;
    let mut paths: Vec<PathBuf> = listed.lines().map(|p| dir.join(p)).collect();
    if paths.len() != 3 {
        return None;
    }
    let symbolic = git_output(dir, &["symbolic-ref", "-q", "HEAD"])?;
    match symbolic.status.code() {
        Some(0) => {
            let branch = String::from_utf8(symbolic.stdout).ok()?;
            let loose = git(dir, &["rev-parse", "--git-path", branch.trim()])?;
            paths.push(dir.join(loose));
        }
        // `-q` exits 1 for a detached HEAD: not a failure.
        Some(1) => {}
        _ => return None,
    }
    paths.retain(|p| p.is_file());
    paths.sort();
    paths.dedup();
    Some(paths)
}

/// `SOURCE_DATE_EPOCH` (digits only, at most `MAX_EPOCH`; empty counts as unset), else
/// the commit time, else `unknown`.
pub(crate) fn build_date(
    source_date_epoch: Option<&str>,
    commit_time: Option<u64>,
) -> Result<String, String> {
    match source_date_epoch {
        Some(v) if !v.is_empty() => {
            let secs = if v.bytes().all(|b| b.is_ascii_digit()) {
                v.parse::<u64>().ok().filter(|s| *s <= MAX_EPOCH)
            } else {
                None
            };
            secs.map(format_utc).ok_or_else(|| {
                format!("SOURCE_DATE_EPOCH must be decimal seconds in 0..={MAX_EPOCH}, got {v:?}")
            })
        }
        _ => Ok(commit_time
            .filter(|t| *t <= MAX_EPOCH)
            .map(format_utc)
            .unwrap_or_else(|| "unknown".to_string())),
    }
}

/// Unix seconds (`0..=MAX_EPOCH`) as `YYYY-MM-DDTHH:MM:SSZ` (always 20 bytes).
pub(crate) fn format_utc(secs: u64) -> String {
    // Manual UTC formatting (avoids a chrono dependency in the build script)
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;
    let (year, month, day) = days_to_date(days);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

/// Convert days since Unix epoch to (year, month, day).
fn days_to_date(days: u64) -> (u64, u64, u64) {
    // Algorithm from Howard Hinnant's chrono-compatible date library
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}
