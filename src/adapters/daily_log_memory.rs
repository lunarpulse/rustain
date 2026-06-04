//! `DailyLogMemory` — the first real `MemoryPort` adapter (Story 11.1).
//!
//! Maintains append-only daily logs at `{workspace}/.rustain/memory/YYYY-MM-DD.md`.
//! Each day's file is a valid, human-readable markdown document. Entries are
//! never modified or deleted (AC2). `recent` / `search` operate over an
//! in-memory snapshot of the current + previous day (AC3, AC6), loaded lazily
//! on first use.
//!
//! Design notes (see story Dev Notes):
//! - **Midnight rollover (AC5) is structural, not timed**: the target filename is
//!   recomputed from `Local::now()` on every `store`, so the first append after
//!   midnight lands in the new day's file automatically. No background task.
//! - **Append is <5ms (NFR55)**: a single idempotent `create_dir_all` + one
//!   append open + one write. The file is never re-read or rewritten on append.
//! - **Lock policy (CLAUDE.md)**: `loaded` is a `tokio::sync::RwLock`; write
//!   guards are scoped tightly and never held across an `.await`.
//! - **Lazy load (Q3)**: the composed adapter is held as `Arc<dyn MemoryPort>`,
//!   so `initialize()` is adapter-specific and cannot be called polymorphically
//!   without widening the trait beyond store/recent/search. We therefore load
//!   current + previous day on first store/recent/search via a `OnceCell`
//!   guard; `initialize()` stays public for explicit/startup calls and tests.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use chrono::{Local, NaiveDate, NaiveTime, TimeZone};
use tokio::io::AsyncWriteExt;
use tokio::sync::{OnceCell, RwLock};

use crate::domain::errors::MemoryError;
use crate::domain::models::MemoryEntry;
use crate::domain::ports::MemoryPort;

/// File-backed daily-log memory adapter.
pub struct DailyLogMemory {
    /// `{workspace}/.rustain/memory` — resolved once at construction (no I/O).
    memory_dir: PathBuf,
    /// In-memory snapshot of current + previous day entries, ascending by
    /// timestamp. Populated by `initialize` / lazy load and appended by `store`.
    loaded: RwLock<Vec<MemoryEntry>>,
    /// Ensures the day-file load runs exactly once (lazy-init guard).
    init: OnceCell<()>,
}

impl DailyLogMemory {
    /// Construct an adapter rooted at `{workspace}/.rustain/memory`.
    /// Does NO I/O — the directory is created on first `store`, and day files
    /// are read on first `store`/`recent`/`search` (or an explicit `initialize`).
    pub fn new(workspace_path: &Path) -> Self {
        Self {
            memory_dir: workspace_path.join(".rustain").join("memory"),
            loaded: RwLock::new(Vec::new()),
            init: OnceCell::new(),
        }
    }

    /// Path of the markdown file for `date`: `{memory_dir}/YYYY-MM-DD.md`.
    fn day_file(&self, date: NaiveDate) -> PathBuf {
        self.memory_dir
            .join(format!("{}.md", date.format("%Y-%m-%d")))
    }

    /// Load the current + previous day's entries into `loaded` (AC3).
    /// Idempotent: runs the load at most once via the `OnceCell` guard.
    /// Missing files are not an error (empty-state init, AC3).
    pub async fn initialize(&self) -> Result<(), MemoryError> {
        self.ensure_loaded().await
    }

    /// Lazily run the day-file load exactly once.
    async fn ensure_loaded(&self) -> Result<(), MemoryError> {
        self.init
            .get_or_try_init(|| async { self.load_days().await })
            .await
            .map(|_| ())
    }

    /// Read current + previous day files and populate `loaded` (ascending).
    async fn load_days(&self) -> Result<(), MemoryError> {
        let today = Local::now().date_naive();
        // `pred_opt` returns None only at the representable-range edge; guard
        // against double-loading the same file if it ever coincides with today.
        let mut dates: Vec<NaiveDate> = Vec::with_capacity(2);
        if let Some(prev) = today.pred_opt() {
            if prev != today {
                dates.push(prev);
            }
        }
        dates.push(today);

        let mut entries: Vec<MemoryEntry> = Vec::new();
        for date in dates {
            let path = self.day_file(date);
            match tokio::fs::read_to_string(&path).await {
                Ok(content) => entries.extend(Self::parse_day_file(&content, date)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(MemoryError::IoError(format!(
                        "failed to read {}: {}",
                        path.display(),
                        e
                    )));
                }
            }
        }
        entries.sort_by_key(|e| e.timestamp);

        // Tight write-guard scope — no `.await` while held (CLAUDE.md lock policy).
        {
            let mut guard = self.loaded.write().await;
            *guard = entries;
        }
        Ok(())
    }

    /// Parse a day file's markdown back into `MemoryEntry`s.
    ///
    /// Format: a single `# YYYY-MM-DD` H1, then per entry an
    /// `## HH:MM:SS — {summary}` H2 followed by an optional context body. The
    /// H1 and any malformed headings are skipped rather than failing the load.
    fn parse_day_file(content: &str, date: NaiveDate) -> Vec<MemoryEntry> {
        let mut entries = Vec::new();
        let mut lines = content.lines().peekable();
        while let Some(line) = lines.next() {
            // Entry headings are H2 (`## `); the H1 date header (`# `) is skipped.
            let Some(rest) = line.strip_prefix("## ") else {
                continue;
            };
            let Some((time_str, summary)) = rest
                .split_once(" — ")
                .or_else(|| rest.split_once(" - "))
                .or_else(|| rest.split_once(" – "))
            else {
                continue; // malformed heading — skip defensively
            };
            let Ok(time) = NaiveTime::parse_from_str(time_str.trim(), "%H:%M:%S") else {
                continue;
            };
            let timestamp = Self::local_at(date, time);
            let summary = summary.trim().to_string();

            // Context = all lines up to the next heading, trailing blanks trimmed.
            let mut ctx_lines: Vec<&str> = Vec::new();
            while let Some(peek) = lines.peek() {
                if peek.starts_with("## ") || peek.starts_with("# ") {
                    break;
                }
                ctx_lines.push(lines.next().unwrap());
            }
            while ctx_lines
                .last()
                .map(|l| l.trim().is_empty())
                .unwrap_or(false)
            {
                ctx_lines.pop();
            }
            let context = if ctx_lines.is_empty() {
                None
            } else {
                Some(ctx_lines.join("\n"))
            };

            entries.push(MemoryEntry {
                timestamp,
                summary,
                context,
            });
        }
        entries
    }

    /// Combine a date + local time into a `DateTime<Local>`, tolerating DST gaps.
    fn local_at(date: NaiveDate, time: NaiveTime) -> chrono::DateTime<Local> {
        let ndt = date.and_time(time);
        Local
            .from_local_datetime(&ndt)
            .single()
            .or_else(|| Local.from_local_datetime(&ndt).earliest())
            .or_else(|| Local.from_local_datetime(&ndt).latest())
            .unwrap_or_else(Local::now)
    }

    /// Render one entry as the markdown block that `store` appends.
    fn render_entry(entry: &MemoryEntry, include_header: bool) -> String {
        let mut buf = String::new();
        if include_header {
            buf.push_str(&format!(
                "# {}\n\n",
                entry.timestamp.date_naive().format("%Y-%m-%d")
            ));
        }
        buf.push_str(&format!(
            "## {} — {}\n",
            entry.timestamp.format("%H:%M:%S"),
            entry.summary
        ));
        if let Some(ctx) = &entry.context {
            if !ctx.is_empty() {
                buf.push_str(ctx);
                if !ctx.ends_with('\n') {
                    buf.push('\n');
                }
            }
        }
        buf.push('\n');
        buf
    }
}

#[async_trait]
impl MemoryPort for DailyLogMemory {
    /// Append `entry` to today's day file (creating the dir + file as needed)
    /// and push it into the loaded snapshot. Append-only; <5ms (NFR55).
    async fn store(&self, entry: MemoryEntry) -> Result<(), MemoryError> {
        self.ensure_loaded().await?;

        if entry.summary.contains('\n') {
            return Err(MemoryError::NotSupported(
                "summary must not contain newlines".into(),
            ));
        }
        if let Some(ref ctx) = entry.context {
            if ctx.lines().any(|l| l.starts_with("## ")) {
                return Err(MemoryError::NotSupported(
                    "context must not contain lines starting with '## '".into(),
                ));
            }
        }

        // Ensure the memory directory exists (AC4). Idempotent + cheap once present.
        tokio::fs::create_dir_all(&self.memory_dir)
            .await
            .map_err(|e| {
                MemoryError::IoError(format!(
                    "failed to create memory dir {}: {}",
                    self.memory_dir.display(),
                    e
                ))
            })?;

        let date = entry.timestamp.date_naive();
        let path = self.day_file(date);

        // Append open — never truncates, never re-reads existing content (AC2).
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(|e| {
                MemoryError::IoError(format!("failed to open {}: {}", path.display(), e))
            })?;

        // Write the H1 header only when the file is new/empty (AC2: appears once).
        // Check via the open handle (not a separate stat) to avoid TOCTOU with
        // concurrent stores on a new/empty file.
        let include_header = file.metadata().await.map_or(true, |m| m.len() == 0);

        let block = Self::render_entry(&entry, include_header);

        file.write_all(block.as_bytes()).await.map_err(|e| {
            MemoryError::IoError(format!("failed to append {}: {}", path.display(), e))
        })?;
        // `tokio::fs::File` buffers writes and the background write is dispatched
        // to the blocking pool — flush so the bytes are durable and visible to
        // the next `metadata()` (header decision) and to readers, before drop.
        file.flush().await.map_err(|e| {
            MemoryError::IoError(format!("failed to flush {}: {}", path.display(), e))
        })?;

        // Mirror the append into the loaded snapshot (tight guard, no `.await`).
        {
            let mut guard = self.loaded.write().await;
            guard.push(entry);
        }
        Ok(())
    }

    /// Return the last `limit` loaded entries, newest-first.
    async fn recent(&self, limit: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
        self.ensure_loaded().await?;
        let guard = self.loaded.read().await;
        Ok(guard.iter().rev().take(limit).cloned().collect())
    }

    /// Case-insensitive substring match over `summary` + `context` of loaded
    /// entries, newest-first, capped at `limit`.
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
        self.ensure_loaded().await?;
        let needle = query.to_lowercase();
        let guard = self.loaded.read().await;
        Ok(guard
            .iter()
            .rev()
            .filter(|e| {
                e.summary.to_lowercase().contains(&needle)
                    || e.context
                        .as_ref()
                        .is_some_and(|c| c.to_lowercase().contains(&needle))
            })
            .take(limit)
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Local};

    fn entry(summary: &str, context: Option<&str>) -> MemoryEntry {
        MemoryEntry {
            timestamp: Local::now(),
            summary: summary.to_string(),
            context: context.map(|s| s.to_string()),
        }
    }

    // 1. store creates the file + dir (AC4).
    #[tokio::test]
    async fn store_creates_file_and_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = DailyLogMemory::new(tmp.path());
        mem.store(entry("first decision", None)).await.unwrap();

        let dir = tmp.path().join(".rustain").join("memory");
        assert!(dir.is_dir(), "memory dir auto-created (AC4)");
        let today = Local::now().date_naive().format("%Y-%m-%d").to_string();
        let file = dir.join(format!("{today}.md"));
        assert!(file.exists(), "today's day file created");
    }

    // 2. Append-only: two stores → both entries, H1 appears once (AC2).
    #[tokio::test]
    async fn append_only_two_entries_single_header() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = DailyLogMemory::new(tmp.path());
        mem.store(entry("alpha", None)).await.unwrap();
        mem.store(entry("beta", Some("some context")))
            .await
            .unwrap();

        let today = Local::now().date_naive().format("%Y-%m-%d").to_string();
        let file = tmp
            .path()
            .join(".rustain")
            .join("memory")
            .join(format!("{today}.md"));
        let content = std::fs::read_to_string(&file).unwrap();
        assert!(content.contains("alpha"), "first entry retained");
        assert!(content.contains("beta"), "second entry appended");
        assert!(content.contains("some context"), "context body written");
        let header = format!("# {today}");
        assert_eq!(
            content.matches(&header).count(),
            1,
            "H1 date header appears exactly once"
        );
    }

    // 3. Valid markdown structure (AC2).
    #[tokio::test]
    async fn valid_markdown_structure() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = DailyLogMemory::new(tmp.path());
        mem.store(entry("structured", None)).await.unwrap();

        let today = Local::now().date_naive().format("%Y-%m-%d").to_string();
        let file = tmp
            .path()
            .join(".rustain")
            .join("memory")
            .join(format!("{today}.md"));
        let content = std::fs::read_to_string(&file).unwrap();
        assert!(
            content.starts_with(&format!("# {today}")),
            "file starts with H1 date header"
        );
        assert!(
            content.contains(" — structured"),
            "entry uses `## HH:MM:SS — summary` heading"
        );
    }

    // 4. initialize loads current + previous day (AC3).
    #[tokio::test]
    async fn initialize_loads_current_and_previous_day() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".rustain").join("memory");
        std::fs::create_dir_all(&dir).unwrap();

        let today = Local::now().date_naive();
        let yesterday = today.pred_opt().unwrap();
        std::fs::write(
            dir.join(format!("{}.md", yesterday.format("%Y-%m-%d"))),
            format!(
                "# {}\n\n## 09:00:00 — yesterday work\n\n",
                yesterday.format("%Y-%m-%d")
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join(format!("{}.md", today.format("%Y-%m-%d"))),
            format!(
                "# {}\n\n## 10:00:00 — today work\n\n",
                today.format("%Y-%m-%d")
            ),
        )
        .unwrap();

        let mem = DailyLogMemory::new(tmp.path());
        mem.initialize().await.unwrap();
        let recent = mem.recent(10).await.unwrap();
        assert_eq!(recent.len(), 2, "both days loaded");
        let summaries: Vec<&str> = recent.iter().map(|e| e.summary.as_str()).collect();
        assert!(summaries.contains(&"yesterday work"));
        assert!(summaries.contains(&"today work"));
    }

    // 5. Empty state: fresh tempdir initializes Ok with no entries (AC3).
    #[tokio::test]
    async fn empty_state_initializes_without_error() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = DailyLogMemory::new(tmp.path());
        mem.initialize().await.unwrap();
        assert!(mem.recent(10).await.unwrap().is_empty());
        assert!(mem.search("anything", 10).await.unwrap().is_empty());
    }

    // 6. recent ordering (newest-first) + limit cap.
    #[tokio::test]
    async fn recent_orders_newest_first_and_caps() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = DailyLogMemory::new(tmp.path());
        // Distinct ascending timestamps so ordering is deterministic.
        let base = Local::now();
        for (i, s) in ["one", "two", "three"].iter().enumerate() {
            mem.store(MemoryEntry {
                timestamp: base + Duration::seconds(i as i64),
                summary: s.to_string(),
                context: None,
            })
            .await
            .unwrap();
        }
        let recent = mem.recent(2).await.unwrap();
        assert_eq!(recent.len(), 2, "limit respected");
        assert_eq!(recent[0].summary, "three", "newest first");
        assert_eq!(recent[1].summary, "two");
    }

    // 7. search: case-insensitive substring over summary + context; cap respected.
    #[tokio::test]
    async fn search_case_insensitive_substring() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = DailyLogMemory::new(tmp.path());
        mem.store(entry("Refactored the Parser", None))
            .await
            .unwrap();
        mem.store(entry("unrelated note", Some("touched the PARSER module")))
            .await
            .unwrap();
        mem.store(entry("nothing here", None)).await.unwrap();

        let hits = mem.search("parser", 10).await.unwrap();
        assert_eq!(hits.len(), 2, "summary + context matches, case-insensitive");
        assert!(
            mem.search("parser", 1).await.unwrap().len() == 1,
            "limit cap"
        );
        assert!(
            mem.search("nonexistent", 10).await.unwrap().is_empty(),
            "non-match excluded"
        );
    }

    // 8. Round-trip: store then parse the file back → equal (modulo sub-second).
    #[tokio::test]
    async fn round_trip_store_then_parse() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = DailyLogMemory::new(tmp.path());
        let original = entry(
            "decided on hexagonal seam",
            Some("ports stay pure\nadapters do I/O"),
        );
        mem.store(original.clone()).await.unwrap();

        let today = Local::now().date_naive();
        let file = tmp
            .path()
            .join(".rustain")
            .join("memory")
            .join(format!("{}.md", today.format("%Y-%m-%d")));
        let content = std::fs::read_to_string(&file).unwrap();
        let parsed = DailyLogMemory::parse_day_file(&content, today);

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].summary, original.summary);
        assert_eq!(parsed[0].context, original.context);
        // Timestamps round-trip at second precision (HH:MM:SS).
        assert_eq!(
            parsed[0].timestamp.format("%H:%M:%S").to_string(),
            original.timestamp.format("%H:%M:%S").to_string()
        );
    }

    // 9. NFR55 — append <5ms (warm dir). #[ignore]: perf is env-sensitive.
    //    Run: cargo test -p rustain --lib daily_log_memory -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "perf (NFR55): run with --ignored"]
    async fn perf_append_under_5ms() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = DailyLogMemory::new(tmp.path());
        // Warm the dir + lazy-load so the timed append is steady-state.
        mem.store(entry("warmup", None)).await.unwrap();

        let start = std::time::Instant::now();
        mem.store(entry("timed append", None)).await.unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(5),
            "append took {elapsed:?}, expected <5ms (NFR55)"
        );
    }

    // 10. NFR55 — 50KB day-file load <100ms. #[ignore]: perf is env-sensitive.
    #[tokio::test]
    #[ignore = "perf (NFR55): run with --ignored"]
    async fn perf_load_50kb_under_100ms() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".rustain").join("memory");
        std::fs::create_dir_all(&dir).unwrap();
        let today = Local::now().date_naive();

        // Generate a ~50KB day file.
        let mut body = format!("# {}\n\n", today.format("%Y-%m-%d"));
        let mut n = 0;
        while body.len() < 50 * 1024 {
            body.push_str(&format!(
                "## 12:00:{:02} — entry {n}\nsome context line\n\n",
                n % 60
            ));
            n += 1;
        }
        std::fs::write(dir.join(format!("{}.md", today.format("%Y-%m-%d"))), body).unwrap();

        let mem = DailyLogMemory::new(tmp.path());
        let start = std::time::Instant::now();
        mem.initialize().await.unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(100),
            "50KB load took {elapsed:?}, expected <100ms (NFR55)"
        );
        assert!(!mem.recent(1).await.unwrap().is_empty());
    }

    // 11. AC5 midnight rollover (STRUCTURAL, not timed): within a single process
    //     moment, two stores whose timestamps straddle local midnight land in
    //     SEPARATE day files — each opened with its own H1 date header, with no
    //     bleed across the boundary. This proves the first append after midnight
    //     rolls into the new day's file automatically (no background task). Fixed
    //     timestamps keep it deterministic (project rule: determinism > realism)
    //     and would catch a regression to a single-file or now()-keyed writer.
    #[tokio::test]
    async fn midnight_rollover_writes_separate_day_files() {
        use chrono::{NaiveDate, NaiveTime};

        let tmp = tempfile::tempdir().unwrap();
        let mem = DailyLogMemory::new(tmp.path());

        // A DST-stable June date and its successor (avoids spring-forward gaps).
        let day1 = NaiveDate::from_ymd_opt(2026, 6, 15).unwrap();
        let day2 = NaiveDate::from_ymd_opt(2026, 6, 16).unwrap();
        // 60s apart, but across the date boundary — reuse the adapter's own
        // local_at so timezone handling matches store() exactly.
        let before_midnight =
            DailyLogMemory::local_at(day1, NaiveTime::from_hms_opt(23, 59, 30).unwrap());
        let after_midnight =
            DailyLogMemory::local_at(day2, NaiveTime::from_hms_opt(0, 0, 30).unwrap());
        assert_eq!(before_midnight.date_naive(), day1, "pre-midnight is day one");
        assert_eq!(after_midnight.date_naive(), day2, "post-midnight is day two");

        mem.store(MemoryEntry {
            timestamp: before_midnight,
            summary: "last note of day one".into(),
            context: None,
        })
        .await
        .unwrap();
        mem.store(MemoryEntry {
            timestamp: after_midnight,
            summary: "first note of day two".into(),
            context: None,
        })
        .await
        .unwrap();

        let dir = tmp.path().join(".rustain").join("memory");
        let f1 = dir.join(format!("{}.md", day1.format("%Y-%m-%d")));
        let f2 = dir.join(format!("{}.md", day2.format("%Y-%m-%d")));
        assert!(f1.exists(), "pre-midnight entry created day-one file {f1:?}");
        assert!(f2.exists(), "post-midnight entry rolled into day-two file {f2:?}");

        let c1 = std::fs::read_to_string(&f1).unwrap();
        let c2 = std::fs::read_to_string(&f2).unwrap();

        // Each entry lands in its OWN day file — no cross-boundary bleed.
        assert!(c1.contains("last note of day one"));
        assert!(
            !c1.contains("first note of day two"),
            "the post-midnight entry must NOT append to the previous day's file"
        );
        assert!(c2.contains("first note of day two"));
        assert!(
            !c2.contains("last note of day one"),
            "the pre-midnight entry must NOT appear in the new day's file"
        );

        // Each new day file opens with its own H1 date header, exactly once.
        assert_eq!(
            c1.matches(&format!("# {}", day1.format("%Y-%m-%d")))
                .count(),
            1,
            "day-one file carries its own H1 date header once"
        );
        assert_eq!(
            c2.matches(&format!("# {}", day2.format("%Y-%m-%d")))
                .count(),
            1,
            "day-two file carries its own H1 date header once"
        );

        // The in-memory snapshot holds both across the boundary, newest-first.
        let recent = mem.recent(10).await.unwrap();
        assert_eq!(recent.len(), 2, "both entries retained across the rollover");
        assert_eq!(recent[0].summary, "first note of day two", "newest first");
    }
}
