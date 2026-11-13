//! infra-1: the build script's helpers, mounted from `build.rs` and run against real
//! git in temp directories. Freshness itself (a second `cargo build` compiles nothing)
//! is proved through real `cargo build`s, not here: see the story's second-build check.

use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

#[allow(dead_code)]
#[path = "../build.rs"]
mod build_script;

use build_script::{build_date, format_utc, git_commit_time, git_short_hash, git_watch_paths};

/// Hermetic git: no user/system config, fixed identity, no signing.
fn git_at(dir: &Path, committer_date: Option<&str>, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args);
    if let Some(date) = committer_date {
        cmd.env("GIT_COMMITTER_DATE", date)
            .env("GIT_AUTHOR_DATE", date);
    }
    let out = cmd.output().expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf-8")
        .trim()
        .to_string()
}

fn git(dir: &Path, args: &[&str]) -> String {
    git_at(dir, None, args)
}

fn commit(dir: &Path, committer_date: &str) {
    git_at(
        dir,
        Some(committer_date),
        &["commit", "-q", "--allow-empty", "-m", "c"],
    );
}

/// A repository on `main` with one commit dated 1700000000.
fn repo() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).expect("mkdir repo");
    git(&repo, &["init", "-q", "-b", "main"]);
    commit(&repo, "@1700000000 +0000");
    (tmp, repo)
}

/// `repo` plus a linked worktree `wt` on branch `story/x`.
fn repo_with_worktree() -> (TempDir, PathBuf, PathBuf) {
    let (tmp, repo) = repo();
    let wt = tmp.path().join("wt");
    git(&repo, &["worktree", "add", "-q", "-b", "story/x", "../wt"]);
    assert!(
        wt.join(".git").is_file(),
        "a linked worktree's .git is a file"
    );
    (tmp, repo, wt)
}

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|e| panic!("canonicalize {p:?}: {e}"))
}

/// Watch set, canonicalized; asserts every path is absolute and an existing regular
/// file (AC2: no directory, no missing path).
fn watch(dir: &Path) -> Vec<PathBuf> {
    git_watch_paths(dir)
        .iter()
        .map(|p| {
            assert!(p.is_absolute(), "watch path must be absolute: {p:?}");
            assert!(p.is_file(), "watch path must be an existing file: {p:?}");
            canon(p)
        })
        .collect()
}

/// The watch set of `dir` equals `wants` (paths relative to `repo`); `packed-refs` is
/// tolerated when absent from `wants` because git may or may not have created it.
fn assert_watch(dir: &Path, repo: &Path, wants: &[&str]) {
    let mut want: Vec<PathBuf> = wants.iter().map(|w| canon(&repo.join(w))).collect();
    want.sort();
    let mut got: Vec<PathBuf> = watch(dir)
        .into_iter()
        .filter(|p| want.contains(p) || !p.ends_with("packed-refs"))
        .collect();
    got.sort();
    assert_eq!(got, want, "watch set of {dir:?}");
}

// ── B1–B3: date logic ────────────────────────────────────────────────────────

#[test]
fn b1_build_date_precedence() {
    assert_eq!(
        build_date(Some("1700000000"), Some(0)).unwrap(),
        "2023-11-14T22:13:20Z"
    );
    assert_eq!(build_date(None, Some(0)).unwrap(), "1970-01-01T00:00:00Z");
    assert_eq!(
        build_date(Some(""), Some(0)).unwrap(),
        "1970-01-01T00:00:00Z"
    );
    assert_eq!(build_date(None, None).unwrap(), "unknown");
    assert_eq!(build_date(Some(""), None).unwrap(), "unknown");
}

#[test]
fn b2_source_date_epoch_is_validated() {
    for bad in [
        "abc",
        "-1",
        "+5",
        "1.5",
        " 5",
        "253402300800",
        "99999999999999999999999",
    ] {
        let err = build_date(Some(bad), Some(0)).expect_err(bad);
        assert!(err.contains("SOURCE_DATE_EPOCH"), "{bad:?}: {err}");
    }
    assert_eq!(
        build_date(Some("253402300799"), None).unwrap(),
        "9999-12-31T23:59:59Z"
    );
}

#[test]
fn b3_format_utc_calendar_vectors() {
    for (secs, want) in [
        (0, "1970-01-01T00:00:00Z"),
        (951_782_400, "2000-02-29T00:00:00Z"),
        (1_709_251_199, "2024-02-29T23:59:59Z"),
        (4_102_444_800, "2100-01-01T00:00:00Z"),
        (4_107_542_400, "2100-03-01T00:00:00Z"),
        (253_402_300_799, "9999-12-31T23:59:59Z"),
    ] {
        let got = format_utc(secs);
        assert_eq!(got, want, "{secs}");
        assert_eq!(got.len(), 20);
    }
}

// ── B4–B8, B10: watch sets against real repositories ─────────────────────────

#[test]
fn b4_plain_repo_watches_head_reflog_and_branch_ref() {
    let (_tmp, repo) = repo();
    assert_watch(
        &repo,
        &repo,
        &[".git/HEAD", ".git/logs/HEAD", ".git/refs/heads/main"],
    );
}

#[test]
fn b5_linked_worktree_watches_its_own_head_reflog_and_branch_ref() {
    let (tmp, repo, wt) = repo_with_worktree();
    // a sibling worktree with a commit of its own must not appear in this set
    let sibling = tmp.path().join("sibling");
    git(
        &repo,
        &["worktree", "add", "-q", "-b", "story/y", "../sibling"],
    );
    commit(&sibling, "@1700000100 +0000");
    assert_watch(
        &wt,
        &repo,
        &[
            ".git/worktrees/wt/HEAD",
            ".git/worktrees/wt/logs/HEAD",
            ".git/refs/heads/story/x",
        ],
    );
    // control: the sibling's own set names its own branch
    assert_watch(
        &sibling,
        &repo,
        &[
            ".git/worktrees/sibling/HEAD",
            ".git/worktrees/sibling/logs/HEAD",
            ".git/refs/heads/story/y",
        ],
    );
}

#[test]
fn b6_packed_branch_watches_packed_refs_and_reflog_but_no_directory() {
    let (_tmp, repo, wt) = repo_with_worktree();
    git(&wt, &["pack-refs", "--all"]);
    assert!(
        !repo.join(".git/refs/heads/story/x").exists(),
        "pack-refs removed the loose ref"
    );
    assert_watch(
        &wt,
        &repo,
        &[
            ".git/worktrees/wt/HEAD",
            ".git/worktrees/wt/logs/HEAD",
            ".git/packed-refs",
        ],
    );
}

#[test]
fn b7_detached_head_watches_no_branch_ref() {
    let (_tmp, repo, wt) = repo_with_worktree();
    git(&wt, &["checkout", "-q", "--detach"]);
    assert_watch(
        &wt,
        &repo,
        &[".git/worktrees/wt/HEAD", ".git/worktrees/wt/logs/HEAD"],
    );
}

#[test]
fn b8_directory_inside_an_unrelated_repo_is_not_a_repository() {
    let (_tmp, repo) = repo();
    let probe = repo.join("vendor/probe");
    std::fs::create_dir_all(&probe).expect("mkdir probe");
    assert!(git_watch_paths(&probe).is_empty());
    assert_eq!(git_short_hash(&probe), None);
    assert_eq!(git_commit_time(&probe), None);
    // control: the enclosing repository itself reports a hash
    assert!(git_short_hash(&repo).is_some());
}

#[test]
fn b10_git_failure_yields_an_empty_watch_set_and_no_values() {
    let tmp = TempDir::new().expect("tempdir");
    let missing = tmp.path().join("missing");
    std::fs::write(
        tmp.path().join(".git"),
        format!("gitdir: {}\n", missing.display()),
    )
    .expect("write .git file");
    assert!(git_watch_paths(tmp.path()).is_empty());
    assert_eq!(git_short_hash(tmp.path()), None);
    assert_eq!(git_commit_time(tmp.path()), None);
}

#[test]
fn unborn_head_has_no_hash_or_time_but_still_watches_head() {
    let tmp = TempDir::new().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main"]);
    assert_eq!(git_short_hash(tmp.path()), None);
    assert_eq!(git_commit_time(tmp.path()), None);
    assert_watch(tmp.path(), tmp.path(), &[".git/HEAD"]);
}

// ── B9: values ───────────────────────────────────────────────────────────────

#[test]
fn b9_hash_and_commit_time_match_git_and_are_independent() {
    let (_tmp, repo) = repo();
    assert_eq!(
        git_short_hash(&repo).as_deref(),
        Some(git(&repo, &["rev-parse", "--short", "HEAD"]).as_str())
    );
    assert_eq!(git_commit_time(&repo), Some(1_700_000_000));
    assert_eq!(
        git(&repo, &["log", "-1", "--format=%ct"])
            .parse::<u64>()
            .ok(),
        git_commit_time(&repo)
    );
}
