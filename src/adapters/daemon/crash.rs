//! Daemon crash persistence (Story 12.1b AC-12-1b-5, Task 4) — the adapter side of
//! the post-mortem record. Owns the file I/O the pure [`DaemonCrashRecord`] domain
//! type cannot: atomic write of the latest-only `daemon-crash.json`, the immutable
//! `crash-<ts>.log` backtrace trail (capped/rotated), and the headless daemon panic
//! hook.
//!
//! Two sinks feed ONE record (party-mode Q2):
//!  - **`daemon-crash.json`** — latest-only machine state `status` reads. Each crash
//!    reads the prior record, folds (`restart_count`++ / bounded ring), and overwrites
//!    atomically (temp+rename, the `pidfile::write_atomic` idiom) so `status` never
//!    sees a torn file.
//!  - **`crash-<ts>.log`** — immutable timestamped backtrace files (the forensic
//!    trail), capped at [`CRASH_LOG_KEEP`] newest so a tight loop can't fill the disk.

#![cfg(unix)]

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

use crate::domain::models::DaemonCrashRecord;
use crate::infrastructure::paths;

/// Keep at most this many `crash-<ts>.log` backtrace files per workspace. Older ones
/// are pruned on each new write so a crash loop cannot fill the disk (Winston's
/// worst-case guard, party-mode Q2).
pub const CRASH_LOG_KEEP: usize = 10;

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Atomically write `body` to `path` via a sibling temp file + rename (the
/// `pidfile::write_atomic` idiom) so a concurrent reader never observes a torn file.
fn write_atomic(path: &Path, body: &str) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    // create_new(true) matches pidfile::write_atomic — fails if a leftover temp
    // exists from a prior interrupted write. Remove the stale temp once and retry.
    let mut f = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::remove_file(&tmp)
                .with_context(|| format!("removing stale {}", tmp.display()))?;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .with_context(|| format!("creating {} (retry)", tmp.display()))?
        }
        Err(e) => return Err(e).with_context(|| format!("creating {}", tmp.display())),
    };
    f.write_all(body.as_bytes())
        .with_context(|| format!("writing {}", tmp.display()))?;
    drop(f);
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Read the current crash record for `workspace`, or `None` when there is none / it
/// is unreadable. Used both by `status` and to fold the prior record on a new crash.
pub fn read_record(workspace: &Path) -> Option<DaemonCrashRecord> {
    let path = paths::daemon_crash_path(workspace).ok()?;
    let body = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&body).ok()
}

/// Persist a fresh crash: read the prior record, fold (`restart_count`++ + bounded
/// ring), and atomically overwrite `daemon-crash.json`. Returns the folded record
/// actually written (so callers can log `restart_count`).
pub fn persist_crash(workspace: &Path, fresh: DaemonCrashRecord) -> Result<DaemonCrashRecord> {
    let prior = read_record(workspace);
    let folded = fresh.fold_from_prior(prior.as_ref());
    let path = paths::daemon_crash_path(workspace)?;
    let body = serde_json::to_string_pretty(&folded).context("serializing daemon crash record")?;
    write_atomic(&path, &body)?;
    Ok(folded)
}

/// Write a timestamped backtrace file and prune the trail to [`CRASH_LOG_KEEP`]
/// newest. Best-effort on the prune (a prune failure must not lose the new record).
pub fn write_backtrace_log(workspace: &Path, ts: u64, body: &str) -> Result<std::path::PathBuf> {
    let path = paths::daemon_crash_log_path(workspace, ts)?;
    // Use create_new so a same-second double-panic doesn't silently overwrite
    // the first backtrace. On collision, append nanoseconds to disambiguate.
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut f) => {
            use std::io::Write;
            f.write_all(body.as_bytes())
                .with_context(|| format!("writing {}", path.display()))?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Same-second collision — disambiguate with nanoseconds.
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            let alt = paths::daemon_crash_log_path(workspace, ts)?;
            let alt = alt.with_file_name(format!("crash-{ts}-{nanos}.log"));
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&alt)
                .with_context(|| format!("writing {}", alt.display()))?;
            use std::io::Write;
            f.write_all(body.as_bytes())
                .with_context(|| format!("writing {}", alt.display()))?;
        }
        Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
    }
    prune_backtrace_logs(workspace, CRASH_LOG_KEEP);
    Ok(path)
}

/// Parse the unix timestamp out of a `crash-<ts>.log` file name, or `None` if it
/// doesn't match the shape.
fn crash_log_ts(name: &str) -> Option<u64> {
    name.strip_prefix("crash-")
        .and_then(|rest| rest.strip_suffix(".log"))
        .and_then(|ts| ts.parse::<u64>().ok())
}

/// Keep only the newest `keep` `crash-*.log` files under `{workspace}/.rustain/`,
/// removing the rest. Ordered by the parsed numeric timestamp (NOT lexicographically
/// — `crash-12` must sort after `crash-4`). Best-effort: errors are ignored.
fn prune_backtrace_logs(workspace: &Path, keep: usize) {
    let dir = workspace.join(".rustain");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut logs: Vec<(u64, std::path::PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let ts = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(crash_log_ts)?;
            Some((ts, path))
        })
        .collect();
    if logs.len() <= keep {
        return;
    }
    logs.sort_by_key(|(ts, _)| *ts);
    let remove_count = logs.len() - keep;
    for (_, old) in logs.into_iter().take(remove_count) {
        let _ = std::fs::remove_file(old);
    }
}

/// Restart-side crash detection (AC-12-1b-4/6). Call when the already-running guard
/// reports `Stale` and a PID file is present: graceful shutdown ALWAYS removes the
/// PID file (`lifecycle.rs`), so a leftover one means the previous instance died
/// without its shutdown path (panic, SIGKILL, OOM, power loss). Reads the prior
/// metadata, records the crash (making the otherwise-silent reclaim observable —
/// `reason: "stale-pidfile"`), and logs the AC-12-1b-6 recovery line.
///
/// **Best-effort:** a recording failure must never block startup. Returns the folded
/// record when one was written (so callers/tests can assert), else `None` (e.g. an
/// unreadable leftover with no recoverable metadata).
pub fn detect_and_record_stale(workspace: &Path, pid_path: &Path) -> Option<DaemonCrashRecord> {
    let prev = match super::pidfile::DaemonPidFile::read(pid_path) {
        Ok(prev) => prev,
        Err(e) => {
            tracing::warn!(
                error = %e, path = %pid_path.display(),
                "reclaiming an unreadable stale daemon PID file (no crash record)"
            );
            return None;
        }
    };
    let now = now_unix();
    let uptime = now.saturating_sub(prev.started_at_unix);
    let fresh = DaemonCrashRecord {
        pid: prev.pid,
        profile: prev.profile.clone(),
        workspace: workspace.to_path_buf(),
        started_at_unix: prev.started_at_unix,
        detected_at_unix: now,
        uptime_secs: uptime,
        reason: "stale-pidfile".into(),
        restart_count: 0,
        last_n_crash_unix: Vec::new(),
    };
    match persist_crash(workspace, fresh) {
        Ok(folded) => {
            let crash_path = paths::daemon_crash_path(workspace)
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            tracing::warn!(
                prev_pid = prev.pid,
                uptime_secs = uptime,
                restart_count = folded.restart_count,
                "recovered from unclean exit (prev PID {}, crashed after {}s); see {}",
                prev.pid,
                uptime,
                crash_path
            );
            Some(folded)
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to persist daemon crash record");
            None
        }
    }
}

/// Context the daemon panic hook needs to write a daemon-attributed crash record.
#[derive(Clone)]
pub struct DaemonPanicContext {
    pub pid: u32,
    pub profile: String,
    pub workspace: std::path::PathBuf,
    pub started_at_unix: u64,
}

/// Record a daemon panic (Story 12.1c P1, extracted from the hook closure so it is
/// directly unit-testable WITHOUT a real panic or touching the process-global panic
/// hook — `reason` should be the `"panic: <message>"` string, `backtrace` the captured
/// trace). Writes the timestamped forensic log + folds the `daemon-crash.json` record.
/// **Best-effort:** every step swallows its own error (a panic hook runs in a
/// panicking process; a write failure must never escalate to a double-panic).
fn record_daemon_panic(ctx: &DaemonPanicContext, reason: &str, backtrace: &str) {
    let ts = now_unix();
    let body = format!(
        "Rustain daemon crash report\n\
         Timestamp: {ts}\n\
         PID: {}\n\
         Profile: {}\n\
         Workspace: {}\n\
         \n{reason}\n\nBacktrace:\n{backtrace}\n",
        ctx.pid,
        ctx.profile,
        ctx.workspace.display(),
    );
    let _ = write_backtrace_log(&ctx.workspace, ts, &body);

    let fresh = DaemonCrashRecord {
        pid: ctx.pid,
        profile: ctx.profile.clone(),
        workspace: ctx.workspace.clone(),
        started_at_unix: ctx.started_at_unix,
        detected_at_unix: ts,
        uptime_secs: ts.saturating_sub(ctx.started_at_unix),
        reason: reason.to_string(),
        restart_count: 0,
        last_n_crash_unix: Vec::new(),
    };
    let _ = persist_crash(&ctx.workspace, fresh);
}

/// Install the **headless** daemon panic hook (AC-12-1b-5). Unlike the global TUI
/// hook (`signals::install_panic_hook`, which restores the terminal and writes a
/// context-free `~/.rustain/crash-<ts>.log`), this hook makes NO terminal assumptions
/// and writes the panic backtrace into the daemon's workspace-scoped forensic trail
/// PLUS a `reason: "panic: <message>"` crash record with full daemon context — then
/// chains to the prior hook so existing behavior is preserved.
///
/// Best-effort by construction: a panic hook runs in a panicking process, so every
/// step swallows its own errors rather than risk a double-panic. The recording body
/// lives in [`record_daemon_panic`] (unit-tested directly).
pub fn install_daemon_panic_hook(ctx: DaemonPanicContext) {
    let prior = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Wrap in catch_unwind so an OOM-induced double-panic doesn't abort
        // before the prior hook runs. If catch_unwind itself fails the process
        // still aborts, but the common allocation-failure path is contained.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let backtrace = std::backtrace::Backtrace::force_capture();
            record_daemon_panic(&ctx, &format!("panic: {info}"), &backtrace.to_string());
        }));

        // Chain to the prior hook (the global TUI hook) so its behavior is preserved.
        prior(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(workspace: &Path, detected: u64, reason: &str) -> DaemonCrashRecord {
        DaemonCrashRecord {
            pid: 99,
            profile: "coding".into(),
            workspace: workspace.to_path_buf(),
            started_at_unix: detected.saturating_sub(5),
            detected_at_unix: detected,
            uptime_secs: 5,
            reason: reason.into(),
            restart_count: 0,
            last_n_crash_unix: Vec::new(),
        }
    }

    #[test]
    fn persist_then_read_roundtrips_and_folds() {
        let ws = tempfile::tempdir().unwrap();
        assert!(read_record(ws.path()).is_none());

        let first = persist_crash(ws.path(), rec(ws.path(), 1_000, "stale-pidfile")).unwrap();
        assert_eq!(first.restart_count, 1);

        let read = read_record(ws.path()).expect("record present after persist");
        assert_eq!(read, first);

        // Second crash folds onto the prior: count→2, ring appends.
        let second = persist_crash(ws.path(), rec(ws.path(), 2_000, "stale-pidfile")).unwrap();
        assert_eq!(second.restart_count, 2);
        assert_eq!(second.last_n_crash_unix, vec![1_000, 2_000]);
        // Latest-only: the file holds exactly the newest folded record.
        assert_eq!(read_record(ws.path()).unwrap(), second);
    }

    #[test]
    fn backtrace_logs_are_capped_to_keep_newest() {
        let ws = tempfile::tempdir().unwrap();
        // Write CRASH_LOG_KEEP + 3 logs at increasing timestamps.
        for ts in 1..=(CRASH_LOG_KEEP as u64 + 3) {
            write_backtrace_log(ws.path(), ts, &format!("crash {ts}")).unwrap();
        }
        let dir = ws.path().join(".rustain");
        let mut tss: Vec<u64> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter_map(|n| crash_log_ts(&n))
            .collect();
        tss.sort_unstable();
        assert_eq!(tss.len(), CRASH_LOG_KEEP, "trail must be capped");
        // Oldest kept is ts=4 (1..=3 pruned); newest is CRASH_LOG_KEEP+3.
        assert_eq!(*tss.first().unwrap(), 4);
        assert_eq!(*tss.last().unwrap(), CRASH_LOG_KEEP as u64 + 3);
    }

    // ── Panic-record path (Story 12.1c P2) ───────────────────────────────────────
    // The panic hook gained logic in review (catch_unwind + collision fallback) but
    // had no L1 test, violating the story's "every L3 token has an L1 assertion".
    // Test the extracted body directly — no real panic, no global-hook mutation.

    fn panic_ctx(workspace: &Path) -> DaemonPanicContext {
        DaemonPanicContext {
            pid: 7,
            profile: "coding".into(),
            workspace: workspace.to_path_buf(),
            started_at_unix: 1_000,
        }
    }

    #[test]
    fn record_daemon_panic_writes_reason_and_backtrace_log() {
        let ws = tempfile::tempdir().unwrap();
        record_daemon_panic(
            &panic_ctx(ws.path()),
            "panic: boom at x.rs:1",
            "BT-LINE-A\nBT-LINE-B",
        );

        // (1) daemon-crash.json carries the panic reason + folded count.
        let rec = read_record(ws.path()).expect("crash record written");
        assert!(rec.reason.starts_with("panic:"), "reason: {}", rec.reason);
        assert_eq!(rec.restart_count, 1);
        assert_eq!(rec.pid, 7);

        // (2) a crash-<ts>.log forensic file with the backtrace exists.
        let dir = ws.path().join(".rustain");
        let logs: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with("crash-") && n.ends_with(".log"))
            })
            .collect();
        assert_eq!(logs.len(), 1, "exactly one backtrace log");
        let body = std::fs::read_to_string(logs[0].path()).unwrap();
        assert!(body.contains("BT-LINE-A") && body.contains("Profile: coding"));
    }

    #[test]
    fn record_daemon_panic_is_best_effort_on_bad_workspace() {
        // A non-writable/nonexistent workspace must NOT panic (the hook runs in an
        // already-panicking process — a double-panic would abort before chaining).
        let bogus = Path::new("/proc/nonexistent-rustain-12-1c/deeper");
        record_daemon_panic(&panic_ctx(bogus), "panic: x", "bt");
        // No assertion beyond "did not panic" — reaching here is the pass.
    }

    #[test]
    fn two_panics_increment_restart_count() {
        let ws = tempfile::tempdir().unwrap();
        record_daemon_panic(&panic_ctx(ws.path()), "panic: first", "bt1");
        record_daemon_panic(&panic_ctx(ws.path()), "panic: second", "bt2");
        let rec = read_record(ws.path()).unwrap();
        assert_eq!(rec.restart_count, 2, "second panic folds onto the first");
        assert!(rec.reason.contains("second"));
    }
}
