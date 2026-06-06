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
use tokio::sync::{Mutex, OnceCell, RwLock};

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
    /// Single-writer serialization for the append critical section (Story 12.0
    /// C1 / AC1). THE single hardened write sink (AC9): every `store()` append
    /// — and the `prepare_detach()` drain that funnels through it — acquires this
    /// guard across the whole open→header-decision→`write_all`→`flush` window, so
    /// two concurrent appends to the SAME day-file cannot both observe `len()==0`
    /// (double H1 header) nor interleave their multi-line blocks. `tokio::sync`
    /// per CLAUDE.md Async Lock Policy → the `MAX_KNOWN_STD_SYNC_LOCKS=4` ratchet
    /// is untouched. Contention is low (NFR55 `<5ms`); a profile swap that wants
    /// to detach this adapter drains by acquiring the same guard.
    write_lock: Mutex<()>,
    /// Test-only deterministic suspension seam (Story 12.0 AC10). Compiled out of
    /// release builds — zero production behaviour change.
    #[cfg(test)]
    seam: WriteSeam,
}

/// Test-only suspension seam pinning the C1 interleave window between the
/// header-decision and the `write_all` (Story 12.0 AC10). A test arms it, then
/// the FIRST writer to reach the seam disarms it, signals `reached`, and parks
/// on `proceed` — letting the test deterministically interleave a second writer
/// (under the unfixed code) without a `sleep`. Under the `write_lock` fix the
/// parked writer holds the lock, so a second writer simply blocks on the lock
/// and the seam is moot. `notify_one` permits are stored when no waiter is yet
/// registered, so arrival/release ordering is forgiving.
#[cfg(test)]
#[derive(Default)]
struct WriteSeam {
    armed: std::sync::atomic::AtomicBool,
    reached: tokio::sync::Notify,
    proceed: tokio::sync::Notify,
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
            write_lock: Mutex::new(()),
            #[cfg(test)]
            seam: WriteSeam::default(),
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

        // ── Single-writer critical section (Story 12.0 C1 / AC1, AC9) ──
        // Serialise the entire open→header-decision→write→flush window so two
        // concurrent `store()` calls to the same day-file cannot both decide
        // `include_header == true` (double H1) nor interleave their blocks. This
        // is the ONE hardened write sink the profile-swap drain (`prepare_detach`,
        // AC9) also funnels through — do NOT add a second per-call-site guard.
        // The guard is released on block exit, before the `loaded` snapshot push.
        {
            let _write_guard = self.write_lock.lock().await;

            // Append open — never truncates, never re-reads existing content (AC2).
            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .await
                .map_err(|e| {
                    MemoryError::IoError(format!("failed to open {}: {}", path.display(), e))
                })?;

            // Write the H1 header only when the file is new/empty (AC2: appears
            // once). Under the `write_lock` no other writer can be between this
            // decision and the `write_all`, so two new-file stores can no longer
            // both render a header.
            let include_header = file.metadata().await.map_or(true, |m| m.len() == 0);

            // Test-only deterministic suspension seam (AC10): pin the interleave
            // window between the header-decision and the `write_all`. The first
            // writer to arrive parks here; under the fix it does so while holding
            // `write_lock`, so a second writer blocks on the lock (seam moot).
            #[cfg(test)]
            if self
                .seam
                .armed
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                self.seam.reached.notify_one();
                self.seam.proceed.notified().await;
            }

            let block = Self::render_entry(&entry, include_header);

            file.write_all(block.as_bytes()).await.map_err(|e| {
                MemoryError::IoError(format!("failed to append {}: {}", path.display(), e))
            })?;
            // `tokio::fs::File` buffers writes and the background write is
            // dispatched to the blocking pool — flush so the bytes are durable and
            // visible to the next `metadata()` (header decision) and to readers,
            // before drop.
            file.flush().await.map_err(|e| {
                MemoryError::IoError(format!("failed to flush {}: {}", path.display(), e))
            })?;
        }

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

    /// Drain in-flight appends before a profile swap detaches this adapter
    /// (Story 12.0 C2 / AC2, AC9). Acquiring (and immediately dropping) the
    /// single-writer `write_lock` cannot return until any `store()` currently
    /// inside its critical section has flushed and released — so the swap only
    /// proceeds once this adapter is quiescent and the in-flight write is durable
    /// on disk. The newly-composed adapter (sharing the same backing day-files)
    /// then lazily loads disk AFTER the drain, so the straddling write is never
    /// lost. Reuses the existing `tokio::sync` lock → no new shared state, ratchet
    /// neutral.
    async fn prepare_detach(
        &self,
    ) -> Result<crate::domain::models::TransitionState, crate::domain::errors::TransitionError>
    {
        let _drained = self.write_lock.lock().await;
        Ok(crate::domain::models::TransitionState::empty("memory"))
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

    // ── Story 12.0 C1 — concurrent daily-log appends (AC1, AC10) ──

    // C1 (AC1) — N=32 concurrent `store()` calls to the SAME day-file, released
    // simultaneously via a `Barrier`, MUST yield exactly N well-formed entries
    // and exactly ONE H1 header. This is the fix-agnostic invariant oracle: it
    // asserts the property, not the mechanism. RED on the unfixed code (no
    // `write_lock`): many of the 32 racers observe `len()==0` and each renders
    // its own H1, so the file ends up with multiple `# <date>` headers. GREEN
    // under the single-writer fix (writes serialise → header decided once).
    // Deterministic-enough by construction: with 32 truly-concurrent racers a
    // clean single-header file is not a 1-in-1000 fluke — the unfixed double
    // header is overwhelming. (The seam test below pins the 2-writer window
    // exactly.) No `sleep`.
    #[tokio::test]
    async fn concurrent_appends_single_header_n32() {
        use std::sync::Arc;
        use tokio::sync::Barrier;

        const N: usize = 32;
        let tmp = tempfile::tempdir().unwrap();
        let mem = Arc::new(DailyLogMemory::new(tmp.path()));

        // A DST-stable fixed day so every write targets ONE day-file and the test
        // can never straddle a real midnight boundary mid-run.
        let day = NaiveDate::from_ymd_opt(2026, 6, 15).unwrap();
        let barrier = Arc::new(Barrier::new(N));

        let mut handles = Vec::with_capacity(N);
        for i in 0..N {
            let mem = Arc::clone(&mem);
            let barrier = Arc::clone(&barrier);
            handles.push(tokio::spawn(async move {
                // Distinct second-precision timestamps → distinct rendered entries.
                let ts = DailyLogMemory::local_at(
                    day,
                    NaiveTime::from_hms_opt(12, 0, (i % 60) as u32).unwrap(),
                );
                // Release all writers into the critical section at once.
                barrier.wait().await;
                mem.store(MemoryEntry {
                    timestamp: ts,
                    summary: format!("concurrent entry {i:02}"),
                    context: None,
                })
                .await
                .unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        let file = tmp
            .path()
            .join(".rustain")
            .join("memory")
            .join(format!("{}.md", day.format("%Y-%m-%d")));
        let content = std::fs::read_to_string(&file).unwrap();

        // Exactly one H1 header (the corruption the fix prevents).
        assert_eq!(
            content.matches(&format!("# {}", day.format("%Y-%m-%d"))).count(),
            1,
            "exactly one H1 date header after {N} concurrent appends (no double header)"
        );
        // Exactly N well-formed entries, zero phantom/garbled rows: re-parse and
        // count, then confirm every summary survived intact.
        let parsed = DailyLogMemory::parse_day_file(&content, day);
        assert_eq!(parsed.len(), N, "exactly {N} well-formed entries re-parse");
        let mut got: Vec<String> = parsed.iter().map(|e| e.summary.clone()).collect();
        got.sort();
        let mut want: Vec<String> = (0..N).map(|i| format!("concurrent entry {i:02}")).collect();
        want.sort();
        assert_eq!(got, want, "every concurrent entry present exactly once, intact");
    }

    // C1 (AC10) — deterministic single-writer proof via the interior seam. Writer
    // A parks between the header-decision and `write_all` while holding the
    // `write_lock`; writer B therefore BLOCKS on the lock and cannot enter. When
    // A is released it finishes first; B then observes a non-empty file and writes
    // NO second header. Pins the exact interleave window the unfixed code got
    // wrong, with no `sleep` and no flakiness. (Reverting the `write_lock` turns
    // this window into the double-header race — that is how RED-first was verified
    // during dev; see story Debug Log.)
    #[tokio::test]
    async fn seam_serialises_two_writers() {
        use std::sync::Arc;
        use std::sync::atomic::Ordering;

        let tmp = tempfile::tempdir().unwrap();
        let mem = Arc::new(DailyLogMemory::new(tmp.path()));
        let day = NaiveDate::from_ymd_opt(2026, 6, 15).unwrap();
        mem.seam.armed.store(true, Ordering::SeqCst);

        // Writer A — will arm-disarm the seam and park holding `write_lock`.
        let a = Arc::clone(&mem);
        let a_handle = tokio::spawn(async move {
            a.store(MemoryEntry {
                timestamp: DailyLogMemory::local_at(day, NaiveTime::from_hms_opt(9, 0, 0).unwrap()),
                summary: "writer A".into(),
                context: None,
            })
            .await
            .unwrap();
        });

        // Wait until A is parked at the interior seam (holding the lock).
        mem.seam.reached.notified().await;

        // Writer B — blocks on `write_lock` until A releases.
        let b = Arc::clone(&mem);
        let b_handle = tokio::spawn(async move {
            b.store(MemoryEntry {
                timestamp: DailyLogMemory::local_at(day, NaiveTime::from_hms_opt(9, 0, 1).unwrap()),
                summary: "writer B".into(),
                context: None,
            })
            .await
            .unwrap();
        });

        // Release A; the lock guarantees A completes before B can enter.
        mem.seam.proceed.notify_one();
        a_handle.await.unwrap();
        b_handle.await.unwrap();

        let file = tmp
            .path()
            .join(".rustain")
            .join("memory")
            .join(format!("{}.md", day.format("%Y-%m-%d")));
        let content = std::fs::read_to_string(&file).unwrap();
        assert_eq!(
            content.matches(&format!("# {}", day.format("%Y-%m-%d"))).count(),
            1,
            "serialised writers emit exactly one H1 header"
        );
        let parsed = DailyLogMemory::parse_day_file(&content, day);
        assert_eq!(parsed.len(), 2, "both serialised entries present");
        let summaries: Vec<&str> = parsed.iter().map(|e| e.summary.as_str()).collect();
        assert!(summaries.contains(&"writer A"));
        assert!(summaries.contains(&"writer B"));
    }

    // C2 (AC2) — `prepare_detach()` drains an in-flight append before the swap.
    // Writer A is parked mid-write (holding `write_lock`); a concurrent
    // `prepare_detach()` (the warm-swap drain) MUST block until A flushes, then a
    // freshly-composed adapter over the SAME day-files sees A's entry. Proves the
    // drain seam closes the lost-write window. RED on the unfixed default
    // `prepare_detach` (no-op → returns before A flushes → a new adapter that
    // lazy-loaded in the gap would miss the entry).
    #[tokio::test]
    async fn prepare_detach_drains_in_flight_append() {
        use std::sync::Arc;
        use std::sync::atomic::Ordering;

        let tmp = tempfile::tempdir().unwrap();
        let mem_a = Arc::new(DailyLogMemory::new(tmp.path()));
        // Use today's timestamp: the freshly-composed adapter's `recent()` loads
        // the current + previous day, so the drained entry must land in today's
        // file to be visible across the swap.
        mem_a.seam.armed.store(true, Ordering::SeqCst);

        let a = Arc::clone(&mem_a);
        let a_handle = tokio::spawn(async move {
            a.store(MemoryEntry {
                timestamp: Local::now(),
                summary: "straddling write".into(),
                context: None,
            })
            .await
            .unwrap();
        });

        // A is parked mid-write (header decided, not yet written), holding lock.
        mem_a.seam.reached.notified().await;

        // The warm-swap drain + new-adapter compose run as ONE causal chain: the
        // new adapter loads disk ONLY after `prepare_detach()` returns. Under the
        // drain fix, `prepare_detach()` cannot return until A has flushed and
        // released `write_lock`, so the freshly-composed adapter (sharing the same
        // day-files) loads disk AFTER A's flush and sees the entry. (On the
        // unfixed no-op default, `prepare_detach` returns before A flushes and a
        // new adapter that lazy-loaded in that gap would miss it — the lost write.)
        let root = tmp.path().to_path_buf();
        let mem_a2 = Arc::clone(&mem_a);
        let drained_then_loaded = tokio::spawn(async move {
            mem_a2.prepare_detach().await.unwrap();
            let mem_b = DailyLogMemory::new(&root);
            mem_b.initialize().await.unwrap();
            mem_b.recent(10).await.unwrap()
        });

        // Release A so the parked write completes and the drain can finish.
        mem_a.seam.proceed.notify_one();
        let recent = drained_then_loaded.await.unwrap();
        a_handle.await.unwrap();

        assert!(
            recent.iter().any(|e| e.summary == "straddling write"),
            "the in-flight write survives the swap (visible on the live adapter)"
        );
    }
}
