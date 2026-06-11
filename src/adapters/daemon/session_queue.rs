//! Durable session-boundary intent queue (Story 12.1c AC2 + AC3) — the adapter
//! side of the two records the headless daemon QUEUES at a `SessionBoundary` for a
//! later TUI attach (Story 12.2) to consume.
//!
//! Owns the file I/O the pure domain types ([`ConsolidationDueMarker`],
//! [`MemoryMdPurgeNotice`]) cannot: latest-only atomic write (temp→rename, the
//! `crash::write_atomic` idiom) under `{workspace}/.rustain/`. Daily logs are NEVER
//! touched by this path (Murat's append-only invariant).

#![cfg(unix)]

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::domain::models::{
    ConsolidationDueMarker, MemoryMdPurgeNotice, PURGE_NOTICE_PREVIEW_CAP,
};
use crate::infrastructure::paths;

/// Atomically write `body` to `path` via a sibling temp file + rename so a
/// concurrent reader never observes a torn file (same idiom as `crash::write_atomic`
/// and `pidfile::write_atomic`).
fn write_atomic(path: &Path, body: &str) -> Result<()> {
    // Unique temp name: pid + monotonic counter to avoid concurrent collisions
    // and symlink-attack predictability (same idiom as adapter::write_atomic).
    let tmp = {
        let mut s = path.to_path_buf().into_os_string();
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        s.push(format!(".tmp.{pid}.{n:016x}"));
        PathBuf::from(s)
    };
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    f.write_all(body.as_bytes())
        .with_context(|| format!("writing {}", tmp.display()))?;
    f.sync_all()
        .with_context(|| format!("syncing {}", tmp.display()))?;
    drop(f);
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

// ── AC2: consolidation-due marker (latest-only) ──────────────────────────────

/// Queue (overwrite) the latest "consolidation-due" marker for `workspace`. Latest-
/// only: one pending marker, so repeated boundaries don't grow the file. The daemon
/// has no engine to GENERATE a suggestion — this only records the trigger + the
/// daily-log slice 12.2 should consolidate. NEVER deletes daily logs.
pub fn enqueue_consolidation_due(workspace: &Path, marker: &ConsolidationDueMarker) -> Result<()> {
    let path = paths::daemon_consolidation_queue_path(workspace)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let body =
        serde_json::to_string_pretty(marker).context("serializing consolidation-due marker")?;
    write_atomic(&path, &body)
}

/// Read the pending consolidation-due marker, or `None` when there is none / it is
/// unreadable. The 12.2 attach consumer reads this.
pub fn read_consolidation_due(workspace: &Path) -> Option<ConsolidationDueMarker> {
    let path = paths::daemon_consolidation_queue_path(workspace).ok()?;
    match std::fs::read_to_string(&path) {
        Ok(body) => match serde_json::from_str(&body) {
            Ok(marker) => Some(marker),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "corrupt consolidation-due queue file");
                None
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "cannot read consolidation-due queue file");
            None
        }
    }
}

/// Remove the pending consolidation-due marker after it has been consumed by
/// the daemon's consolidation card flow (Story 12.2d AC6). A missing file is
/// already consumed and is therefore success; any other I/O error is returned.
/// Modeled on [`clear_purge_notice`].
pub fn clear_consolidation_due(workspace: &Path) -> Result<()> {
    let path = paths::daemon_consolidation_queue_path(workspace)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// Clear the consolidation-due marker ONLY when the on-disk marker's
/// `queued_at_unix` equals `expected_queued_at` (Story 12.2d code-review P1).
/// A resolve carries the identity of the marker the card was generated from; if a
/// NEWER boundary marker was written between card-shown and resolve, the unconditional
/// [`clear_consolidation_due`] would silently delete that newer marker (data loss).
/// Returns `Ok(true)` if a matching marker was cleared, `Ok(false)` if the marker was
/// absent or superseded (left intact for its own resolve).
pub fn clear_consolidation_due_if(workspace: &Path, expected_queued_at: u64) -> Result<bool> {
    match read_consolidation_due(workspace) {
        Some(m) if m.queued_at_unix == expected_queued_at => {
            clear_consolidation_due(workspace)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

// ── AC3: MEMORY.md purge audit notice (latest-only) ──────────────────────────

/// Queue (overwrite) the latest MEMORY.md purge audit notice for `workspace` — the
/// "never silent" record for a LIVE file-edit purge (the purge already happened; this
/// is visibility, NOT a gate). `summaries` is bounded to [`PURGE_NOTICE_PREVIEW_CAP`].
pub fn enqueue_purge_notice(
    workspace: &Path,
    purged_count: usize,
    summaries: Vec<String>,
    queued_at_unix: u64,
) -> Result<()> {
    let recent_summaries: Vec<String> = summaries
        .into_iter()
        .take(PURGE_NOTICE_PREVIEW_CAP)
        .collect();
    let notice = MemoryMdPurgeNotice {
        purged_count,
        queued_at_unix,
        recent_summaries,
    };
    let path = paths::daemon_purge_notice_path(workspace)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(&notice).context("serializing purge notice")?;
    write_atomic(&path, &body)
}

/// Remove the pending MEMORY.md purge notice after it has been successfully
/// enqueued to a writer-attach connection (Story 12.2c AC7). A missing file is
/// already drained and is therefore success; any other I/O error is returned so
/// the daemon can leave an audit trail instead of silently breaking the once-only
/// delivery contract.
pub fn clear_purge_notice(workspace: &Path) -> Result<()> {
    let path = paths::daemon_purge_notice_path(workspace)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// Read the pending MEMORY.md purge notice, or `None` when there is none / unreadable.
pub fn read_purge_notice(workspace: &Path) -> Option<MemoryMdPurgeNotice> {
    let path = paths::daemon_purge_notice_path(workspace).ok()?;
    match std::fs::read_to_string(&path) {
        Ok(body) => match serde_json::from_str(&body) {
            Ok(notice) => Some(notice),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "corrupt purge notice file");
                None
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "cannot read purge notice file");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consolidation_marker_round_trips_latest_only() {
        let ws = tempfile::tempdir().unwrap();
        let m1 = ConsolidationDueMarker {
            boundary: "daily_reset".into(),
            queued_at_unix: 1_000,
            daily_log_ref: "2026-06-07".into(),
        };
        enqueue_consolidation_due(ws.path(), &m1).unwrap();
        assert_eq!(read_consolidation_due(ws.path()).unwrap(), m1);

        // Latest-only: a second boundary overwrites (no growth, one pending marker).
        let m2 = ConsolidationDueMarker {
            boundary: "shutdown".into(),
            queued_at_unix: 2_000,
            daily_log_ref: "2026-06-07".into(),
        };
        enqueue_consolidation_due(ws.path(), &m2).unwrap();
        assert_eq!(read_consolidation_due(ws.path()).unwrap(), m2);
    }

    #[test]
    fn purge_notice_round_trips_and_caps_preview() {
        let ws = tempfile::tempdir().unwrap();
        let many: Vec<String> = (0..25).map(|i| format!("fact {i}")).collect();
        enqueue_purge_notice(ws.path(), 25, many, 5_000).unwrap();
        let got = read_purge_notice(ws.path()).unwrap();
        assert_eq!(got.purged_count, 25);
        assert_eq!(got.queued_at_unix, 5_000);
        assert_eq!(
            got.recent_summaries.len(),
            PURGE_NOTICE_PREVIEW_CAP,
            "preview is bounded"
        );
        assert_eq!(
            got.message(),
            "25 facts removed from MEMORY.md — purged from search index"
        );
    }

    #[test]
    fn absent_queue_reads_none() {
        let ws = tempfile::tempdir().unwrap();
        assert!(read_consolidation_due(ws.path()).is_none());
        assert!(read_purge_notice(ws.path()).is_none());
    }
}
