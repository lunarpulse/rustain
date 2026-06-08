//! `daemon status` snapshot + RSS measurement (Story 12.1a Task 8, AC-12-1a-2/4).
//!
//! "Active conversations: 0" is the honest 12.1a state — there is no message
//! source until Story 12.2 (attach) / 12.3 (telegram) / 12.4 (cron). `status`
//! reports it truthfully rather than faking activity.

use super::pidfile::DaemonPidFile;
use crate::domain::models::{AppConfig, ChannelKind, DaemonCrashRecord};

/// A structured daemon status snapshot. Rendered either as a human block or as
/// JSON (`--json`, mirrors `profile list --json` for scriptability).
#[derive(Debug, serde::Serialize)]
pub struct StatusSnapshot {
    pub running: bool,
    pub pid: u32,
    pub uptime_secs: u64,
    pub profile: String,
    /// Connected channels. Empty in 12.1a (only `terminal` → `NoOpChannel`).
    pub channels: Vec<String>,
    /// Always 0 in 12.1a (no message runtime — see module docs).
    pub active_conversations: u64,
    /// Resident set size in KiB, or `None` where the platform can't report it.
    pub rss_kb: Option<u64>,
    /// Last-activity Unix seconds. In 12.1a (no messages) this equals start time.
    pub last_activity_unix: u64,
    pub socket_path: String,
    pub workspace: String,
    /// Last recorded unclean exit for this workspace's daemon (Story 12.1b
    /// AC-12-1b-6), or `None`/`null` when there is no crash record. Read from
    /// `paths::daemon_crash_path`.
    pub last_crash: Option<DaemonCrashRecord>,
}

impl StatusSnapshot {
    /// Gather a snapshot for a confirmed-running daemon.
    pub fn gather(pf: &DaemonPidFile, _config: &AppConfig) -> Self {
        let now = now_unix();
        let uptime_secs = now.saturating_sub(pf.started_at_unix);
        StatusSnapshot {
            running: true,
            pid: pf.pid,
            uptime_secs,
            profile: pf.profile.clone(),
            // Story 12.2b — the daemon now serves the interactive terminal/attach
            // channel (Telegram/cron join in 12.3/12.4). Conversation count is read
            // honestly from the session store (the per-process conversation persists
            // after each turn). `status` runs out-of-process, so this reflects the
            // on-disk session count rather than a live socket query.
            channels: vec![ChannelKind::Terminal.as_prefix().to_string()],
            active_conversations: count_persisted_conversations(&pf.workspace),
            rss_kb: read_rss_kb(pf.pid),
            last_activity_unix: pf.started_at_unix,
            socket_path: pf.socket_path.display().to_string(),
            workspace: pf.workspace.display().to_string(),
            last_crash: super::crash::read_record(&pf.workspace),
        }
    }

    /// Render as a human-readable block (AC-12-1a-2 required fields).
    pub fn to_human(&self) -> String {
        let rss = match self.rss_kb {
            Some(kb) => format!("{:.1} MB", kb as f64 / 1024.0),
            None => "n/a".to_string(),
        };
        let channels = if self.channels.is_empty() {
            "none".to_string()
        } else {
            self.channels.join(", ")
        };
        let last_crash = match &self.last_crash {
            Some(c) => format!(
                "{} (prev PID {}, after {}, restart #{})",
                c.reason,
                c.pid,
                fmt_uptime(c.uptime_secs),
                c.restart_count
            ),
            None => "none".to_string(),
        };
        format!(
            "Daemon running\n\
             PID:                  {}\n\
             Uptime:               {}\n\
             Active profile:       {}\n\
             Connected channels:   {}\n\
             Active conversations: {}\n\
             Resident memory:      {}\n\
             Last activity:        {} ago\n\
             Last crash:           {}\n\
             Socket:               {}",
            self.pid,
            fmt_uptime(self.uptime_secs),
            self.profile,
            channels,
            self.active_conversations,
            rss,
            // 12.1a: no message source, so last activity == start → "uptime" ago.
            fmt_uptime(self.uptime_secs),
            last_crash,
            self.socket_path,
        )
    }

    /// Render as pretty JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }
}

fn fmt_uptime(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}h {m}m {s}s")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Story 12.2b AC4 — count persisted conversations in the workspace session store
/// (each conversation is a subdirectory under `sessions_dir`). Best-effort: a
/// missing/unreadable dir reads as `0` (honest for a never-used workspace).
fn count_persisted_conversations(workspace: &std::path::Path) -> u64 {
    let dir = crate::infrastructure::paths::sessions_dir(workspace);
    std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .count() as u64
        })
        .unwrap_or(0)
}

/// Read resident set size in KiB for `pid` (AC-12-1a-4 / NFR46). Linux reads
/// `/proc/<pid>/status` `VmRSS`; other platforms return `None` (status shows
/// "n/a", and the NFR46 measurement gate self-skips — see the integration test).
#[cfg(target_os = "linux")]
pub fn read_rss_kb(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            // e.g. "VmRSS:    12345 kB"
            return rest.split_whitespace().next()?.parse().ok();
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
pub fn read_rss_kb(_pid: u32) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_uptime_buckets() {
        assert_eq!(fmt_uptime(5), "5s");
        assert_eq!(fmt_uptime(65), "1m 5s");
        assert_eq!(fmt_uptime(3661), "1h 1m 1s");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn read_rss_for_self_is_some_and_nonzero() {
        let rss = read_rss_kb(std::process::id());
        assert!(
            rss.is_some(),
            "VmRSS for the test process should be readable"
        );
        assert!(rss.unwrap() > 0);
    }

    #[test]
    fn snapshot_json_includes_required_fields() {
        // Non-existent workspace → no crash record → last_crash null (AC-12-1b-6).
        let pf = DaemonPidFile {
            pid: 7,
            socket_path: "/tmp/x.sock".into(),
            workspace: "/nonexistent-ws-12-1b".into(),
            started_at_unix: now_unix().saturating_sub(10),
            profile: "coding".into(),
            nonce: String::new(),
            boot_id: None,
        };
        let snap = StatusSnapshot::gather(&pf, &AppConfig::default());
        let json = snap.to_json();
        for field in [
            "\"running\"",
            "\"pid\"",
            "\"uptime_secs\"",
            "\"profile\"",
            "\"active_conversations\"",
            "\"last_crash\"",
        ] {
            assert!(json.contains(field), "json missing {field}: {json}");
        }
        assert_eq!(snap.active_conversations, 0);
        assert!(snap.uptime_secs >= 10);
        assert!(snap.last_crash.is_none());
        assert!(json.contains("\"last_crash\": null"));
        assert!(snap.to_human().contains("Last crash:           none"));
    }
}
